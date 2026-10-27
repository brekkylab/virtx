//! What a remote store answers about its entries: [`Stat`], [`DirentKind`], [`Dirent`].

use std::time::SystemTime;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirentKind {
    File,
    Dir,
}

/// Metadata about a single directory entry.
///
/// Fields beyond `kind`/`size` are optional because backends expose different subsets.
#[derive(Clone, Debug)]
pub struct Stat {
    pub kind: DirentKind,

    pub size: u64,

    /// Last-modified time, if the backend reports one (S3 `LastModified`,
    /// Notion `last_edited_time`).
    pub mtime: Option<SystemTime>,

    /// Last-access time; usually `None`, since most object/document backends don't track it.
    pub atime: Option<SystemTime>,

    /// The backend's nearest timestamp to POSIX `ctime`, if any (Notion `created_time`).
    pub ctime: Option<SystemTime>,

    /// Birth time, if the backend distinguishes one (local files' `created`).
    pub created: Option<SystemTime>,

    /// Entity tag / content fingerprint, if available (S3 `ETag`).
    pub etag: Option<String>,

    /// Version id, if the backend is versioned (S3 `VersionId`).
    pub version: Option<String>,
}

impl Stat {
    /// A stat with only `kind` and `size`; every optional field `None`.
    pub fn new(kind: DirentKind, size: u64) -> Self {
        Self {
            kind,
            size,
            mtime: None,
            atime: None,
            ctime: None,
            created: None,
            etag: None,
            version: None,
        }
    }
}

/// One entry in a directory listing.
///
/// `Clone` so a store can cache a listing and hand the same entries out again.
#[derive(Clone, Debug)]
pub struct Dirent {
    pub name: String,

    pub kind: DirentKind,

    /// Full metadata, only when the listing produced it for free: object stores and document APIs
    /// return it in the listing (sparing an N+1 of `stat`s), while a local directory would pay an
    /// `lstat` per entry.
    ///
    /// Private so it cannot contradict `kind`: [`with_stat`](Self::with_stat), the only setter,
    /// takes `kind` *from* the stat.
    stat: Option<Stat>,
}

impl Dirent {
    /// An entry whose metadata the listing did not include.
    pub fn new(name: impl Into<String>, kind: DirentKind) -> Self {
        Dirent {
            name: name.into(),
            kind,
            stat: None,
        }
    }

    /// An entry the listing already knew everything about.
    pub fn with_stat(name: impl Into<String>, stat: Stat) -> Self {
        Dirent {
            name: name.into(),
            kind: stat.kind,
            stat: Some(stat),
        }
    }

    /// The metadata the listing came with, if it came with any.
    ///
    /// `None` means "not free from the listing", not "unknown"; ask
    /// [`Remote::stat`](super::Remote::stat) if it is needed.
    pub fn stat(&self) -> Option<&Stat> {
        self.stat.as_ref()
    }
}
