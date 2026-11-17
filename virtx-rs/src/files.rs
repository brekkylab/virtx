//! What a handle with files reads and writes them by: its client and its root.

use virtx_core::{DirEntry, FileRoot, Metadata};

use crate::{Call, Client};

/// Whose files a handle's requests name, and the client they go through: implemented by this
/// crate's handles alone, so [`Files`] is too.
pub(crate) mod sealed {
    use virtx_core::FileRoot;

    use crate::Client;

    pub trait Root {
        fn client(&self) -> &Client;
        fn root(&self) -> FileRoot;
    }
}

/// The files of a [`Volume`](crate::Volume) or a [`Sandbox`](crate::Sandbox). A path is
/// relative to the handle's root, `/`-separated, as `dir/file.txt`; the empty path is the root.
/// The server refuses one that leads outside.
pub trait Files: sealed::Root {
    /// What is at `path`.
    fn stat_file(&self, path: impl Into<String>) -> Call<Metadata> {
        client(self).stat_file(&root(self), path)
    }

    /// Whether anything is at `path`.
    fn exists(&self, path: impl Into<String>) -> Call<bool> {
        client(self).exists(&root(self), path)
    }

    /// Whether a file is at `path`.
    fn is_file(&self, path: impl Into<String>) -> Call<bool> {
        client(self).is_file(&root(self), path)
    }

    /// Whether a directory is at `path`.
    fn is_dir(&self, path: impl Into<String>) -> Call<bool> {
        client(self).is_dir(&root(self), path)
    }

    /// The entries of the directory at `path`.
    fn read_dir(&self, path: impl Into<String>) -> Call<Vec<DirEntry>> {
        client(self).read_dir(&root(self), path)
    }

    /// The file at `path`, from `offset`, `length` bytes of it or all that is left.
    fn read_file(
        &self,
        path: impl Into<String>,
        offset: u64,
        length: Option<u64>,
    ) -> Call<Vec<u8>> {
        client(self).read_file(&root(self), path, offset, length)
    }

    /// The whole file at `path` as text, which it has to be: UTF-8.
    fn read_text(&self, path: impl Into<String>) -> Call<String> {
        client(self).read_text(&root(self), path)
    }

    /// Write `content` as the whole file at `path`, creating it if it does not exist. Its
    /// parent directory must exist. With `if_match`, the `etag` a [`Metadata`] of the file
    /// gave, the write goes only while the file still has it, and is refused with a `412`
    /// otherwise.
    fn write_file(
        &self,
        path: impl Into<String>,
        content: impl AsRef<[u8]>,
        if_match: Option<&str>,
    ) -> Call<()> {
        client(self).write_file(&root(self), path, content, if_match)
    }

    /// Create the directory at `path`, whose parent must exist.
    fn create_dir(&self, path: impl Into<String>) -> Call<()> {
        client(self).create_dir(&root(self), path)
    }

    /// Have the directory at `path`, as `mkdir -p`: its parents are created as needed, and one
    /// there already is left as it is. Refused with a `405` when a file is at `path`.
    fn create_dir_all(&self, path: impl Into<String>) -> Call<()> {
        client(self).create_dir_all(&root(self), path)
    }

    /// Remove what is at `path`: a directory with everything in it. With `missing_ok`, nothing
    /// being there is fine, as `rm -f`; without it, a `404`.
    fn remove_file(&self, path: impl Into<String>, missing_ok: bool) -> Call<()> {
        client(self).remove_file(&root(self), path, missing_ok)
    }

    /// Move what is at `from` to `to`, replacing what is at `to` if `overwrite`, and refused if
    /// not and something is there.
    fn rename_file(
        &self,
        from: impl Into<String>,
        to: impl Into<String>,
        overwrite: bool,
    ) -> Call<()> {
        client(self).rename_file(&root(self), from, to, overwrite)
    }

    /// Copy what is at `from` to `to`, a directory with everything in it, replacing what is at
    /// `to` as [`rename_file`](Self::rename_file) does.
    fn copy_file(
        &self,
        from: impl Into<String>,
        to: impl Into<String>,
        overwrite: bool,
    ) -> Call<()> {
        client(self).copy_file(&root(self), from, to, overwrite)
    }
}

impl<T: sealed::Root> Files for T {}

fn client<T: sealed::Root + ?Sized>(handle: &T) -> &Client {
    sealed::Root::client(handle)
}

fn root<T: sealed::Root + ?Sized>(handle: &T) -> FileRoot {
    sealed::Root::root(handle)
}
