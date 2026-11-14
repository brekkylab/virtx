//! A volume: storage kept by name apart from any machine, with remote stores mounted at paths
//! in it.
//!
//! # Layout
//!
//! ```text
//! <root>/
//!   volume/<name>/files/        what the volume holds, and what a machine shares
//!   volume/<name>/mounts.json   its mounts, credentials and all: readable by this user alone
//!   volume/<name>/lock          what a machine that mounts it, or a removal of it, holds
//! ```
//!
//! A machine a volume is mounted into writes the same directory, and may put a symbolic link
//! in it that points anywhere on this host. So every path is resolved through a [`Dir`] on the
//! volume's root, which never leaves it however the guest rearranges the tree under a request.
//! On top of that a symbolic link is left out of a listing, and a path to or through one is
//! refused as if nothing were there.
//!
//! A path under one of the volume's mounts is the mount's [`Remote`]'s instead. The
//! stores take no changes, so only reads reach one, and a copy out of one into the volume.

use std::fs::{self, File};
use std::io::{self, Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cap_std::fs::{Dir, OpenOptions};
use serde::{Deserialize, Serialize};
use virtx_core::MountSource;

use super::{Dirent, DirentKind, Remote, Stat};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0:?} is not a name: letters, digits, '.', '_' and '-', not starting with '.' or '-'")]
    InvalidName(String),

    #[error("no volume here is named {0}")]
    Unknown(String),

    #[error("a volume named {0} is already here")]
    AlreadyExists(String),

    #[error("a mount is at {0} already")]
    MountExists(String),

    #[error("volume {0} is mounted by a machine or being removed")]
    InUse(String),

    #[error(
        "{0:?} is not a path in a volume: relative, '/'-separated, without '.', '..' or empty \
         components"
    )]
    InvalidPath(String),

    /// A request a volume cannot take as asked: one on its root, one that would copy a tree
    /// into itself, or a change to or over a mount.
    #[error("{0}")]
    Refused(String),

    #[error("no mount in {volume} is at {path}")]
    NoMount { volume: String, path: String },

    /// A mount's source that its store could not be made from.
    #[error("making the mount's store: {0}")]
    Source(String),

    #[error("reading {0}: {1}")]
    Origins(&'static str, #[source] serde_json::Error),

    /// A filesystem operation, on the volume's own files or a store's. `source.kind()` says
    /// whether it was nothing there, something already there, a mount taking no changes...
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: io::Error,
    },
}

impl Error {
    fn io(context: impl Into<String>, source: io::Error) -> Error {
        Error::Io {
            context: context.into(),
            source,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// The most one [`Volume::read`] answers; a caller reads on from where it stops.
pub const MAX_READ: u64 = 4 << 20;

#[derive(Clone, Debug)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Result<Store> {
        let root = root.into();
        let dir = root.join("volume");
        fs::create_dir_all(&dir).map_err(|e| Error::io(format!("making {}", dir.display()), e))?;
        Ok(Store { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The name of every volume here, in no set order.
    pub fn list(&self) -> Result<Vec<String>> {
        let dir = self.root.join("volume");
        let mut names = Vec::new();
        for entry in
            fs::read_dir(&dir).map_err(|e| Error::io(format!("reading {}", dir.display()), e))?
        {
            let entry = entry.map_err(|e| Error::io(format!("reading {}", dir.display()), e))?;
            if let (Some(name), true) = (
                entry.file_name().to_str(),
                entry.path().join("files").is_dir(),
            ) {
                names.push(name.to_owned());
            }
        }
        Ok(names)
    }

    /// Make an empty volume named `name`.
    pub fn create(&self, name: &str) -> Result<Volume> {
        let dir = self.dir(name)?;
        // Made exclusively: two creates of one name cannot both win.
        match fs::create_dir(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                return Err(Error::AlreadyExists(name.to_owned()));
            }
            Err(e) => return Err(Error::io(format!("making {}", dir.display()), e)),
        }
        let files = dir.join("files");
        fs::create_dir(&files).map_err(|e| Error::io(format!("making {}", files.display()), e))?;
        self.open(name)
    }

    /// Volume `name`, which has to be here.
    pub fn open(&self, name: &str) -> Result<Volume> {
        Volume::open(self.dir(name)?)
    }

    /// Delete volume `name` and everything in it -- refused while a sandbox or a machine holds it.
    pub fn remove(&self, name: &str) -> Result<()> {
        let dir = self.dir(name)?;
        let files = dir.join("files");
        if !files.is_dir() {
            return Err(Error::Unknown(name.to_owned()));
        }
        let lock = lock(&dir, name)?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => return Err(Error::InUse(name.to_owned())),
            Err(fs::TryLockError::Error(e)) => return Err(Error::io(format!("locking {name}"), e)),
        }
        // Its contents while it is held, so no machine mounts it half gone; then the rest, which
        // a machine that takes the lock now finds no files in.
        fs::remove_dir_all(&files)
            .map_err(|e| Error::io(format!("removing {}", files.display()), e))?;
        drop(lock);
        fs::remove_dir_all(&dir).map_err(|e| Error::io(format!("removing {}", dir.display()), e))
    }

    /// Put the files at `files` in place as volume `name`'s, over those there if any -- refused
    /// while a machine holds the volume. Its mounts are kept. `files` is moved, not copied, so
    /// it has to be on this store's filesystem: under [`root`](Store::root).
    pub fn replace(&self, name: &str, files: &Path) -> Result<Volume> {
        let dir = self.dir(name)?;
        fs::create_dir_all(&dir).map_err(|e| Error::io(format!("making {}", dir.display()), e))?;
        let lock = lock(&dir, name)?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => return Err(Error::InUse(name.to_owned())),
            Err(fs::TryLockError::Error(e)) => return Err(Error::io(format!("locking {name}"), e)),
        }
        let at = dir.join("files");
        let old = dir.join("files.old");
        let _ = fs::remove_dir_all(&old);
        match fs::rename(&at, &old) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(Error::io(format!("moving {} aside", at.display()), e)),
        }
        fs::rename(files, &at)
            .map_err(|e| Error::io(format!("moving {} into place", at.display()), e))?;
        drop(lock);
        let _ = fs::remove_dir_all(&old);
        self.open(name)
    }

    /// Volume `name`'s own directory, refused unless `name` [is one](crate::is_name).
    fn dir(&self, name: &str) -> Result<PathBuf> {
        match crate::is_name(name) {
            true => Ok(self.root.join("volume").join(name)),
            false => Err(Error::InvalidName(name.to_owned())),
        }
    }
}

/// A volume, opened: its files' root, and the mounts in it.
pub struct Volume {
    name: String,
    /// Its own directory: `files/`, `mounts.json` and the lock.
    dir: PathBuf,
    root: Dir,
    mounts: Vec<Mount>,
}

/// One remote store shown at a path in a volume, as `mounts.json` keeps it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Mount {
    /// Where it is in the volume: relative and `/`-separated.
    pub path: String,
    pub source: MountSource,
}

/// A volume held by a sandbox or a machine that mounts it, beside any other, and against its
/// removal, until this drops.
#[derive(Debug)]
pub struct Hold(#[allow(dead_code)] File);

/// A path in a volume, as it falls with respect to the volume's mounts.
enum Place {
    /// In the volume's own files.
    Local,
    /// Under a mount: `rest` is the path within it, `/`-rooted, so `/` is the mount point.
    Mounted { rest: PathBuf, store: Arc<Remote> },
}

impl Volume {
    /// The volume whose own directory is `dir`, as [`path`](Volume::path) answers it.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Volume> {
        let dir = dir.into();
        let name = dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let at = dir.join("files");
        let root = open_root(&at).map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => Error::Unknown(name.clone()),
            _ => Error::io(format!("opening {}", at.display()), e),
        })?;
        let table = dir.join("mounts.json");
        let mounts = match fs::read(&table) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| {
                Error::io(format!("reading {}", table.display()), io::Error::other(e))
            })?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(Error::io(format!("reading {}", table.display()), e)),
        };
        Ok(Volume {
            name,
            dir,
            root,
            mounts,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Its own directory, which [`open`](Volume::open) opens it from again.
    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// Where its files are on this host, for a machine to share.
    pub fn files(&self) -> PathBuf {
        self.dir.join("files")
    }

    /// The bytes its files take, links not followed.
    pub fn size(&self) -> u64 {
        size_of(&self.files())
    }

    pub fn mounts(&self) -> &[Mount] {
        &self.mounts
    }

    /// Hold it for a sandbox or a machine that mounts it. Refused while it is being removed.
    pub fn hold(&self) -> Result<Hold> {
        let lock = lock(&self.dir, &self.name)?;
        match lock.try_lock_shared() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => return Err(Error::InUse(self.name.clone())),
            Err(fs::TryLockError::Error(e)) => {
                return Err(Error::io(format!("locking {}", self.name), e));
            }
        }
        if !self.files().is_dir() {
            return Err(Error::Unknown(self.name.clone()));
        }
        Ok(Hold(lock))
    }

    /// Hold it alone, as its files are read whole: no machine mounts it, and it is not removed,
    /// until this drops. Refused while a machine holds it.
    pub fn hold_alone(&self) -> Result<Hold> {
        let lock = lock(&self.dir, &self.name)?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => return Err(Error::InUse(self.name.clone())),
            Err(fs::TryLockError::Error(e)) => {
                return Err(Error::io(format!("locking {}", self.name), e));
            }
        }
        if !self.files().is_dir() {
            return Err(Error::Unknown(self.name.clone()));
        }
        Ok(Hold(lock))
    }

    /// Show `source` at `path`, a directory made for it that must not be there yet.
    pub fn mount(&mut self, path: String, source: MountSource) -> Result<()> {
        let at = relative(&path)?;
        if at.as_os_str().is_empty() {
            return Err(Error::Refused(
                "a mount cannot be at the volume's root".into(),
            ));
        }
        let clash = self.mounts.iter().find(|mount| {
            relative(&mount.path)
                .is_ok_and(|other| at.starts_with(&other) || other.starts_with(&at))
        });
        // At the very path, it is the mount there; inside or over it, a path no mount can be at.
        if let Some(clash) =
            clash.filter(|clash| relative(&clash.path).is_ok_and(|other| other == at))
        {
            return Err(Error::MountExists(clash.path.clone()));
        }
        if let Some(clash) = clash {
            return Err(Error::Refused(format!(
                "{path} would be inside or over the mount at {}",
                clash.path
            )));
        }
        // Made before anything is written, so a source that cannot be is refused here.
        Remote::new(&source)?;
        match target(&self.root, &at).map_err(|e| self.failed(e, &path))? {
            Some(_) => return Err(self.failed(io::ErrorKind::AlreadyExists.into(), &path)),
            None => self
                .root
                .create_dir(&at)
                .map_err(|e| self.failed(e, &path))?,
        }
        self.mounts.push(Mount { path, source });
        self.keep()
    }

    /// Take away the mount at `path`, and the directory it was shown in if that is empty.
    pub fn unmount(&mut self, path: &str) -> Result<()> {
        let at = relative(path)?;
        let before = self.mounts.len();
        self.mounts
            .retain(|mount| relative(&mount.path).map_or(true, |other| other != at));
        if self.mounts.len() == before {
            return Err(Error::NoMount {
                volume: self.name.clone(),
                path: path.to_owned(),
            });
        }
        self.keep()?;
        let _ = self.root.remove_dir(&at);
        Ok(())
    }

    pub async fn stat(&self, path: &str) -> Result<Stat> {
        let at = relative(path)?;
        let stat = match self.place(&at)? {
            Place::Local => existing(&self.root, &at).and_then(|meta| described(&meta)),
            Place::Mounted { rest, store } => store.stat(&rest).await,
        };
        stat.map_err(|e| self.failed(e, path))
    }

    /// The entries of `path`, each with its metadata.
    pub async fn read_dir(&self, path: &str) -> Result<Vec<Dirent>> {
        let at = relative(path)?;
        if let Place::Mounted { rest, store } = self.place(&at)? {
            return listed_in(&store, &rest)
                .await
                .map_err(|e| self.failed(e, path));
        }
        let listed = || -> io::Result<Vec<Dirent>> {
            existing(&self.root, &at)?;
            let mut listed = Vec::new();
            for entry in self.root.read_dir(spelled(&at))? {
                let entry = entry?;
                // Not followed: a link is left out, whatever it points at.
                let Ok(stat) = entry.metadata().and_then(|meta| described(&meta)) else {
                    continue;
                };
                let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                listed.push(Dirent::with_stat(name, stat));
            }
            Ok(listed)
        };
        listed().map_err(|e| self.failed(e, path))
    }

    /// Up to `length` bytes of `path` from `offset`, and never more than [`MAX_READ`].
    pub async fn read(&self, path: &str, offset: u64, length: Option<u64>) -> Result<Vec<u8>> {
        let at = relative(path)?;
        let length = length.unwrap_or(MAX_READ).min(MAX_READ);
        if let Place::Mounted { rest, store } = self.place(&at)? {
            return read_from(&store, &rest, offset, length)
                .await
                .map_err(|e| self.failed(e, path));
        }
        let read = || -> io::Result<Vec<u8>> {
            existing(&self.root, &at)?;
            let mut file = self.root.open(&at)?;
            file.seek(SeekFrom::Start(offset))?;
            let mut data = Vec::new();
            file.take(length).read_to_end(&mut data)?;
            Ok(data)
        };
        read().map_err(|e| self.failed(e, path))
    }

    /// Write `data` at `offset` in `path`, made if it is not there; without an offset the file
    /// is `data` and nothing else.
    pub fn write(&self, path: &str, data: &[u8], offset: Option<u64>) -> Result<()> {
        let at = relative(path)?;
        let write = || -> io::Result<()> {
            self.local(&at)?;
            target(&self.root, &at)?;
            let mut options = OpenOptions::new();
            options.write(true).create(true).truncate(offset.is_none());
            let mut file = self.root.open_with(&at, &options)?;
            if let Some(offset) = offset {
                file.seek(SeekFrom::Start(offset))?;
            }
            file.write_all(data)
        };
        write().map_err(|e| self.failed(e, path))
    }

    pub fn create_dir(&self, path: &str, parents: bool) -> Result<()> {
        let at = relative(path)?;
        let create = || -> io::Result<()> {
            self.local(&at)?;
            if !parents {
                target(&self.root, &at)?;
                return self.root.create_dir(spelled(&at));
            }
            // One level at a time, so a link on the way is refused rather than followed.
            let mut prefix = PathBuf::new();
            for part in at.components() {
                prefix.push(part);
                match target(&self.root, &prefix)? {
                    Some(meta) if meta.is_dir() => {}
                    Some(_) => return Err(io::ErrorKind::AlreadyExists.into()),
                    None => self.root.create_dir(&prefix)?,
                }
            }
            Ok(())
        };
        create().map_err(|e| self.failed(e, path))
    }

    pub fn remove_file(&self, path: &str, recursive: bool) -> Result<()> {
        let at = not_root(relative(path)?, path)?;
        self.unmounted(&at, path)?;
        let remove = || -> io::Result<()> {
            self.local(&at)?;
            let meta = existing(&self.root, &at)?;
            match (meta.is_dir(), recursive) {
                (true, true) => self.root.remove_dir_all(&at),
                (true, false) => self.root.remove_dir(&at),
                (false, _) => self.root.remove_file(&at),
            }
        };
        remove().map_err(|e| self.failed(e, path))
    }

    pub fn rename(&self, from: &str, to: &str, overwrite: bool) -> Result<()> {
        let from_at = not_root(relative(from)?, from)?;
        let to_at = not_root(relative(to)?, to)?;
        self.unmounted(&from_at, from)?;
        self.unmounted(&to_at, to)?;
        let rename = || -> io::Result<()> {
            self.local(&from_at)?;
            self.local(&to_at)?;
            existing(&self.root, &from_at)?;
            if target(&self.root, &to_at)?.is_some() && !overwrite {
                return Err(io::ErrorKind::AlreadyExists.into());
            }
            self.root.rename(&from_at, &self.root, &to_at)
        };
        rename().map_err(|e| self.failed(e, &format!("{from} -> {to}")))
    }

    /// Copy `from` to `to`, a directory with everything in it but links. `from` may be under a
    /// mount; `to` may not.
    pub async fn copy(&self, from: &str, to: &str, overwrite: bool) -> Result<()> {
        let from_at = relative(from)?;
        let to_at = not_root(relative(to)?, to)?;
        if to_at.starts_with(&from_at) {
            return Err(Error::Refused(format!(
                "{to} is inside {from}, so a copy would copy itself"
            )));
        }
        self.unmounted(&to_at, to)?;
        let spelled = format!("{from} -> {to}");
        let failed = |e| self.failed(e, &spelled);
        self.local(&to_at).map_err(failed)?;
        // What is at `to` goes once `from` is known to be there.
        let clear = || -> io::Result<()> {
            match target(&self.root, &to_at)? {
                Some(_) if !overwrite => Err(io::ErrorKind::AlreadyExists.into()),
                Some(there) if there.is_dir() => self.root.remove_dir_all(&to_at),
                Some(_) => self.root.remove_file(&to_at),
                None => Ok(()),
            }
        };
        match self.place(&from_at)? {
            Place::Mounted { rest, store } => {
                let kind = store.stat(&rest).await.map_err(failed)?.kind;
                clear().map_err(failed)?;
                copy_out(&store, &rest, kind, &self.root, &to_at)
                    .await
                    .map_err(failed)?;
            }
            Place::Local => {
                self.unmounted(&from_at, from)?;
                let meta = existing(&self.root, &from_at).map_err(failed)?;
                clear().map_err(failed)?;
                copy_tree(&self.root, &from_at, &to_at, meta.is_dir()).map_err(failed)?;
            }
        }
        Ok(())
    }

    /// Where `at` falls among the mounts.
    fn place(&self, at: &Path) -> Result<Place> {
        for mount in &self.mounts {
            let mounted = relative(&mount.path)?;
            if let Ok(rest) = at.strip_prefix(&mounted) {
                // The source is in the key, so a mount made again at the same path, here or
                // by another server, is a new store.
                let source = serde_json::to_string(&mount.source)
                    .map_err(|e| Error::Source(e.to_string()))?;
                let key = format!("{}\n{}\n{source}", self.name, mount.path);
                return Ok(Place::Mounted {
                    store: Remote::shared(key, &mount.source)?,
                    rest: Path::new("/").join(rest),
                });
            }
        }
        Ok(Place::Local)
    }

    /// Refused unless `at` is in the volume's own files: a mount takes no changes.
    fn local(&self, at: &Path) -> io::Result<()> {
        let mounted = self
            .mounts
            .iter()
            .any(|mount| relative(&mount.path).is_ok_and(|m| at.starts_with(m)));
        match mounted {
            true => Err(io::Error::new(
                io::ErrorKind::ReadOnlyFilesystem,
                "a mount takes no changes",
            )),
            false => Ok(()),
        }
    }

    /// Refused when `at` is or holds a mount, which a change to it would leave behind.
    fn unmounted(&self, at: &Path, path: &str) -> Result<()> {
        let below = self
            .mounts
            .iter()
            .find(|mount| relative(&mount.path).is_ok_and(|m| m.starts_with(at)));
        match below {
            Some(mount) => Err(Error::Refused(format!(
                "{path} is or holds the mount at {}; unmount it first",
                mount.path
            ))),
            None => Ok(()),
        }
    }

    /// Write the mounts readable by this user alone, beside their name, and move them under it
    /// once whole.
    fn keep(&self) -> Result<()> {
        let at = self.dir.join("mounts.json");
        let part = self.dir.join("mounts.json.part");
        let written = File::create(&part)
            .and_then(|file| {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    file.set_permissions(fs::Permissions::from_mode(0o600))?;
                }
                serde_json::to_writer(file, &self.mounts).map_err(io::Error::other)
            })
            .and_then(|()| fs::rename(&part, &at));
        if written.is_err() {
            let _ = fs::remove_file(&part);
        }
        written.map_err(|e| Error::io(format!("writing {}", at.display()), e))
    }

    /// `e`, from an operation on `path` in this volume.
    fn failed(&self, e: io::Error, path: &str) -> Error {
        Error::io(format!("{}: {path}", self.name), e)
    }
}

/// The file volume `name`, whose own directory is `dir`, is locked by.
fn lock(dir: &Path, name: &str) -> Result<File> {
    if !dir.is_dir() {
        return Err(Error::Unknown(name.to_owned()));
    }
    let at = dir.join("lock");
    // Read and write: where `flock` is emulated with byte-range locks, as on NFS, a shared lock
    // takes a file open for reading, and an exclusive one for writing.
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&at)
        .map_err(|e| Error::io(format!("opening {}", at.display()), e))
}

/// `path` as a volume spells it -- relative, `/`-separated, the empty path the root --
/// refused unless every component names an entry.
fn relative(path: &str) -> Result<PathBuf> {
    let mut at = PathBuf::new();
    if path.is_empty() {
        return Ok(at);
    }
    for part in path.split('/') {
        if part.is_empty() || part == "." || part == ".." || part.contains('\\') {
            return Err(Error::InvalidPath(path.to_owned()));
        }
        at.push(part);
    }
    Ok(at)
}

fn not_root(at: PathBuf, path: &str) -> Result<PathBuf> {
    match at.as_os_str().is_empty() {
        true => Err(Error::Refused(format!(
            "{path:?} is the volume's root, which this cannot be"
        ))),
        false => Ok(at),
    }
}

/// Copy `from` to `to`, a directory with everything in it but links.
fn copy_tree(root: &Dir, from: &Path, to: &Path, is_dir: bool) -> io::Result<()> {
    if !is_dir {
        return root.copy(spelled(from), root, to).map(|_| ());
    }
    root.create_dir(to)?;
    for entry in root.read_dir(spelled(from))? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() || kind.is_file() {
            let name = entry.file_name();
            copy_tree(root, &from.join(&name), &to.join(&name), kind.is_dir())?;
        }
    }
    Ok(())
}

/// Copy `from` in `store`, a `kind`, to `to` in the volume, a directory with everything in it.
async fn copy_out(
    store: &Remote,
    from: &Path,
    kind: DirentKind,
    root: &Dir,
    to: &Path,
) -> io::Result<()> {
    let mut pending = vec![(from.to_path_buf(), to.to_path_buf(), kind)];
    while let Some((from, to, kind)) = pending.pop() {
        match kind {
            DirentKind::Dir => {
                root.create_dir(&to)?;
                for entry in store.list(&from).await? {
                    if named(&entry.name) {
                        let (from, to) = (from.join(&entry.name), to.join(&entry.name));
                        pending.push((from, to, entry.kind));
                    }
                }
            }
            DirentKind::File => {
                let mut file = root.create(&to)?;
                let mut chunk = vec![0; 1 << 20];
                let mut offset = 0;
                loop {
                    let read = store.read_at(&from, &mut chunk, offset).await?;
                    if read == 0 {
                        break;
                    }
                    file.write_all(&chunk[..read])?;
                    offset += read as u64;
                }
            }
        }
    }
    Ok(())
}

/// The entries of `at` in `store`. A listing that did not come with an entry's metadata is
/// asked for it.
async fn listed_in(store: &Remote, at: &Path) -> io::Result<Vec<Dirent>> {
    let mut listed = Vec::new();
    for entry in store.list(at).await? {
        if !named(&entry.name) {
            continue;
        }
        let entry = match entry.stat() {
            Some(_) => entry,
            None => {
                let stat = store.stat(&at.join(&entry.name)).await?;
                Dirent::with_stat(entry.name, stat)
            }
        };
        listed.push(entry);
    }
    Ok(listed)
}

/// Up to `length` bytes of `at` in `store` from `offset`, fewer only at its end.
async fn read_from(store: &Remote, at: &Path, offset: u64, length: u64) -> io::Result<Vec<u8>> {
    let mut data = vec![0; length as usize];
    let mut filled = 0;
    while filled < data.len() {
        let read = store
            .read_at(at, &mut data[filled..], offset + filled as u64)
            .await?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    data.truncate(filled);
    Ok(data)
}

/// Whether a store's entry has a name a path in the volume can hold.
fn named(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\'])
}

/// A volume's files' root, opened as [`Dir`].
///
/// cap-std opens a directory on Windows without `FILE_SHARE_DELETE`, so that it cannot be
/// renamed or removed while it is open -- which would keep [`Store::remove`] from removing a
/// volume while any [`Volume`] of it is open, as Unix lets it. Here the root is opened sharing
/// deletion too: removed under an open [`Volume`], its name goes, and what that `Volume` does
/// next fails, as on Unix.
fn open_root(at: &Path) -> io::Result<Dir> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        let file = fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(at)?;
        if !file.metadata()?.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                "not a directory",
            ));
        }
        Ok(Dir::from_std_file(file))
    }
    #[cfg(not(windows))]
    Dir::open_ambient_dir(at, cap_std::ambient_authority())
}

/// What is at `at`, or `None` for nothing there, once every directory on the way is one and
/// not a link. A link at `at` itself is nothing there that can be made either: `NotFound`.
fn target(root: &Dir, at: &Path) -> io::Result<Option<cap_std::fs::Metadata>> {
    let mut prefix = PathBuf::new();
    let mut parts = at.components().peekable();
    while let Some(part) = parts.next() {
        prefix.push(part);
        let meta = match root.symlink_metadata(&prefix) {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound && parts.peek().is_none() => {
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        if meta.is_symlink() {
            return Err(io::ErrorKind::NotFound.into());
        }
        if parts.peek().is_none() {
            return Ok(Some(meta));
        }
        if !meta.is_dir() {
            return Err(io::ErrorKind::NotADirectory.into());
        }
    }
    root.dir_metadata().map(Some)
}

/// What is at `at`, which must be there.
fn existing(root: &Dir, at: &Path) -> io::Result<cap_std::fs::Metadata> {
    target(root, at)?.ok_or_else(|| io::ErrorKind::NotFound.into())
}

/// `at` as a path to hand a [`Dir`]: the root is `.`.
fn spelled(at: &Path) -> &Path {
    match at.as_os_str().is_empty() {
        true => Path::new("."),
        false => at,
    }
}

/// `meta` as a [`Stat`]; anything but a file or a directory is not seen.
fn described(meta: &cap_std::fs::Metadata) -> io::Result<Stat> {
    let kind = match (meta.is_file(), meta.is_dir()) {
        (true, _) => DirentKind::File,
        (_, true) => DirentKind::Dir,
        _ => return Err(io::ErrorKind::NotFound.into()),
    };
    let mut stat = Stat::new(kind, meta.len());
    stat.mtime = Some(meta.modified()?.into_std());
    Ok(stat)
}

/// The bytes the files under `dir` take, links not followed.
fn size_of(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.file_type() {
            Ok(kind) if kind.is_dir() => size_of(&entry.path()),
            Ok(kind) if kind.is_file() => entry.metadata().map_or(0, |meta| meta.len()),
            _ => 0,
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(Store);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let path = std::env::temp_dir()
                .join(format!("virtx-uvm-volume-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            Scratch(Store::new(path).unwrap())
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(self.0.root());
        }
    }

    #[tokio::test]
    async fn a_volume_keeps_what_is_written_until_it_is_removed() {
        let scratch = Scratch::new("files");
        let store = &scratch.0;
        let volume = store.create("data").unwrap();
        assert!(matches!(store.create("data"), Err(Error::AlreadyExists(_))));

        volume.create_dir("a/b", true).unwrap();
        volume.write("a/b/c", b"hello", None).unwrap();
        volume.write("a/b/c", b"J", Some(0)).unwrap();
        assert_eq!(volume.read("a/b/c", 0, None).await.unwrap(), b"Jello");
        assert_eq!(volume.stat("a/b/c").await.unwrap().size, 5);
        assert_eq!(volume.size(), 5);

        volume.copy("a", "z", false).await.unwrap();
        let listed: Vec<_> = volume.read_dir("z/b").await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "c");
        assert!(matches!(
            volume.copy("a", "a/inside", false).await,
            Err(Error::Refused(_))
        ));

        assert_eq!(store.list().unwrap(), ["data"]);
        let held = volume.hold().unwrap();
        assert!(matches!(store.remove("data"), Err(Error::InUse(_))));
        drop(held);
        store.remove("data").unwrap();
        assert!(store.list().unwrap().is_empty());
        assert!(matches!(store.open("data"), Err(Error::Unknown(_))));
    }

    #[tokio::test]
    async fn files_replace_a_volume_s_unless_a_machine_holds_it() {
        let scratch = Scratch::new("replace");
        let store = &scratch.0;
        let volume = store.create("data").unwrap();
        volume.write("old", b"old", None).unwrap();
        let incoming = store.root().join("incoming");
        fs::create_dir_all(&incoming).unwrap();
        fs::write(incoming.join("new"), b"new").unwrap();

        // Alone only while nothing else holds it, and nothing holds it meanwhile.
        let held = volume.hold().unwrap();
        assert!(matches!(volume.hold_alone(), Err(Error::InUse(_))));
        assert!(matches!(
            store.replace("data", &incoming),
            Err(Error::InUse(_))
        ));
        drop(held);
        let alone = volume.hold_alone().unwrap();
        assert!(matches!(volume.hold(), Err(Error::InUse(_))));
        drop(alone);

        let replaced = store.replace("data", &incoming).unwrap();
        assert_eq!(replaced.read("new", 0, None).await.unwrap(), b"new");
        assert!(replaced.stat("old").await.is_err());
        assert!(!incoming.exists());
        assert_eq!(store.list().unwrap(), ["data"]);
    }

    #[test]
    fn a_name_or_a_path_that_leaves_the_volume_is_refused() {
        let scratch = Scratch::new("names");
        let store = &scratch.0;
        assert!(matches!(store.create("../x"), Err(Error::InvalidName(_))));
        let volume = store.create("v").unwrap();
        for path in ["../x", "/x", "a//b", "a/./b"] {
            assert!(matches!(
                volume.write(path, b"", None),
                Err(Error::InvalidPath(_))
            ));
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_link_is_not_followed() {
        let scratch = Scratch::new("links");
        let volume = scratch.0.create("v").unwrap();
        std::os::unix::fs::symlink("/", volume.files().join("out")).unwrap();

        assert!(volume.read_dir("").await.unwrap().is_empty());
        let refused = volume.read("out/etc/hosts", 0, None).await;
        assert!(
            matches!(&refused, Err(Error::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound),
            "{refused:?}"
        );
    }

    #[test]
    fn a_mount_at_a_path_a_mount_is_at_is_one_there_and_inside_one_is_refused() {
        let scratch = Scratch::new("mounts");
        let mut volume = scratch.0.create("v").unwrap();
        let notion = || MountSource::Notion {
            api_key: "k".into(),
        };
        volume.mount("notes".into(), notion()).unwrap();
        let again = volume.mount("notes".into(), notion());
        assert!(matches!(again, Err(Error::MountExists(_))), "{again:?}");
        let inside = volume.mount("notes/deeper".into(), notion());
        assert!(matches!(inside, Err(Error::Refused(_))), "{inside:?}");
    }
}
