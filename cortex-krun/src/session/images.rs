//! The image plane: `build_image`, `list_images` and `remove_image`, and turning whichever
//! form an `init` named its image in into a name in the cache.

use std::collections::BTreeMap;

use cortex::image::{ImageEntry, ImageSource};
use cortex::protocol::{
    BuildImageCall, BuildImageResp, Error, ListImagesResp, RemoveImageCall, RemoveImageResp,
};
use cortex_krun::image::{
    Cache, Digest, Error as ImageError, Image, Reference, remove, tag, untag,
};

use super::{build, coded, pulled};

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
pub async fn resolve(image: &ImageSource) -> anyhow::Result<(String, Option<Built>)> {
    let (name, built) = match image {
        ImageSource::Recipe { recipe } if recipe.steps.is_empty() => (recipe.base.clone(), None),
        ImageSource::Recipe { recipe } => {
            let (name, fresh) = build::build(recipe).await?;
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
pub struct Built(String);

impl Drop for Built {
    fn drop(&mut self) {
        if let Err(e) = discard(&self.0) {
            eprintln!("{}: removing {}: {e:#}", env!("CARGO_BIN_NAME"), self.0);
        }
    }
}

fn discard(name: &str) -> anyhow::Result<()> {
    let index = Cache::new().image_index()?;
    let Some(image) = index.get(name) else {
        return Ok(());
    };
    let claimed = index
        .iter()
        .any(|(other, it)| it.manifest == image.manifest && !build::internal(other));
    if claimed {
        untag(name, None)?;
    } else {
        remove(name, None)?;
    }
    Ok(())
}

pub async fn build_image(call: BuildImageCall) -> anyhow::Result<BuildImageResp> {
    let (built, _) = resolve(&call.recipe.into()).await?;

    let name = canonical(&built)?;
    let mut index = Cache::new().image_index()?;
    let reference = match call.reference {
        Some(reference) => reference,
        None => picked_name(&index)?,
    };
    let image = index
        .remove(&name)
        .ok_or_else(|| anyhow::anyhow!("{name} is not in the cache, having just been built"))?;

    tag(built.as_str(), reference.as_str(), None).map_err(image_error)?;

    Ok(BuildImageResp {
        reference,
        digest: image.manifest.to_string(),
    })
}

pub fn list_images() -> anyhow::Result<ListImagesResp> {
    let mut by_manifest: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, image) in Cache::new().image_index()? {
        let refs = by_manifest.entry(image.manifest.to_string()).or_default();
        if !build::internal(&name) {
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

pub fn remove_image(call: RemoveImageCall) -> anyhow::Result<RemoveImageResp> {
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
    remove(name.as_str(), None).map_err(image_error)?;
    Ok(RemoveImageResp {})
}

fn by_digest(digest: &str) -> anyhow::Result<String> {
    let wanted = Digest::parse(digest).map_err(|e| {
        coded(
            Error::INVALID_PARAMS,
            anyhow::anyhow!("{digest} is not a digest: {e}"),
        )
    })?;

    Cache::new()
        .image_index()?
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
    let reference: Reference = name
        .parse()
        .map_err(|e| coded(Error::INVALID_PARAMS, anyhow::anyhow!("{name}: {e}")))?;
    Ok(reference.to_string())
}

fn image_error(e: ImageError) -> anyhow::Error {
    let code = match &e {
        ImageError::Unknown(_) | ImageError::NotPulled(_) => Error::UNKNOWN_IMAGE,
        ImageError::Reference(_) => Error::INVALID_PARAMS,
        _ => Error::INTERNAL_ERROR,
    };
    coded(code, anyhow::Error::new(e))
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
            manifest: Digest::of(""),
            layers: Vec::new(),
            config: Default::default(),
            platform: String::new(),
        }
    }
}
