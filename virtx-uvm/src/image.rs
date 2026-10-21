use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use clap::Subcommand;
use virtx::image::{ImageEntry, ImageSource, Recipe};
use virtx::protocol::{
    BuildImageCall, BuildImageResp, Error, ListImagesResp, RemoveImageCall, RemoveImageResp,
};
use virtx_uvm::cache::{Cache, GRACE};
use virtx_uvm::filesystem::Digest;
use virtx_uvm::image::Image;
use virtx_uvm::registry::{Reference, Registry};
use virtx_uvm::rootfs::Rootfs;

use crate::image_build;
use crate::session::coded;

#[derive(Subcommand)]
pub(crate) enum Command {
    /// List the images in the cache.
    #[command(visible_alias = "list")]
    Ls,
    /// Pull an image from its registry into the cache.
    Pull { reference: String },
    /// Build an image from a Dockerfile.
    Build {
        /// The Dockerfile to read.
        #[arg(short, long, default_value = "Dockerfile")]
        file: PathBuf,
        /// Also give the built image this name.
        #[arg(short, long)]
        tag: Option<String>,
    },
    /// Give an image in the cache another name.
    Tag { from: String, to: String },
    /// Take a name away, leaving the image it pointed at.
    Untag { reference: String },
    /// Remove an image, every name it has and the files only it uses.
    #[command(visible_alias = "remove")]
    Rm {
        #[arg(required = true)]
        references: Vec<String>,
    },
    /// Delete every file in the cache that no image names.
    Prune {
        /// Also delete files written in the last hour, which a pull may not have named yet.
        #[arg(long)]
        all: bool,
    },
}

pub(crate) async fn run(command: Command) -> anyhow::Result<()> {
    let cache = Cache::default();
    match command {
        Command::Ls => {
            let mut listed = Vec::new();
            for (reference, manifest) in cache.refs()? {
                let Some(image) = cache.get_image(&manifest)? else {
                    continue;
                };
                // What the layers weigh on this host, which a layer two images share is
                // counted in for both: the honest number is what `rm` says it reclaimed.
                let size = image
                    .layers
                    .iter()
                    .filter_map(|layer| std::fs::metadata(cache.layer_path(layer)).ok())
                    .map(|meta| meta.len())
                    .sum::<u64>();
                let provisioned = cache
                    .rootfs_path(&image.layers)
                    .is_some_and(|at| Rootfs::open(&at).is_some());
                listed.push((reference.to_string(), manifest, size, provisioned));
            }
            let width = listed
                .iter()
                .map(|(reference, ..)| reference.len())
                .max()
                .unwrap_or(0)
                .max("REFERENCE".len());
            println!(
                "{:width$}  {:12}  {:>9}  PROVISIONED",
                "REFERENCE", "MANIFEST", "SIZE"
            );
            for (reference, manifest, size, provisioned) in listed {
                let manifest = manifest.digest();
                println!(
                    "{:width$}  {:12}  {:>9}  {}",
                    reference,
                    &manifest[..manifest.len().min(12)],
                    bytes(size),
                    if provisioned { "yes" } else { "no" },
                );
            }
        }
        Command::Pull { reference } => {
            let pulled = Registry::default().pull(reference.as_str(), &cache).await?;
            println!("{reference}: {}", pulled.manifest);
        }
        Command::Build { file, tag } => {
            let text = std::fs::read_to_string(&file)
                .map_err(|e| anyhow::anyhow!("reading {}: {e}", file.display()))?;
            let recipe = Recipe::from_dockerfile(&text)
                .map_err(|e| anyhow::anyhow!("{}: {e}", file.display()))?;
            let built = image_build::build(&recipe).await;
            // The build's disk was under this process's directory, and a command line is not
            // a session that would remove it on the way out. Only if empty: a session's is
            // not this command's to touch.
            let _ = std::fs::remove_dir(
                crate::session::sessions().join(std::process::id().to_string()),
            );
            let (name, _) = built?;
            let image = cache.find(&parsed(&name)?)?.ok_or_else(|| {
                anyhow::anyhow!("{name} is not in the cache, having just been built")
            })?;
            if let Some(tag) = &tag {
                cache.set_ref(&parsed(tag)?, &image.manifest)?;
            }
            println!("{}: {}", tag.unwrap_or(name), image.manifest);
        }
        Command::Tag { from, to } => {
            let image = cache
                .find(&parsed(&from)?)?
                .ok_or_else(|| anyhow::anyhow!("{from} is not in the cache"))?;
            cache.set_ref(&parsed(&to)?, &image.manifest)?;
            println!("{to}: {}", image.manifest);
        }
        Command::Untag { reference } => {
            let manifest = cache
                .remove_ref(&parsed(&reference)?)?
                .ok_or_else(|| anyhow::anyhow!("{reference} names nothing here"))?;
            println!("untagged {reference} ({manifest})");
        }
        Command::Rm { references } => {
            for reference in references {
                let removed = cache
                    .remove(&parsed(&reference)?)?
                    .ok_or_else(|| anyhow::anyhow!("{reference} is not in the cache"))?;
                for name in &removed.untagged {
                    println!("untagged {name}");
                }
                println!("removed {reference}, reclaiming {}", bytes(removed.bytes));
            }
        }
        Command::Prune { all } => {
            let grace = if all { Duration::ZERO } else { GRACE };
            let pruned = cache.prune(grace)?;
            println!(
                "deleted {} files, reclaiming {}",
                pruned.files.len(),
                bytes(pruned.bytes)
            );
            if pruned.spared > 0 {
                println!(
                    "spared {} written in the last hour; --all deletes them too",
                    pruned.spared
                );
            }
        }
    }
    Ok(())
}

/// Make sure `image` is in the cache, pulling it if it is not, and that its root filesystem
/// is made.
///
/// The cache is asked first, and only an image it does not have is worth reaching a registry
/// over. The root filesystem is made here rather than left to the boot so that a stack that
/// will not merge is a failure this end can report before anything is spawned -- the boot
/// then finds it made.
pub(crate) async fn pulled(image: &str) -> anyhow::Result<()> {
    let reference: Reference = image
        .parse()
        .map_err(|e| coded(Error::UNKNOWN_IMAGE, anyhow::anyhow!("{image}: {e}")))?;
    let cache = Cache::default();
    let found = match cache.find(&reference)? {
        Some(found) => found,
        None => Registry::default()
            .pull(reference, &cache)
            .await
            .map_err(|e| coded(Error::UNKNOWN_IMAGE, anyhow::Error::new(e)))?,
    };

    let at = cache.rootfs_path(&found.layers).ok_or_else(|| {
        coded(
            Error::UNSUPPORTED_IMAGE,
            anyhow::anyhow!("{image} has no layers to boot"),
        )
    })?;
    if Rootfs::open(&at).is_none() {
        let layers: Vec<_> = found.layers.iter().map(|l| cache.layer_path(l)).collect();
        Rootfs::create(&layers, &at)
            .map_err(|e| coded(Error::BOOT_FAILED, anyhow::Error::new(e)))?;
    }
    Ok(())
}

/// `text` as a reference, refused as the caller's to change when it is not one.
fn parsed(text: &str) -> anyhow::Result<Reference> {
    text.parse().map_err(|e| {
        coded(
            Error::INVALID_PARAMS,
            anyhow::anyhow!("{text} is not an OCI reference: {e}"),
        )
    })
}

fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

const ADJECTIVES: &[&str] = &[
    "agile", "amber", "arctic", "bold", "brave", "breezy", "bright", "calm", "cheerful", "clever",
    "cosmic", "cozy", "crimson", "crisp", "daring", "eager", "fancy", "frosty", "gentle", "golden",
    "happy", "humble", "jolly", "keen", "kind", "lively", "lucky", "mellow", "merry", "misty",
    "nimble", "noble", "proud", "quiet", "rapid", "rusty", "serene", "shy", "silent", "snowy",
    "sunny", "swift", "tidy", "vivid", "wild", "witty", "young", "zesty",
];
const NOUNS: &[&str] = &[
    "acorn", "aurora", "badger", "beacon", "bison", "canyon", "cedar", "comet", "coral", "dolphin",
    "ember", "falcon", "fern", "finch", "fox", "glacier", "harbor", "heron", "island", "koala",
    "lantern", "lynx", "maple", "meadow", "moose", "nebula", "orchid", "otter", "owl", "panda",
    "pebble", "penguin", "pine", "quartz", "raven", "reef", "river", "robin", "sparrow", "summit",
    "tiger", "tulip", "valley", "walrus", "willow", "wolf", "yak", "zebra",
];

/// A name no image here has yet.
fn picked_name(index: &BTreeMap<String, Image>) -> anyhow::Result<String> {
    use std::hash::{BuildHasher, Hasher};

    for attempt in 0.. {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u32(attempt);
        let random = hasher.finish();

        let adjective = ADJECTIVES[(random % ADJECTIVES.len() as u64) as usize];
        let noun = NOUNS[((random >> 32) % NOUNS.len() as u64) as usize];
        let name = if attempt < 16 {
            format!("{adjective}-{noun}:latest")
        } else {
            format!("{adjective}-{noun}-{}:latest", random % 10_000)
        };
        if !index.contains_key(&canonical(&name)?) {
            return Ok(name);
        }
    }
    unreachable!()
}

/// The name in the cache a session on `image` boots, pulling or building it on the way, and
/// the image itself when this call is what built it.
pub(crate) async fn resolve(image: &ImageSource) -> anyhow::Result<(String, Option<Built>)> {
    let (name, built) = match image {
        ImageSource::Recipe { recipe } if recipe.steps.is_empty() => (recipe.base.clone(), None),
        ImageSource::Recipe { recipe } => {
            let (name, fresh) = image_build::build(recipe).await?;
            let built = fresh.then(|| Built(name.clone()));
            (name, built)
        }
        ImageSource::Ref { reference } => (reference.clone(), None),
        ImageSource::Digest { digest } => (by_digest(digest)?, None),
    };
    pulled(&name).await?;
    Ok((name, built))
}

/// An image a session's `init` built, which goes when the session does.
///
/// Only its own name goes if a client has since given the image one of theirs.
pub(crate) struct Built(String);

impl Drop for Built {
    fn drop(&mut self) {
        if let Err(e) = discard(&self.0) {
            eprintln!("{}: removing {}: {e:#}", env!("CARGO_BIN_NAME"), self.0);
        }
    }
}

fn discard(name: &str) -> anyhow::Result<()> {
    let cache = Cache::default();
    let index = index(&cache)?;
    let Some(image) = index.get(name) else {
        return Ok(());
    };
    let claimed = index
        .iter()
        .any(|(other, it)| it.manifest == image.manifest && !image_build::internal(other));
    let reference = parsed(name)?;
    if claimed {
        cache.remove_ref(&reference)?;
    } else {
        cache.remove(&reference)?;
    }
    Ok(())
}

/// Every name in the cache against the image it names, keyed by the name's canonical
/// spelling -- what the old index file was, read off the cache's own references.
fn index(cache: &Cache) -> anyhow::Result<BTreeMap<String, Image>> {
    let mut index = BTreeMap::new();
    for (reference, manifest) in cache.refs()? {
        if let Some(image) = cache.get_image(&manifest)? {
            index.insert(reference.to_string(), image);
        }
    }
    Ok(index)
}

pub(crate) async fn build_image(call: BuildImageCall) -> anyhow::Result<BuildImageResp> {
    // Held past `tag`: dropped earlier, it discards the build before the ref claims it.
    let (built, _guard) = resolve(&call.recipe.into()).await?;

    let name = canonical(&built)?;
    let cache = Cache::default();
    let mut index = index(&cache)?;
    let reference = match call.reference {
        Some(reference) => reference,
        None => picked_name(&index)?,
    };
    let image = index
        .remove(&name)
        .ok_or_else(|| anyhow::anyhow!("{name} is not in the cache, having just been built"))?;

    cache.set_ref(&parsed(&reference)?, &image.manifest)?;

    Ok(BuildImageResp {
        reference,
        digest: image.manifest.to_string(),
    })
}

pub(crate) fn list_images() -> anyhow::Result<ListImagesResp> {
    let mut by_manifest: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, image) in index(&Cache::default())? {
        let refs = by_manifest.entry(image.manifest.to_string()).or_default();
        if !image_build::internal(&name) {
            refs.push(name);
        }
    }

    Ok(ListImagesResp {
        images: by_manifest
            .into_iter()
            .map(|(digest, refs)| ImageEntry { digest, refs })
            .collect(),
    })
}

pub(crate) fn remove_image(call: RemoveImageCall) -> anyhow::Result<RemoveImageResp> {
    let name = match &call.image {
        ImageSource::Ref { reference } => reference.clone(),
        ImageSource::Digest { digest } => by_digest(digest)?,
        ImageSource::Recipe { .. } => {
            return Err(coded(
                Error::INVALID_PARAMS,
                anyhow::anyhow!("an image is removed by its ref or its digest, not by a recipe"),
            ));
        }
    };
    Cache::default().remove(&parsed(&name)?)?.ok_or_else(|| {
        coded(
            Error::UNKNOWN_IMAGE,
            anyhow::anyhow!("{name} is not in the cache"),
        )
    })?;
    Ok(RemoveImageResp {})
}

fn by_digest(digest: &str) -> anyhow::Result<String> {
    let wanted: Digest = digest.parse().map_err(|e| {
        coded(
            Error::INVALID_PARAMS,
            anyhow::anyhow!("{digest} is not a digest: {e}"),
        )
    })?;

    index(&Cache::default())?
        .into_iter()
        .find(|(_, image)| image.manifest == wanted)
        .map(|(name, _)| name)
        .ok_or_else(|| {
            coded(
                Error::UNKNOWN_IMAGE,
                anyhow::anyhow!("no image here is {digest}"),
            )
        })
}

fn canonical(name: &str) -> anyhow::Result<String> {
    Ok(parsed(name)?.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_picked_name_is_a_reference_nothing_here_has() {
        let mut index = BTreeMap::new();
        for _ in 0..200 {
            let name = picked_name(&index).unwrap();
            assert!(name.ends_with(":latest"), "{name}");
            let key = canonical(&name).unwrap();
            assert!(!index.contains_key(&key), "{name} picked twice");
            index.insert(key, test_image());
        }
    }

    fn test_image() -> Image {
        Image {
            manifest: format!("sha256:{}", "0".repeat(64)).parse().unwrap(),
            layers: Vec::new(),
            config: Default::default(),
            platform: String::new(),
        }
    }
}
