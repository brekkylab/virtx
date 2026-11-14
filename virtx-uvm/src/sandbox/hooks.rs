//! What a server plugs into its sandboxes: each hook is called as a sandbox does what it
//! names, and may refuse it. Every one has a default that does nothing, which a server on its
//! own -- its sandboxes' disks here and nowhere else -- leaves as it is.
//!
//! A hook runs while its sandbox starts, stops or is deleted, one of those at a time: it must
//! not start, stop or delete a sandbox itself, which would wait on it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use virtx_core::{PortSpec, SandboxPort};

use super::{Error, Sandbox};
use crate::{Digest, MachineSpec};

/// Anything a hook holds for a sandbox, until it drops: while its machine runs, or while a
/// port of it is open.
pub type Held = Box<dyn Send + Sync>;

/// What a sandbox's machine may write: its disk, named by the sandbox's id, and each volume
/// it mounts to write.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Written {
    Disk(String),
    Volume(String),
}

/// The image a sandbox is made of, before it is looked for here.
pub enum Wanted<'a> {
    /// By its tag.
    Tag(&'a str),
    /// By its digest, as the store names it.
    Image(&'a Digest),
}

pub trait Hooks: Send + Sync + 'static {
    /// The image a sandbox is about to be made of, brought here first if it is kept elsewhere.
    fn want<'a>(&'a self, wanted: Wanted<'a>) -> BoxFuture<'a, Result<(), Error>> {
        let _ = wanted;
        Box::pin(async { Ok(()) })
    }

    /// The key a port is named by, as `spec` asks for it, and its URL: `kept` for one that had
    /// a key before, on another server. Without a key, its URL is [`listen`](Hooks::listen)'s.
    fn port(&self, spec: &PortSpec, kept: Option<String>) -> (Option<String>, String) {
        let _ = (spec, kept);
        (None, String::new())
    }

    /// Where `port` of `sandbox` is answered from now on, and what answers it there, until it
    /// drops: as the sandbox first starts, or as the port is opened. `None`, and it is
    /// answered at its URL as it is.
    fn listen<'a>(
        &'a self,
        sandbox: &'a Arc<Sandbox>,
        port: &'a SandboxPort,
    ) -> BoxFuture<'a, Result<Option<(String, Held)>, Error>> {
        let _ = (sandbox, port);
        Box::pin(async { Ok(None) })
    }

    /// The disk of sandbox `id` was just made here, `size` bytes.
    fn made<'a>(&'a self, id: &'a str, size: u64) -> BoxFuture<'a, Result<(), Error>> {
        let _ = (id, size);
        Box::pin(async { Ok(()) })
    }

    /// Sandbox `id`, made again here under the id it had elsewhere: its disk taken from where
    /// it is kept, to boot now if `booted`. Whether one was; `None` where nothing is kept
    /// but here, and its disk is the one here, if there is one.
    fn adopted<'a>(
        &'a self,
        id: &'a str,
        booted: bool,
    ) -> BoxFuture<'a, Result<Option<bool>, Error>> {
        let _ = (id, booted);
        Box::pin(async { Ok(None) })
    }

    /// Sandbox `id` made again here, its disk [adopted](Hooks::adopted) and not booted: ready
    /// for it to start at once when it is next asked to.
    fn readied(&self, id: &str) {
        let _ = id;
    }

    /// `sandbox`'s machine about to boot as `spec`, before its disk is held: what it holds
    /// while it runs.
    fn booting<'a>(
        &'a self,
        sandbox: &'a Sandbox,
        spec: &'a MachineSpec,
    ) -> BoxFuture<'a, Result<Vec<Held>, Error>> {
        let _ = (sandbox, spec);
        Box::pin(async { Ok(Vec::new()) })
    }

    /// What a machine may write, `writes`, changed from now on: as it boots, which a refusal
    /// stops, and as it stops, before its machine has ended, which is refused nothing.
    fn changed(&self, writes: &[Written]) -> Result<(), Error> {
        let _ = writes;
        Ok(())
    }

    /// What sandbox `id`'s disk, at `disk`, is fetched from while its machine runs, where the
    /// server's disks fetch it as it is read ([`Disks::open`](crate::disks::Disks::open)): a
    /// socket, and what answers on it until it drops. `None` for a disk that is all here.
    fn chunks(&self, id: &str, disk: &Path) -> Result<Option<(PathBuf, Held)>, Error> {
        let _ = (id, disk);
        Ok(None)
    }

    /// Sandbox `id`'s machine booted, in `took`, its disk fetched as it is read if `lazily`
    /// ([`chunks`](Hooks::chunks)).
    fn booted(&self, id: &str, took: Duration, lazily: bool) {
        let _ = (id, took, lazily);
    }

    /// What sandbox `id`'s machine logged, at `level`.
    fn said(&self, id: &str, level: &str, line: &str) {
        let _ = (id, level, line);
    }

    /// Sandbox `id`'s disk of `disk`'s path, its own or a copy of it, made all here if part of
    /// it is fetched as it is read: for a reader other than its machine.
    fn materialize<'a>(&'a self, id: &'a str, disk: &'a Path) -> BoxFuture<'a, Result<(), Error>> {
        let _ = (id, disk);
        Box::pin(async { Ok(()) })
    }

    /// Sandbox `id`'s disk, stopped, brought up to what is kept, to be read.
    fn reading<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), Error>> {
        let _ = id;
        Box::pin(async { Ok(()) })
    }

    /// Sandbox `id` about to be deleted, its machine stopped: its disk removed from wherever
    /// else it is kept, before it is here.
    fn deleting<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), Error>> {
        let _ = id;
        Box::pin(async { Ok(()) })
    }
}

/// A server's that plugs nothing in.
pub struct NoHooks;

impl Hooks for NoHooks {}
