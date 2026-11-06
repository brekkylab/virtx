//! A sandbox, and the client its requests go through.

use virtx_core::{FileRoot, Sandbox as SandboxInfo, SandboxExecArgs, SandboxJob, SandboxJobEntry};

use crate::{Call, Client, StartedJob};

/// A sandbox the server holds, as making it or asking for it answered, and the client that
/// reaches it: what is needed to run processes in it and read and write its files.
///
/// Cloning holds the same client once more. Dropping it leaves the sandbox as it is; it goes
/// with [`remove`](Self::remove), or [`Client::remove_sandbox`].
#[derive(Clone)]
pub struct Sandbox {
    client: Client,
    info: SandboxInfo,
}

impl Sandbox {
    pub(crate) fn new(client: Client, info: SandboxInfo) -> Sandbox {
        Sandbox { client, info }
    }

    /// The client its requests go through.
    pub fn client(&self) -> &Client {
        &self.client
    }

    pub fn id(&self) -> &str {
        &self.info.id
    }

    /// What the server answered about it: its image, its snapshot, whether it runs, and its
    /// ports, which only making it answers.
    pub fn info(&self) -> &SandboxInfo {
        &self.info
    }

    /// Its files, its root as its processes see it, for the client's files requests.
    pub fn root(&self) -> FileRoot {
        FileRoot::Sandbox(self.info.id.clone())
    }

    /// Start a process in it, as [`Client::exec_sandbox`] does.
    pub fn exec(&self, args: &SandboxExecArgs) -> StartedJob<SandboxJob, SandboxJobEntry> {
        self.client.exec_sandbox(&self.info.id, args)
    }

    /// The file at `path`, from `offset`, `length` bytes of it or all that is left; `path` is
    /// from its root, as `etc/hostname`.
    pub fn read_file(
        &self,
        path: impl Into<String>,
        offset: u64,
        length: Option<u64>,
    ) -> Call<Vec<u8>> {
        self.client.read_file(&self.root(), path, offset, length)
    }

    /// The whole file at `path` as text, which it has to be: UTF-8.
    pub fn read_text(&self, path: impl Into<String>) -> Call<String> {
        self.client.read_text(&self.root(), path)
    }

    /// Write `content` as the whole file at `path`, as [`Client::write_file`] does.
    pub fn write_file(
        &self,
        path: impl Into<String>,
        content: impl AsRef<[u8]>,
        if_match: Option<&str>,
    ) -> Call<()> {
        self.client
            .write_file(&self.root(), path, content, if_match)
    }

    /// Have the directory at `path`, as `mkdir -p`.
    pub fn create_dir_all(&self, path: impl Into<String>) -> Call<()> {
        self.client.create_dir_all(&self.root(), path)
    }

    /// Remove what is at `path`, as [`Client::remove_file`] does.
    pub fn remove_file(&self, path: impl Into<String>, missing_ok: bool) -> Call<()> {
        self.client.remove_file(&self.root(), path, missing_ok)
    }

    /// Stop it, end its jobs, and forget it, keeping its snapshot and volumes.
    pub fn remove(&self) -> Call<()> {
        self.client.remove_sandbox(&self.info.id)
    }
}

impl std::fmt::Debug for Sandbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Sandbox").field(&self.info).finish()
    }
}
