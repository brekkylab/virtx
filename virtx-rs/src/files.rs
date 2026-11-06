//! What a handle with files reads and writes them by: its client and its root.

use virtx_core::{DirEntry, FileRoot, Metadata};

use crate::{Call, Client};

/// The files of a [`Volume`](crate::Volume), a [`Snapshot`](crate::Snapshot) or a
/// [`Sandbox`](crate::Sandbox), each request the client's of the same name at its
/// [`root`](Self::root). A path is relative to the root, as `dir/file.txt`.
pub trait Files {
    /// The client its requests go through.
    fn client(&self) -> &Client;

    /// Whose files they are, for the client's files requests.
    fn root(&self) -> FileRoot;

    /// What is at `path`.
    fn stat_file(&self, path: impl Into<String>) -> Call<Metadata> {
        self.client().stat_file(&self.root(), path)
    }

    /// Whether anything is at `path`.
    fn exists(&self, path: impl Into<String>) -> Call<bool> {
        self.client().exists(&self.root(), path)
    }

    /// Whether a file is at `path`.
    fn is_file(&self, path: impl Into<String>) -> Call<bool> {
        self.client().is_file(&self.root(), path)
    }

    /// Whether a directory is at `path`.
    fn is_dir(&self, path: impl Into<String>) -> Call<bool> {
        self.client().is_dir(&self.root(), path)
    }

    /// The entries of the directory at `path`.
    fn read_dir(&self, path: impl Into<String>) -> Call<Vec<DirEntry>> {
        self.client().read_dir(&self.root(), path)
    }

    /// The file at `path`, from `offset`, `length` bytes of it or all that is left.
    fn read_file(
        &self,
        path: impl Into<String>,
        offset: u64,
        length: Option<u64>,
    ) -> Call<Vec<u8>> {
        self.client().read_file(&self.root(), path, offset, length)
    }

    /// The whole file at `path` as text, which it has to be: UTF-8.
    fn read_text(&self, path: impl Into<String>) -> Call<String> {
        self.client().read_text(&self.root(), path)
    }

    /// Write `content` as the whole file at `path`, as [`Client::write_file`] does.
    fn write_file(
        &self,
        path: impl Into<String>,
        content: impl AsRef<[u8]>,
        if_match: Option<&str>,
    ) -> Call<()> {
        self.client()
            .write_file(&self.root(), path, content, if_match)
    }

    /// Create the directory at `path`, whose parent must exist.
    fn create_dir(&self, path: impl Into<String>) -> Call<()> {
        self.client().create_dir(&self.root(), path)
    }

    /// Have the directory at `path`, as `mkdir -p`.
    fn create_dir_all(&self, path: impl Into<String>) -> Call<()> {
        self.client().create_dir_all(&self.root(), path)
    }

    /// Remove what is at `path`, as [`Client::remove_file`] does.
    fn remove_file(&self, path: impl Into<String>, missing_ok: bool) -> Call<()> {
        self.client().remove_file(&self.root(), path, missing_ok)
    }

    /// Move what is at `from` to `to`, as [`Client::rename_file`] does.
    fn rename_file(
        &self,
        from: impl Into<String>,
        to: impl Into<String>,
        overwrite: bool,
    ) -> Call<()> {
        self.client().rename_file(&self.root(), from, to, overwrite)
    }

    /// Copy what is at `from` to `to`, as [`Client::copy_file`] does.
    fn copy_file(
        &self,
        from: impl Into<String>,
        to: impl Into<String>,
        overwrite: bool,
    ) -> Call<()> {
        self.client().copy_file(&self.root(), from, to, overwrite)
    }
}
