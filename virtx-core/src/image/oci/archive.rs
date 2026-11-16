//! Moving an image between the server's registry and a file on this machine: an OCI image
//! layout in a tar archive, `oci-layout` and `index.json` and each blob at
//! `blobs/algorithm/hex`.
//!
//! Each streams its log, a line a blob, then what it moved. Neither holds a blob in memory: a
//! download writes each as it arrives, checking its digest, and an upload reads each from where
//! the archive keeps it.

use std::{
    collections::{HashMap, HashSet},
    io::SeekFrom,
    path::{Path, PathBuf},
};

use bytes::Bytes;
use futures_util::Stream;
use http_body_util::{BodyExt, StreamBody};
use hyper::body::Frame;
use serde::Deserialize;
use serde_json::{Map, json};
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufWriter},
};

use super::{
    api::Registry,
    manifest::{Child, Descriptor, Digester, Manifest, OCI_INDEX, REF_NAME, blob_path},
    reference::{Reference, Target},
};
use crate::{
    Error,
    image::{Event, Progress},
    image::{ImageDownloadOutput, ImageUploadOutput},
    transport::{Body, TransportClient},
};

/// The annotation containerd and Docker name a manifest in an archive by, as a full reference.
const IMAGE_NAME: &str = "io.containerd.image.name";

/// The most of an archive's `index.json` or a manifest that is read into memory.
const MAX_MANIFEST: u64 = 16 << 20;

/// How much of a blob one read of an archive takes.
const CHUNK: usize = 64 << 10;

/// What a manifest names, in the order it is moved: each blob and each manifest after
/// everything it names, so a registry has all of an image before its manifest.
enum Step {
    Blob(Descriptor),
    Manifest(Manifest),
}

/// The `n`th line of a log.
fn line(n: u64, line: String) -> Progress<String> {
    Progress {
        progress: n,
        total: None,
        content: Some(line),
    }
}

/// Write the image `reference` names to an archive at `path`, replacing what is there once the
/// whole image is written.
pub(crate) fn download(
    client: &TransportClient,
    reference: String,
    path: PathBuf,
) -> impl Stream<Item = Result<Event<ImageDownloadOutput, String>, Error>> + Send + '_ {
    async_stream::try_stream! {
        let reference = Reference::parse(&reference)?;
        let registry = Registry::new(client);
        let top = registry.manifest(&reference.name, reference.target()).await?;
        let mut steps = Vec::new();
        plan(
            &mut |digest| {
                let registry = &registry;
                let name = &reference.name;
                async move { registry.manifest(name, &digest).await }
            },
            &top,
            &mut steps,
            &mut HashSet::new(),
        )
        .await?;

        let mut archive = Writer::create(&path).await?;
        archive.file("oci-layout", br#"{"imageLayoutVersion":"1.0.0"}"#).await?;
        let mut descriptor = top.descriptor();
        if let Target::Tag(tag) = &reference.target {
            let mut annotations = Map::new();
            annotations.insert(REF_NAME.into(), tag.clone().into());
            annotations.insert(IMAGE_NAME.into(), reference.to_string().into());
            descriptor.annotations = Some(annotations);
        }
        let index = json!({"schemaVersion": 2, "mediaType": OCI_INDEX, "manifests": [descriptor]});
        archive.file("index.json", &serde_json::to_vec(&index).expect("JSON")).await?;
        let mut n = 0;
        for step in steps {
            match step {
                Step::Blob(descriptor) => {
                    n += 1;
                    let Descriptor { digest, size, .. } = &descriptor;
                    yield Event::Progress(line(n, format!("downloading {digest} ({size} bytes)")));
                    let body = registry.blob(&reference.name, digest).await?;
                    archive.blob(digest, *size, body).await?;
                }
                Step::Manifest(manifest) => {
                    archive.file(&blob_path(&manifest.digest), &manifest.bytes).await?;
                }
            }
        }
        archive.finish().await?;
        yield Event::Output(ImageDownloadOutput { digest: top.digest });
    }
}

/// Upload the image in the archive at `path`, and point the tag `reference` at it: the
/// archive's only image, or the one it names by that tag.
pub(crate) fn upload(
    client: &TransportClient,
    path: PathBuf,
    reference: String,
) -> impl Stream<Item = Result<Event<ImageUploadOutput, String>, Error>> + Send + '_ {
    async_stream::try_stream! {
        let (name, tag) = Reference::parse_tag(&reference)?;
        let archive = Archive::open(path).await?;
        let descriptor = archive.select(&tag, &reference).await?;
        let top = archive.manifest(&descriptor).await?;
        let mut steps = Vec::new();
        plan(
            &mut |digest| {
                let archive = &archive;
                async move { archive.manifest_by_digest(&digest).await }
            },
            &top,
            &mut steps,
            &mut HashSet::new(),
        )
        .await?;

        let registry = Registry::new(client);
        let mut n = 0;
        for step in steps {
            match step {
                Step::Blob(descriptor) => {
                    n += 1;
                    let Descriptor { digest, size, .. } = &descriptor;
                    if registry.has_blob(&name, digest).await? {
                        yield Event::Progress(line(n, format!("{digest} is there already")));
                        continue;
                    }
                    yield Event::Progress(line(n, format!("uploading {digest} ({size} bytes)")));
                    let body = archive.blob(digest, *size)?;
                    let upload = registry.start_upload(&name).await?;
                    registry.finish_upload(upload, digest, *size, body).await?;
                }
                Step::Manifest(manifest) if manifest.digest == top.digest => {}
                Step::Manifest(manifest) => {
                    registry.put_manifest(&name, &manifest.digest, &manifest).await?;
                }
            }
        }
        registry.put_manifest(&name, &tag, &top).await?;
        yield Event::Output(ImageUploadOutput { digest: top.digest });
    }
}

/// Add to `steps` what `manifest` names, each manifest got by `fetch` from its digest, then
/// `manifest` itself. Whatever is in `seen` is skipped.
async fn plan<F, Fut>(
    fetch: &mut F,
    manifest: &Manifest,
    steps: &mut Vec<Step>,
    seen: &mut HashSet<String>,
) -> Result<(), Error>
where
    F: FnMut(String) -> Fut + Send,
    Fut: Future<Output = Result<Manifest, Error>> + Send,
{
    if !seen.insert(manifest.digest.clone()) {
        return Ok(());
    }
    for child in manifest.children()? {
        match child {
            Child::Manifest(descriptor) => {
                let child = fetch(descriptor.digest).await?;
                Box::pin(plan(fetch, &child, steps, seen)).await?;
            }
            Child::Blob(descriptor) => {
                if seen.insert(descriptor.digest.clone()) {
                    steps.push(Step::Blob(descriptor));
                }
            }
        }
    }
    steps.push(Step::Manifest(manifest.clone()));
    Ok(())
}

fn file_error(path: &Path) -> impl Fn(std::io::Error) -> Error + '_ {
    move |e| Error::Client(format!("{}: {e}", path.display()))
}

/// An archive being written, beside where it goes; removed unless it is finished.
struct Writer {
    file: BufWriter<File>,
    partial: PathBuf,
    path: PathBuf,
    finished: bool,
}

impl Writer {
    async fn create(path: &Path) -> Result<Writer, Error> {
        let mut name = path
            .file_name()
            .ok_or_else(|| Error::Client(format!("{} names no file", path.display())))?
            .to_owned();
        name.push(".partial");
        let partial = path.with_file_name(name);
        let file = File::create(&partial).await.map_err(file_error(&partial))?;
        Ok(Writer {
            file: BufWriter::new(file),
            partial,
            path: path.to_owned(),
            finished: false,
        })
    }

    async fn write(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.file
            .write_all(bytes)
            .await
            .map_err(file_error(&self.partial))
    }

    async fn header(&mut self, name: &str, size: u64) -> Result<(), Error> {
        let mut header = tar::Header::new_ustar();
        header
            .set_path(name)
            .map_err(|e| Error::Client(format!("{name}: {e}")))?;
        header.set_size(size);
        header.set_mode(0o644);
        header.set_mtime(0);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_cksum();
        self.write(header.as_bytes()).await
    }

    /// Fill the block an entry of `size` bytes ends in.
    async fn pad(&mut self, size: u64) -> Result<(), Error> {
        let over = (size % 512) as usize;
        if over == 0 {
            return Ok(());
        }
        self.write(&[0; 512][over..]).await
    }

    async fn file(&mut self, name: &str, bytes: &[u8]) -> Result<(), Error> {
        self.header(name, bytes.len() as u64).await?;
        self.write(bytes).await?;
        self.pad(bytes.len() as u64).await
    }

    /// The blob `digest`, `size` bytes read from `body`, checked against both.
    async fn blob(&mut self, digest: &str, size: u64, mut body: Body) -> Result<(), Error> {
        self.header(&blob_path(digest), size).await?;
        let mut digester = Digester::new(digest)?;
        let mut written = 0;
        while let Some(frame) = body.frame().await {
            let frame =
                frame.map_err(|e| Error::Transport(format!("reading the blob {digest}: {e}")))?;
            let Ok(data) = frame.into_data() else {
                continue;
            };
            written += data.len() as u64;
            if written > size {
                break;
            }
            digester.update(&data);
            self.write(&data).await?;
        }
        if written != size {
            return Err(Error::Client(format!(
                "the blob {digest} is {size} bytes, but more or fewer came"
            )));
        }
        digester.check()?;
        self.pad(size).await
    }

    /// End the archive, and put it where it goes.
    async fn finish(mut self) -> Result<(), Error> {
        self.write(&[0; 1024]).await?;
        self.file.flush().await.map_err(file_error(&self.partial))?;
        tokio::fs::rename(&self.partial, &self.path)
            .await
            .map_err(file_error(&self.path))?;
        self.finished = true;
        Ok(())
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        if !self.finished {
            let _ = std::fs::remove_file(&self.partial);
        }
    }
}

/// An archive being read: where each of its files starts, and how long it is.
struct Archive {
    path: PathBuf,
    entries: HashMap<String, (u64, u64)>,
}

impl Archive {
    /// The archive at `path`, its headers read through once.
    async fn open(path: PathBuf) -> Result<Archive, Error> {
        tokio::task::spawn_blocking(move || {
            let entries = Archive::entries(&path).map_err(file_error(&path))?;
            Ok(Archive { path, entries })
        })
        .await
        .map_err(|e| Error::Client(format!("reading the archive: {e}")))?
    }

    /// Where each regular file in the archive at `path` starts, and how long it is, by its
    /// name without a leading `./`.
    fn entries(path: &Path) -> std::io::Result<HashMap<String, (u64, u64)>> {
        let mut archive = tar::Archive::new(std::fs::File::open(path)?);
        let mut entries = HashMap::new();
        for entry in archive.entries_with_seek()? {
            let entry = entry?;
            if !entry.header().entry_type().is_file() {
                continue;
            }
            let name = entry
                .path()?
                .to_string_lossy()
                .trim_start_matches("./")
                .to_owned();
            entries.insert(name, (entry.raw_file_position(), entry.size()));
        }
        Ok(entries)
    }

    fn entry(&self, name: &str) -> Result<(u64, u64), Error> {
        self.entries
            .get(name)
            .copied()
            .ok_or_else(|| Error::Client(format!("{} has no {name}", self.path.display())))
    }

    /// The whole of the file `name`, which is small.
    async fn read(&self, name: &str) -> Result<Bytes, Error> {
        let (offset, size) = self.entry(name)?;
        if size > MAX_MANIFEST {
            return Err(Error::Client(format!(
                "{name} in {} is {size} bytes, too many for a manifest",
                self.path.display()
            )));
        }
        let failed = file_error(&self.path);
        let mut file = File::open(&self.path).await.map_err(&failed)?;
        file.seek(SeekFrom::Start(offset)).await.map_err(&failed)?;
        let mut bytes = vec![0; size as usize];
        file.read_exact(&mut bytes).await.map_err(&failed)?;
        Ok(bytes.into())
    }

    /// The manifest `index.json` names by `tag` or `reference`, or its only one.
    async fn select(&self, tag: &str, reference: &str) -> Result<Descriptor, Error> {
        #[derive(Deserialize)]
        struct Index {
            manifests: Vec<Descriptor>,
        }
        let index: Index = serde_json::from_slice(&self.read("index.json").await?)
            .map_err(|e| Error::Client(format!("{}: index.json: {e}", self.path.display())))?;
        let names = |descriptor: &Descriptor| -> Vec<String> {
            let annotations = descriptor.annotations.as_ref();
            [REF_NAME, IMAGE_NAME]
                .into_iter()
                .filter_map(|key| annotations?.get(key)?.as_str().map(str::to_owned))
                .collect()
        };
        let mut manifests = index.manifests;
        if manifests.len() == 1 {
            return Ok(manifests.remove(0));
        }
        let named = manifests.iter().position(|descriptor| {
            names(descriptor)
                .iter()
                .any(|name| name == tag || name == reference)
        });
        match named {
            Some(i) => Ok(manifests.remove(i)),
            None => {
                let all: Vec<String> = manifests.iter().flat_map(names).collect();
                Err(Error::Client(format!(
                    "{} holds {} images, none named {reference:?}: {}",
                    self.path.display(),
                    manifests.len(),
                    all.join(", ")
                )))
            }
        }
    }

    async fn manifest(&self, descriptor: &Descriptor) -> Result<Manifest, Error> {
        let digest = &descriptor.digest;
        if !crate::image::is_digest(digest) {
            return Err(Error::Client(format!("{digest:?} is not a digest")));
        }
        let bytes = self.read(&blob_path(digest)).await?;
        Manifest::new(bytes, Some(descriptor.media_type.clone()), Some(digest))
    }

    async fn manifest_by_digest(&self, digest: &str) -> Result<Manifest, Error> {
        let bytes = self.read(&blob_path(digest)).await?;
        Manifest::new(bytes, None, Some(digest))
    }

    /// The blob `digest`, of `size` bytes, as it is read from the archive.
    fn blob(&self, digest: &str, size: u64) -> Result<Body, Error> {
        let name = blob_path(digest);
        let (offset, stored) = self.entry(&name)?;
        if stored != size {
            return Err(Error::Client(format!(
                "{name} in {} is {stored} bytes, not the {size} its manifest says",
                self.path.display()
            )));
        }
        let path = self.path.clone();
        let chunks = async_stream::try_stream! {
            let mut file = File::open(&path).await?;
            file.seek(SeekFrom::Start(offset)).await?;
            let mut left = size;
            let mut buffer = vec![0; CHUNK];
            while left > 0 {
                let want = buffer.len().min(left as usize);
                let read = file.read(&mut buffer[..want]).await?;
                if read == 0 {
                    Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof))?;
                }
                left -= read as u64;
                yield Frame::data(Bytes::copy_from_slice(&buffer[..read]));
            }
        };
        Ok(StreamBody::new(chunks).boxed())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;
    use crate::{image::oci::manifest::sha256, transport::full};

    const IMAGE: &str = "application/vnd.oci.image.manifest.v1+json";

    /// A directory of the test's own, removed when it drops.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Scratch {
            let dir = std::env::temp_dir().join(format!("virtx-archive-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }

        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn descriptor(media_type: &str, bytes: &[u8]) -> Value {
        json!({"mediaType": media_type, "digest": sha256(bytes), "size": bytes.len()})
    }

    /// An image of the config `{}` and the one layer `layer`: its manifest, and its blobs.
    fn image(layer: &'static [u8]) -> (Manifest, [&'static [u8]; 2]) {
        let config: &[u8] = b"{}";
        let bytes = serde_json::to_vec(&json!({
            "schemaVersion": 2,
            "mediaType": IMAGE,
            "config": descriptor("application/vnd.oci.image.config.v1+json", config),
            "layers": [descriptor("application/vnd.oci.image.layer.v1.tar", layer)],
        }))
        .unwrap();
        (
            Manifest::new(bytes.into(), None, None).unwrap(),
            [config, layer],
        )
    }

    /// `manifest`'s descriptor, named by the annotation `key` as `name`.
    fn named(manifest: &Manifest, key: &str, name: &str) -> Descriptor {
        let mut annotations = Map::new();
        annotations.insert(key.into(), name.into());
        Descriptor {
            annotations: Some(annotations),
            ..manifest.descriptor()
        }
    }

    /// An archive at `path` whose `index.json` names `index`, holding `manifests` and `blobs`.
    async fn write(path: &Path, index: &[Descriptor], manifests: &[&Manifest], blobs: &[&[u8]]) {
        let mut archive = Writer::create(path).await.unwrap();
        archive.file("oci-layout", b"{}").await.unwrap();
        let index = json!({"schemaVersion": 2, "mediaType": OCI_INDEX, "manifests": index});
        let index = serde_json::to_vec(&index).unwrap();
        archive.file("index.json", &index).await.unwrap();
        for manifest in manifests {
            let name = blob_path(&manifest.digest);
            archive.file(&name, &manifest.bytes).await.unwrap();
        }
        for blob in blobs {
            let (digest, size) = (sha256(blob), blob.len() as u64);
            let body = full(Bytes::copy_from_slice(blob));
            archive.blob(&digest, size, body).await.unwrap();
        }
        archive.finish().await.unwrap();
    }

    #[tokio::test]
    async fn an_archive_written_reads_back_its_image() {
        let scratch = Scratch::new();
        let path = scratch.path("image.tar");
        let (manifest, blobs) = image(b"layer");
        write(&path, &[manifest.descriptor()], &[&manifest], &blobs).await;

        let archive = Archive::open(path.clone()).await.unwrap();
        assert_eq!(&archive.read("oci-layout").await.unwrap()[..], b"{}");
        let selected = archive.select("any", "any:tag").await.unwrap();
        assert_eq!(selected, manifest.descriptor());
        let read = archive.manifest(&selected).await.unwrap();
        assert_eq!((read.digest, read.bytes), (manifest.digest, manifest.bytes));
        for blob in blobs {
            let body = archive.blob(&sha256(blob), blob.len() as u64).unwrap();
            assert_eq!(&body.collect().await.unwrap().to_bytes()[..], blob);
        }
        assert!(!scratch.path("image.tar.partial").exists());
    }

    #[tokio::test]
    async fn a_blob_that_is_not_its_digest_leaves_no_archive() {
        let scratch = Scratch::new();
        let path = scratch.path("image.tar");
        let mut archive = Writer::create(&path).await.unwrap();
        let refused = archive.blob(&sha256(b"a"), 1, full("b")).await;
        assert!(matches!(refused, Err(Error::Client(_))), "{refused:?}");
        drop(archive);
        assert!(!path.exists());
        assert!(!scratch.path("image.tar.partial").exists());
    }

    #[tokio::test]
    async fn a_blob_of_more_or_fewer_bytes_than_its_size_is_refused() {
        let scratch = Scratch::new();
        let mut archive = Writer::create(&scratch.path("image.tar")).await.unwrap();
        let digest = sha256(b"ab");
        for (size, body) in [(2, "abc"), (3, "ab")] {
            let refused = archive.blob(&digest, size, full(body)).await;
            assert!(matches!(refused, Err(Error::Client(_))), "{refused:?}");
        }
    }

    #[tokio::test]
    async fn of_several_images_the_one_its_tag_or_reference_names_is_selected() {
        let scratch = Scratch::new();
        let path = scratch.path("images.tar");
        let (one, one_blobs) = image(b"one");
        let (two, two_blobs) = image(b"two");
        let index = [
            named(&one, REF_NAME, "1"),
            named(&two, IMAGE_NAME, "docker.io/library/app:2"),
        ];
        write(
            &path,
            &index,
            &[&one, &two],
            &[one_blobs[0], one_blobs[1], two_blobs[1]],
        )
        .await;

        let archive = Archive::open(path).await.unwrap();
        let by_tag = archive.select("1", "app:1").await.unwrap();
        assert_eq!(by_tag.digest, one.digest);
        let by_reference = archive
            .select("2", "docker.io/library/app:2")
            .await
            .unwrap();
        assert_eq!(by_reference.digest, two.digest);
        let Err(Error::Client(message)) = archive.select("3", "app:3").await else {
            panic!("selected an image named by neither");
        };
        assert!(
            message.contains("1") && message.contains("docker.io/library/app:2"),
            "{message}"
        );
    }

    #[tokio::test]
    async fn a_blob_not_there_or_not_the_size_its_manifest_says_is_refused() {
        let scratch = Scratch::new();
        let path = scratch.path("image.tar");
        let (manifest, blobs) = image(b"layer");
        write(&path, &[manifest.descriptor()], &[&manifest], &blobs).await;

        let archive = Archive::open(path).await.unwrap();
        let layer = sha256(blobs[1]);
        assert!(matches!(archive.blob(&layer, 99), Err(Error::Client(_))));
        let missing = sha256(b"missing");
        assert!(matches!(archive.blob(&missing, 7), Err(Error::Client(_))));
        assert!(matches!(
            archive.manifest_by_digest(&missing).await,
            Err(Error::Client(_))
        ));
    }

    #[tokio::test]
    async fn a_manifest_named_by_what_is_not_a_digest_is_refused() {
        let scratch = Scratch::new();
        let path = scratch.path("image.tar");
        let (manifest, blobs) = image(b"layer");
        write(&path, &[manifest.descriptor()], &[&manifest], &blobs).await;

        let archive = Archive::open(path).await.unwrap();
        let by_path = Descriptor {
            digest: "../../index.json".into(),
            ..manifest.descriptor()
        };
        assert!(matches!(
            archive.manifest(&by_path).await,
            Err(Error::Client(_))
        ));
    }

    #[tokio::test]
    async fn an_entry_written_under_dot_slash_is_found_without_it() {
        let scratch = Scratch::new();
        let path = scratch.path("image.tar");
        let mut builder = tar::Builder::new(std::fs::File::create(&path).unwrap());
        let mut header = tar::Header::new_ustar();
        header.set_size(2);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, "./index.json", &b"{}"[..])
            .unwrap();
        builder.finish().unwrap();
        drop(builder);

        let archive = Archive::open(path).await.unwrap();
        assert_eq!(&archive.read("index.json").await.unwrap()[..], b"{}");
    }

    #[tokio::test]
    async fn a_plan_moves_each_blob_once_and_each_manifest_after_what_it_names() {
        let (one, one_blobs) = image(b"one");
        let (two, two_blobs) = image(b"two");
        let index = serde_json::to_vec(&json!({
            "schemaVersion": 2,
            "mediaType": OCI_INDEX,
            "manifests": [one.descriptor(), two.descriptor()],
        }))
        .unwrap();
        let index = Manifest::new(index.into(), None, None).unwrap();
        let manifests: HashMap<String, Manifest> =
            [&one, &two].map(|m| (m.digest.clone(), m.clone())).into();

        let mut steps = Vec::new();
        let mut fetch = |digest: String| {
            let found = manifests.get(&digest).cloned();
            async move { found.ok_or(Error::Client(digest)) }
        };
        plan(&mut fetch, &index, &mut steps, &mut HashSet::new())
            .await
            .unwrap();
        let steps: Vec<_> = steps
            .into_iter()
            .map(|step| match step {
                Step::Blob(descriptor) => ("blob", descriptor.digest),
                Step::Manifest(manifest) => ("manifest", manifest.digest),
            })
            .collect();
        // The config both images share is moved with the first.
        assert_eq!(
            steps,
            [
                ("blob", sha256(one_blobs[0])),
                ("blob", sha256(one_blobs[1])),
                ("manifest", one.digest),
                ("blob", sha256(two_blobs[1])),
                ("manifest", two.digest),
                ("manifest", index.digest),
            ]
        );
    }
}
