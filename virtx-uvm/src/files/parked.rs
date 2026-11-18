//! A parked sandbox's files as `dav-server` reads them, its machine left parked: its disk read
//! in this process ([`Sandbox::read_parked`]) and stacked over its image as its guest stacks
//! the two, and what a machine booted now mounts over them read where it is.
//!
//! The guest's root is overlayfs: the image's layers below, and over them the directory `upper`
//! of the sandbox's disk, an ext4 image. Here each is read for its metadata alone and the two
//! [stacked](FsTree::stack) by overlayfs's rules, once for each [era](Era) of the disk
//! ([`Views`]); a file's bytes are read from its layer, or its disk, as a `GET` asks for them.
//! The volumes the sandbox mounts, `/abin` and `/alib` are read where they are, as they are now.
//!
//! What is answered is what the agent in a booted machine answers of the same tree: the
//! kernel's filesystems and the agent's own left out at the root, and anything that is neither
//! a file nor a directory; a link along a path followed, and one at its end not; a name that is
//! not UTF-8 not listed. A file's `ETag` is its length and modification time either way, so it
//! is the same across a park and a boot that leave the file as it was.

use std::collections::VecDeque;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::{Buf, Bytes};
use dav_server::davpath::DavPath;
use dav_server::fs::{
    DavDirEntry, DavFile, DavFileSystem, DavMetaData, FsError, FsFuture, FsStream, OpenOptions,
    ReadDirMeta,
};
use futures_util::stream;

use super::volume::{Meta, described, refused};
use crate::disks::Image;
use crate::fstree::{self, Entry, Ext4, Ext4File, FsTree, Span, Timestamp};
use crate::sandbox::{Era, Error as SandboxError, Sandbox, Shown};
use crate::volume::Volume;

/// How many parked sandboxes' trees are kept, the one read longest ago let go first.
const VIEWS: usize = 32;
/// How many images' layers, stacked, are kept, the same.
const LOWERS: usize = 8;

/// What the host is not shown at the root, as the agent in a booted machine leaves it out: the
/// kernel's filesystems, and the agent's.
const HIDDEN: [&str; 4] = ["proc", "sys", "dev", ".virtx"];

/// How many links a path is followed through, as Linux follows them, before it is refused.
const MAX_LINKS: usize = 40;

/// Parked sandboxes' disks as they were read, and the images' layers they stand on.
pub(super) struct Views {
    /// By the sandbox's id, each with the era it was read in.
    views: Kept<String, (Era, Arc<View>)>,
    /// By the layers' files.
    lowers: Kept<Vec<PathBuf>, Arc<FsTree<Span>>>,
}

/// What is kept, by its key, the one read last last.
type Kept<K, V> = Mutex<VecDeque<(K, V)>>;

/// A parked sandbox's disk stacked over its image, as it is in one era.
pub(super) struct View {
    tree: FsTree<Located>,
    /// The disk as it is read, `None` for none made yet.
    ext4: Option<Ext4>,
}

/// Where a file's bytes are: in a layer of the image, or on the sandbox's disk.
#[derive(Clone)]
enum Located {
    Layer(Span),
    Disk(Ext4File),
}

impl Views {
    pub(super) fn new() -> Views {
        Views {
            views: Mutex::default(),
            lowers: Mutex::default(),
        }
    }

    /// `sandbox`'s disk stacked over its image while its machine is parked, and the era it is
    /// in: `None` while it is not, or if it boots before its disk is read. Read once for each
    /// era of the disk.
    pub(super) async fn view(
        &self,
        sandbox: &Arc<Sandbox>,
    ) -> Result<Option<(Era, Arc<View>)>, SandboxError> {
        let Some(era) = sandbox.parked_era().await else {
            return Ok(None);
        };
        let id = sandbox.id();
        if let Some(kept) = recent(&self.views, |at| at == id).filter(|(at, _)| *at == era) {
            return Ok(Some(kept));
        }
        let lower = self.lower(sandbox).await?;
        let read = sandbox
            .read_parked(era, move |image| View::read(&lower, image))
            .await?;
        let Some(read) = read else {
            return Ok(None);
        };
        let view = Arc::new(
            read.map_err(|e| SandboxError::Failed(format!("reading sandbox {id}'s disk: {e}")))?,
        );
        keep(&self.views, VIEWS, id.to_owned(), (era, Arc::clone(&view)));
        Ok(Some((era, view)))
    }

    /// The layers of the image `sandbox` stands on, stacked.
    async fn lower(&self, sandbox: &Sandbox) -> Result<Arc<FsTree<Span>>, SandboxError> {
        let layers = sandbox.layers()?;
        if let Some(lower) = recent(&self.lowers, |at| *at == layers) {
            return Ok(lower);
        }
        let reading = layers.clone();
        let stacked = tokio::task::spawn_blocking(move || {
            let mut tree = FsTree::new();
            for layer in &reading {
                tree.stack(FsTree::from_erofs_meta(layer)?);
            }
            Ok::<_, fstree::Error>(tree)
        })
        .await?
        .map_err(|e| SandboxError::Failed(format!("reading an image's layers: {e}")))?;
        let lower = Arc::new(stacked);
        keep(&self.lowers, LOWERS, layers, Arc::clone(&lower));
        Ok(lower)
    }
}

/// What `kept` has under the key `key` names, made the one read last.
fn recent<K, V: Clone>(kept: &Kept<K, V>, key: impl Fn(&K) -> bool) -> Option<V> {
    let mut kept = kept.lock().unwrap();
    let at = kept.iter().position(|(k, _)| key(k))?;
    let found = kept.remove(at)?;
    let value = found.1.clone();
    kept.push_back(found);
    Some(value)
}

/// `value` kept under `key` in `kept`, the one read last, over what was under it: at most `most`
/// kept, the one read longest ago let go.
fn keep<K: PartialEq, V>(kept: &Kept<K, V>, most: usize, key: K, value: V) {
    let mut kept = kept.lock().unwrap();
    kept.retain(|(k, _)| *k != key);
    while kept.len() >= most {
        kept.pop_front();
    }
    kept.push_back((key, value));
}

impl View {
    /// The disk read from `image` over `lower`: `lower` alone where there is no disk, or
    /// nothing on it yet that a machine wrote.
    fn read(lower: &FsTree<Span>, image: Option<&mut Image>) -> fstree::Result<View> {
        let mut tree = lower.map(|span| Located::Layer(span.clone()));
        let ext4 = match image {
            None => None,
            Some(image) => {
                let ext4 = Ext4::open(&mut *image)?;
                // The overlay's upper directory, which a machine's init makes as it boots.
                if let Some(upper) = ext4.meta(&mut *image)?.subtree(b"upper")? {
                    tree.stack(upper.map(|file| Located::Disk(*file)));
                }
                Some(ext4)
            }
        };
        Ok(View { tree, ext4 })
    }
}

/// A parked sandbox's files for one request: its [`View`], and what is mounted over it.
#[derive(Clone)]
pub(super) struct Fs {
    view: Arc<View>,
    /// Each at its path's names in the guest, in the order mounted.
    shown: Arc<[(Vec<String>, Shown)]>,
    /// The sandbox, and the era of its disk `view` is of: what a file on it is read from.
    disk: Option<(Arc<Sandbox>, Era)>,
}

impl Fs {
    pub(super) fn new(
        view: Arc<View>,
        shown: Vec<(String, Shown)>,
        disk: Option<(Arc<Sandbox>, Era)>,
    ) -> Fs {
        let shown = shown
            .into_iter()
            .map(|(at, shown)| (names(&at), shown))
            .collect();
        Fs { view, shown, disk }
    }

    /// The mount `at` is in, and how many of its names are the mount's: the deepest, and of
    /// two at one path the one mounted last, which lands over the other.
    fn mount_of(&self, at: &[String]) -> Option<(usize, usize)> {
        self.shown
            .iter()
            .enumerate()
            .filter(|(_, (names, _))| at.starts_with(names))
            .max_by_key(|(index, (names, _))| (names.len(), *index))
            .map(|(index, (names, _))| (index, names.len()))
    }

    /// Where `path` leads, as the guest's kernel walks it: a link along it followed, and one at
    /// its end if `last`, as listing a directory follows it and reading what is there does not.
    /// Answered with its names once followed, and the place they are in.
    fn resolve(&self, path: &str, last: bool) -> Result<(Vec<String>, Place), FsError> {
        let mut rest: VecDeque<String> = names(path).into();
        if rest.iter().any(|name| matches!(name.as_str(), "." | "..")) {
            return Err(FsError::Forbidden);
        }
        if rest
            .front()
            .is_some_and(|name| HIDDEN.contains(&name.as_str()))
        {
            return Err(FsError::NotFound);
        }
        let mut at = Vec::new();
        let mut links = 0;
        while let Some(name) = rest.pop_front() {
            match name.as_str() {
                "." => continue,
                ".." => {
                    at.pop();
                    continue;
                }
                _ => at.push(name),
            }
            // What is in a mount is the mount's to follow.
            if (rest.is_empty() && !last) || self.mount_of(&at).is_some() {
                continue;
            }
            match self.view.tree.entry(at.join("/").as_bytes()) {
                Ok(Some(Entry::Directory { .. })) => {}
                Ok(Some(Entry::Symlink { target, .. })) => {
                    links += 1;
                    if links > MAX_LINKS {
                        return Err(FsError::Forbidden);
                    }
                    at.pop();
                    let target = String::from_utf8_lossy(target);
                    if target.starts_with('/') {
                        at.clear();
                    }
                    for name in target.split('/').filter(|name| !name.is_empty()).rev() {
                        rest.push_front(name.to_owned());
                    }
                }
                // At the end, what is there is the listing's to refuse.
                Ok(Some(_)) if rest.is_empty() => {}
                _ => return Err(FsError::NotFound),
            }
        }
        let place = match self.mount_of(&at) {
            Some((index, depth)) => Place::Mounted(index, at[depth..].join("/")),
            None => Place::Tree(at.join("/")),
        };
        Ok((at, place))
    }

    async fn stat(&self, place: &Place) -> Result<Meta, FsError> {
        match place {
            Place::Tree(path) => match self.view.tree.entry(path.as_bytes()) {
                Ok(Some(entry)) => meta_of(entry).ok_or(FsError::NotFound),
                _ => Err(FsError::NotFound),
            },
            Place::Mounted(index, within) => match &self.shown[*index].1 {
                Shown::Volume(volume) => {
                    let stat = volume.stat(within).await.map_err(|e| refused(&e))?;
                    Ok(described(&stat))
                }
                Shown::Dir(dir) => host_stat(&dir.join(within)),
            },
        }
    }

    /// What is in the directory at `place`, `at` in the guest, with what is mounted in it.
    async fn list(&self, at: &[String], place: &Place) -> Result<Vec<(String, Meta)>, FsError> {
        let mut listed = match place {
            Place::Tree(path) => {
                let Ok(Some(entries)) = self.view.tree.entries(path.as_bytes()) else {
                    return Err(FsError::NotFound);
                };
                entries
                    .into_iter()
                    .filter_map(|(name, entry)| {
                        Some((String::from_utf8(name.to_vec()).ok()?, meta_of(entry)?))
                    })
                    .collect()
            }
            Place::Mounted(index, within) => match &self.shown[*index].1 {
                Shown::Volume(volume) => {
                    let entries = volume.read_dir(within).await.map_err(|e| refused(&e))?;
                    entries
                        .iter()
                        .filter_map(|entry| Some((entry.name.clone(), described(entry.stat()?))))
                        .collect()
                }
                Shown::Dir(dir) => host_list(&dir.join(within))?,
            },
        };
        for (names, shown) in self.shown.iter() {
            let Some((name, parent)) = names.split_last() else {
                continue;
            };
            if parent != at {
                continue;
            }
            let root = match shown {
                Shown::Volume(volume) => volume.stat("").await.ok().map(|stat| described(&stat)),
                Shown::Dir(dir) => host_stat(dir).ok(),
            };
            listed.retain(|(listed, _): &(String, Meta)| listed != name);
            listed.extend(root.map(|root| (name.clone(), root)));
        }
        if at.is_empty() {
            listed.retain(|(name, _)| !HIDDEN.contains(&name.as_str()));
        }
        Ok(listed)
    }
}

/// Where a path is once followed: in the stacked tree, by its path there; or in what is mounted
/// in the guest, by its place among them and its path in it.
enum Place {
    Tree(String),
    Mounted(usize, String),
}

/// The names along `path`, the root none.
fn names(path: &str) -> Vec<String> {
    path.split('/')
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect()
}

/// What the host is shown of `entry`: a file or a directory, and nothing else.
fn meta_of(entry: &Entry<Located>) -> Option<Meta> {
    let (meta, is_dir, len) = match entry {
        Entry::File { meta, data } => (meta, false, data.len()),
        Entry::Directory { meta, .. } => (meta, true, 0),
        _ => return None,
    };
    Some(Meta {
        is_dir,
        len,
        modified: modified(meta.mtime),
    })
}

/// `at` as the agent tells it: none before the epoch.
fn modified(at: Timestamp) -> Option<SystemTime> {
    let secs = u64::try_from(at.secs).ok()?;
    Some(UNIX_EPOCH + Duration::new(secs, at.nanos))
}

impl Located {
    fn len(&self) -> u64 {
        match self {
            Located::Layer(span) => span.len,
            Located::Disk(file) => file.len,
        }
    }
}

/// What the host is shown of `path` on this host, not followed: a file or a directory.
fn host_stat(path: &Path) -> Result<Meta, FsError> {
    let meta = std::fs::symlink_metadata(path).map_err(|_| FsError::NotFound)?;
    host_meta(&meta).ok_or(FsError::NotFound)
}

fn host_meta(meta: &std::fs::Metadata) -> Option<Meta> {
    let kind = meta.file_type();
    if !kind.is_file() && !kind.is_dir() {
        return None;
    }
    Some(Meta {
        is_dir: kind.is_dir(),
        len: if kind.is_dir() { 0 } else { meta.len() },
        modified: meta.modified().ok().filter(|at| *at >= UNIX_EPOCH),
    })
}

/// What is in the directory `dir` on this host, as [`host_stat`] shows each.
fn host_list(dir: &Path) -> Result<Vec<(String, Meta)>, FsError> {
    let entries = std::fs::read_dir(dir).map_err(|_| FsError::NotFound)?;
    Ok(entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.to_owned();
            Some((name, host_meta(&entry.metadata().ok()?)?))
        })
        .collect())
}

impl DavFileSystem for Fs {
    fn open<'a>(
        &'a self,
        path: &'a DavPath,
        options: OpenOptions,
    ) -> FsFuture<'a, Box<dyn DavFile>> {
        Box::pin(async move {
            if options.write || options.append || options.create || options.create_new {
                return Err(FsError::Forbidden);
            }
            let (_, place) = self.resolve(&at(path), false)?;
            let meta = self.stat(&place).await?;
            if meta.is_dir {
                return Err(FsError::Forbidden);
            }
            let from = match place {
                Place::Tree(path) => match self.view.tree.entry(path.as_bytes()) {
                    Ok(Some(Entry::File { data, .. })) => From::Located(data.clone()),
                    _ => return Err(FsError::NotFound),
                },
                Place::Mounted(index, within) => match &self.shown[index].1 {
                    Shown::Volume(volume) => From::Volume(Arc::clone(volume), within),
                    Shown::Dir(dir) => From::Host(dir.join(within)),
                },
            };
            Ok(Box::new(File {
                fs: self.clone(),
                from,
                offset: 0,
                meta,
            }) as Box<dyn DavFile>)
        })
    }

    fn read_dir<'a>(
        &'a self,
        path: &'a DavPath,
        _: ReadDirMeta,
    ) -> FsFuture<'a, FsStream<Box<dyn DavDirEntry>>> {
        Box::pin(async move {
            let (at, place) = self.resolve(&at(path), true)?;
            let entries: Vec<_> = self
                .list(&at, &place)
                .await?
                .into_iter()
                .map(|(name, meta)| {
                    Ok(Box::new(Listed {
                        name: name.into_bytes(),
                        meta,
                    }) as Box<dyn DavDirEntry>)
                })
                .collect();
            Ok(Box::pin(stream::iter(entries)) as FsStream<Box<dyn DavDirEntry>>)
        })
    }

    /// The same as `metadata`: a link is nothing this root answers.
    fn symlink_metadata<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, Box<dyn DavMetaData>> {
        self.metadata(path)
    }

    fn metadata<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, Box<dyn DavMetaData>> {
        Box::pin(async move {
            let (_, place) = self.resolve(&at(path), false)?;
            Ok(Box::new(self.stat(&place).await?) as Box<dyn DavMetaData>)
        })
    }
}

/// `path` as the guest spells it: absolute, and the root `/`.
fn at(path: &DavPath) -> String {
    format!("/{}", super::volume::at(path))
}

/// A file open to read.
struct File {
    fs: Fs,
    from: From,
    offset: u64,
    meta: Meta,
}

/// Where a file open to read is read from.
enum From {
    Located(Located),
    Volume(Arc<Volume>, String),
    Host(PathBuf),
}

impl std::fmt::Debug for File {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match &self.fs.disk {
            Some((sandbox, _)) => write!(f, "a file of parked sandbox {}", sandbox.id()),
            None => write!(f, "a file of a parked sandbox"),
        }
    }
}

/// Up to `len` bytes of the file at `path` on this host from `offset`, its bytes starting at
/// `start` in it.
fn read_at(path: &Path, start: u64, offset: u64, len: u64) -> std::io::Result<Vec<u8>> {
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(start + offset))?;
    let mut read = Vec::new();
    file.take(len).read_to_end(&mut read)?;
    Ok(read)
}

impl File {
    /// Up to `count` bytes from where it is.
    async fn read(&self, count: u64) -> Result<Vec<u8>, FsError> {
        let offset = self.offset;
        match &self.from {
            From::Located(Located::Layer(span)) => {
                let span = span.clone();
                let len = count.min(span.len.saturating_sub(offset));
                tokio::task::spawn_blocking(move || read_at(&span.path, span.offset(), offset, len))
                    .await
                    .map_err(|_| FsError::GeneralFailure)?
                    .map_err(|_| FsError::GeneralFailure)
            }
            From::Located(Located::Disk(file)) => {
                let Some((sandbox, era)) = &self.fs.disk else {
                    return Err(FsError::GeneralFailure);
                };
                let (view, file) = (Arc::clone(&self.fs.view), *file);
                let read = sandbox
                    .read_parked(*era, move |image| {
                        let (Some(image), Some(ext4)) = (image, &view.ext4) else {
                            return Err(FsError::GeneralFailure);
                        };
                        ext4.read(image, &file, offset, count)
                            .map_err(|_| FsError::GeneralFailure)
                    })
                    .await;
                // Its machine booted since the file was opened: what it is now is the
                // machine's to say.
                match read {
                    Ok(Some(read)) => read,
                    Ok(None) | Err(_) => Err(FsError::GeneralFailure),
                }
            }
            From::Volume(volume, path) => volume
                .read(path, offset, Some(count))
                .await
                .map_err(|e| refused(&e)),
            From::Host(path) => {
                let path = path.clone();
                tokio::task::spawn_blocking(move || read_at(&path, 0, offset, count))
                    .await
                    .map_err(|_| FsError::GeneralFailure)?
                    .map_err(|_| FsError::GeneralFailure)
            }
        }
    }
}

impl DavFile for File {
    fn metadata(&mut self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        Box::pin(async move { Ok(Box::new(self.meta.clone()) as Box<dyn DavMetaData>) })
    }

    fn write_buf(&mut self, _: Box<dyn Buf + Send>) -> FsFuture<'_, ()> {
        Box::pin(async { Err(FsError::Forbidden) })
    }

    fn write_bytes(&mut self, _: Bytes) -> FsFuture<'_, ()> {
        Box::pin(async { Err(FsError::Forbidden) })
    }

    fn read_bytes(&mut self, count: usize) -> FsFuture<'_, Bytes> {
        Box::pin(async move {
            let read = self.read(count as u64).await?;
            self.offset += read.len() as u64;
            Ok(Bytes::from(read))
        })
    }

    fn seek(&mut self, pos: SeekFrom) -> FsFuture<'_, u64> {
        Box::pin(async move {
            let offset = match pos {
                SeekFrom::Start(n) => Some(n),
                SeekFrom::End(n) => self.meta.len.checked_add_signed(n),
                SeekFrom::Current(n) => self.offset.checked_add_signed(n),
            };
            self.offset = offset.ok_or(FsError::GeneralFailure)?;
            Ok(self.offset)
        })
    }

    fn flush(&mut self) -> FsFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
}

struct Listed {
    name: Vec<u8>,
    meta: Meta,
}

impl DavDirEntry for Listed {
    fn name(&self) -> Vec<u8> {
        self.name.clone()
    }

    fn metadata(&self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        Box::pin(async move { Ok(Box::new(self.meta.clone()) as Box<dyn DavMetaData>) })
    }
}

#[cfg(test)]
mod tests {
    use futures_util::StreamExt as _;

    use super::*;
    use crate::fstree::Meta as FsMeta;

    fn file(data: &[u8], mtime: i64) -> Entry {
        Entry::File {
            meta: FsMeta {
                mode: 0o644,
                mtime: Timestamp {
                    secs: mtime,
                    nanos: 500,
                },
                ..FsMeta::default()
            },
            data: data.to_vec(),
        }
    }

    fn dir(opaque: bool) -> Entry {
        Entry::Directory {
            meta: FsMeta {
                mode: 0o755,
                ..FsMeta::default()
            },
            opaque,
        }
    }

    fn symlink(target: &str) -> Entry {
        Entry::Symlink {
            meta: FsMeta::default(),
            target: target.as_bytes().to_vec(),
        }
    }

    /// An image of one layer, and a disk over it as a machine leaves one: what it wrote under
    /// `upper/`, in overlayfs's spelling, beside the overlay's `work/`; with `/abin` shown from a
    /// directory of this host's.
    fn sandbox(scratch: &Path) -> Fs {
        let mut layer = FsTree::new();
        for name in [
            "etc", "var", "usr", "usr/bin", "proc", "dev", "kept", "hidden",
        ] {
            layer.insert(name.as_bytes(), dir(false)).unwrap();
        }
        layer.insert(b"etc/hostname", file(b"lower", 10)).unwrap();
        layer.insert(b"etc/passwd", file(b"root", 10)).unwrap();
        layer.insert(b"var/gone.txt", file(b"gone", 10)).unwrap();
        layer.insert(b"replaced.txt", file(b"lower", 10)).unwrap();
        layer.insert(b"kept/below.txt", file(b"below", 10)).unwrap();
        layer
            .insert(b"hidden/below.txt", file(b"below", 10))
            .unwrap();
        layer.insert(b"usr/bin/tool", file(b"tool", 10)).unwrap();
        layer.insert(b"bin", symlink("usr/bin")).unwrap();
        let erofs = scratch.join("layer.erofs");
        layer
            .to_erofs(std::fs::File::create(&erofs).unwrap())
            .unwrap();
        let lower = FsTree::from_erofs_meta(&erofs).unwrap();

        let mut disk = FsTree::new();
        disk.insert(b"work", dir(false)).unwrap();
        disk.insert(b"upper", dir(false)).unwrap();
        disk.insert(b"upper/replaced.txt", file(b"upper!", 20))
            .unwrap();
        disk.insert(b"upper/var", dir(false)).unwrap();
        disk.insert(b"upper/var/gone.txt", Entry::Whiteout).unwrap();
        disk.insert(b"upper/hidden", dir(true)).unwrap();
        disk.insert(b"upper/hidden/new.txt", file(b"new", 20))
            .unwrap();
        disk.insert(b"upper/kept/new.txt", file(b"new", 20))
            .unwrap();
        disk.insert(b"upper/home/a", file(b"linked", 20)).unwrap();
        disk.link(b"upper/home/b", b"upper/home/a").unwrap();
        disk.insert(b"upper/home/link", symlink("a")).unwrap();
        disk.insert(b"upper/home/up", symlink("/etc")).unwrap();
        disk.insert(b"upper/home/fifo", Entry::Fifo(FsMeta::default()))
            .unwrap();
        let device = Entry::Device {
            meta: FsMeta::default(),
            block: false,
            major: 1,
            minor: 3,
        };
        disk.insert(b"upper/home/null", device).unwrap();
        disk.insert(b"upper/abin", dir(false)).unwrap();
        let ext4 = scratch.join("disk.ext4");
        disk.to_ext4(std::fs::File::create(&ext4).unwrap(), 64 << 20)
            .unwrap();
        let mut image = Image::open(&ext4, None).unwrap();
        let view = View::read(&lower, Some(&mut image)).unwrap();

        let abin = scratch.join("abin");
        std::fs::create_dir(&abin).unwrap();
        std::fs::write(abin.join("program"), b"#!").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("program", abin.join("link")).unwrap();
        let shown = vec![("/abin".to_owned(), Shown::Dir(abin))];
        Fs::new(Arc::new(view), shown, None)
    }

    fn path(path: &str) -> DavPath {
        DavPath::new(path).unwrap()
    }

    async fn listed(fs: &Fs, at: &str) -> Vec<(String, bool, u64)> {
        let mut entries = fs
            .read_dir(&path(at), ReadDirMeta::Data)
            .await
            .unwrap_or_else(|e| panic!("{at}: {e:?}"));
        let mut listed = Vec::new();
        while let Some(entry) = entries.next().await {
            let entry = entry.unwrap();
            let meta = entry.metadata().await.unwrap();
            let name = String::from_utf8(entry.name()).unwrap();
            listed.push((name, meta.is_dir(), meta.len()));
        }
        listed.sort();
        listed
    }

    async fn stat(fs: &Fs, at: &str) -> Result<(bool, u64), FsError> {
        let meta = fs.metadata(&path(at)).await?;
        Ok((meta.is_dir(), meta.len()))
    }

    async fn read(fs: &Fs, at: &str) -> Result<Vec<u8>, FsError> {
        let reading = OpenOptions {
            read: true,
            ..Default::default()
        };
        let mut file = fs.open(&path(at), reading).await?;
        let mut read = Vec::new();
        loop {
            let bytes = file.read_bytes(3).await?;
            if bytes.is_empty() {
                return Ok(read);
            }
            read.extend_from_slice(&bytes);
        }
    }

    fn entries(names: &[(&str, bool, u64)]) -> Vec<(String, bool, u64)> {
        names
            .iter()
            .map(|(name, is_dir, len)| (name.to_string(), *is_dir, *len))
            .collect()
    }

    #[tokio::test]
    async fn the_disk_is_stacked_over_the_image_as_overlayfs_stacks_them() {
        let scratch = tempfile::tempdir().unwrap();
        let fs = sandbox(scratch.path());

        // The root: the kernel's filesystems left out, and a link.
        assert_eq!(
            listed(&fs, "/").await,
            entries(&[
                ("abin", true, 0),
                ("etc", true, 0),
                ("hidden", true, 0),
                ("home", true, 0),
                ("kept", true, 0),
                ("replaced.txt", false, 6),
                ("usr", true, 0),
                ("var", true, 0),
            ])
        );
        // A whiteout takes the file below away; an opaque directory all below it; a directory
        // over a directory keeps what is below.
        assert_eq!(listed(&fs, "/var").await, entries(&[]));
        assert_eq!(stat(&fs, "/var/gone.txt").await, Err(FsError::NotFound));
        assert_eq!(
            listed(&fs, "/hidden").await,
            entries(&[("new.txt", false, 3)])
        );
        assert_eq!(
            listed(&fs, "/kept").await,
            entries(&[("below.txt", false, 5), ("new.txt", false, 3)])
        );
        // A file replaced above is the one above, and one below is read from its layer.
        assert_eq!(stat(&fs, "/replaced.txt").await, Ok((false, 6)));
        assert_eq!(read(&fs, "/etc/hostname").await.unwrap(), b"lower");
        // Two names of one file both listed; a link, a pipe and a device neither listed nor
        // answered.
        assert_eq!(
            listed(&fs, "/home").await,
            entries(&[("a", false, 6), ("b", false, 6)])
        );
        for at in ["/home/link", "/home/up", "/home/fifo", "/home/null", "/bin"] {
            assert_eq!(stat(&fs, at).await, Err(FsError::NotFound), "{at}");
        }
        // A link along a path is followed, from where it is or from the root.
        assert_eq!(stat(&fs, "/bin/tool").await, Ok((false, 4)));
        assert_eq!(read(&fs, "/bin/tool").await.unwrap(), b"tool");
        assert_eq!(read(&fs, "/home/up/passwd").await.unwrap(), b"root");
        assert_eq!(
            listed(&fs, "/home/up").await,
            entries(&[("hostname", false, 5), ("passwd", false, 4)])
        );
        // The kernel's filesystems are not there; a directory is not a file to read.
        assert_eq!(stat(&fs, "/proc").await, Err(FsError::NotFound));
        assert_eq!(stat(&fs, "/dev/null").await, Err(FsError::NotFound));
        assert!(matches!(read(&fs, "/etc").await, Err(FsError::Forbidden)));
        assert_eq!(stat(&fs, "/nowhere").await, Err(FsError::NotFound));
        assert_eq!(
            stat(&fs, "/replaced.txt/under").await,
            Err(FsError::NotFound)
        );
        // What is mounted is read where it is, over what the disk has at its path.
        #[cfg(unix)]
        assert_eq!(
            listed(&fs, "/abin").await,
            entries(&[("program", false, 2)])
        );
        assert_eq!(read(&fs, "/abin/program").await.unwrap(), b"#!");
    }

    /// A file on the disk is read from the disk, which only a sandbox's own read does: here,
    /// the bytes and the length the tree says agree.
    #[test]
    fn a_file_on_the_disk_is_read_from_its_inode() {
        let scratch = tempfile::tempdir().unwrap();
        let fs = sandbox(scratch.path());
        let Ok(Some(Entry::File {
            data: Located::Disk(file),
            meta,
        })) = fs.view.tree.entry(b"replaced.txt")
        else {
            panic!("replaced.txt is the disk's");
        };
        assert_eq!(
            modified(meta.mtime),
            Some(UNIX_EPOCH + Duration::new(20, 500))
        );
        let mut image = Image::open(&scratch.path().join("disk.ext4"), None).unwrap();
        let ext4 = fs.view.ext4.as_ref().unwrap();
        assert_eq!(ext4.read(&mut image, file, 0, 100).unwrap(), b"upper!");
    }

    /// With no disk made yet, the image alone.
    #[tokio::test]
    async fn without_a_disk_the_image_is_alone() {
        let scratch = tempfile::tempdir().unwrap();
        let mut layer = FsTree::new();
        layer.insert(b"etc/hostname", file(b"lower", 10)).unwrap();
        let erofs = scratch.path().join("layer.erofs");
        layer
            .to_erofs(std::fs::File::create(&erofs).unwrap())
            .unwrap();
        let lower = FsTree::from_erofs_meta(&erofs).unwrap();
        let fs = Fs::new(
            Arc::new(View::read(&lower, None).unwrap()),
            Vec::new(),
            None,
        );
        assert_eq!(listed(&fs, "/").await, entries(&[("etc", true, 0)]));
        assert_eq!(read(&fs, "/etc/hostname").await.unwrap(), b"lower");
    }
}
