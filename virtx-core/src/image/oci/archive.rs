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
    image::{ImageDownloadOutput, ImageUploadOutput},
    method::{Event, Progress},
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
    move |e| Error::File(format!("{}: {e}", path.display()))
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
            .ok_or_else(|| Error::File(format!("{} names no file", path.display())))?
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
            .map_err(|e| Error::File(format!("{name}: {e}")))?;
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
            return Err(Error::Registry(format!(
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
        .map_err(|e| Error::File(format!("reading the archive: {e}")))?
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
            .ok_or_else(|| Error::File(format!("{} has no {name}", self.path.display())))
    }

    /// The whole of the file `name`, which is small.
    async fn read(&self, name: &str) -> Result<Bytes, Error> {
        let (offset, size) = self.entry(name)?;
        if size > MAX_MANIFEST {
            return Err(Error::File(format!(
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
            .map_err(|e| Error::File(format!("{}: index.json: {e}", self.path.display())))?;
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
                Err(Error::File(format!(
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
            return Err(Error::File(format!("{digest:?} is not a digest")));
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
            return Err(Error::File(format!(
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
