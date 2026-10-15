//! The public end: a console server to run commands in, over one channel.
//!
//! [`ConsoleClientBuilder`] describes the session; [`ConsoleClient`] runs commands in it.
//! [`Relay`] drives one session from a second place, such as another native extension.

use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    pin::Pin,
    task::{Context, Poll},
};

use anyhow::Context as _;
use futures_core::{Stream, future::BoxFuture};
use tokio::process::Command;

mod relay;

pub use relay::Relay;

use crate::{
    cache_root,
    fs::Mount,
    image::ImageSource,
    protocol::{
        Call, Client, ExecCall, ExecResp, Failure, InitCall, JobId, KillCall, MountSpec,
        Notification, PollCall, PollResp, Port, ReadCall, ReadResp, Response, WriteCall, WriteResp,
        stdio::StdioClient,
    },
};

/// Whatever it takes to have a channel, deferred until [`build`](ConsoleClientBuilder::build).
///
/// [`Send`] so a builder can be built inside a spawned task.
pub(crate) type ClientFactory = Box<dyn FnOnce() -> anyhow::Result<Box<dyn Client>> + Send>;

/// Assembles a [`ConsoleClient`] from the parts it needs.
///
/// Every part is optional. Nothing starts until [`build`](Self::build).
///
/// The machine's shape ([`vcpus`](Self::vcpus), [`memory_mib`](Self::memory_mib),
/// [`gpu`](Self::gpu), [`gpu_memory_mib`](Self::gpu_memory_mib), [`disk_gib`](Self::disk_gib))
/// is given exactly or refused: one the server cannot give fails [`build`](Self::build) with
/// [`UNSUPPORTED_MACHINE`](crate::protocol::Error::UNSUPPORTED_MACHINE), never a smaller
/// machine or the CPU. Left out, the server picks.
pub struct ConsoleClientBuilder {
    /// A factory, not a client, because starting a program can fail; deferring that to
    /// [`build`](Self::build) keeps every setter infallible.
    ///
    /// Defaults to `virtx-uvm` under the cache's `bin` directory.
    client_factory: ClientFactory,

    /// `None` leaves it to the server: a host backend needs none, and a VM backend refuses
    /// rather than guess a base.
    image: Option<ImageSource>,

    /// What a previous session wrote; `None` starts from scratch.
    snapshot: Option<Vec<u8>>,

    /// Every tree this console gives the session, in mount order. Empty means commands see
    /// only the server's own filesystem.
    mounts: Vec<Mounted>,

    /// On unless turned off.
    network: bool,

    /// Ports on the server's machine that lead into the session, in the order named.
    ports: Vec<Port>,

    vcpus: Option<u8>,

    memory_mib: Option<u32>,

    gpu: Option<bool>,

    gpu_memory_mib: Option<u32>,

    disk_gib: Option<u32>,
}

impl Default for ConsoleClientBuilder {
    fn default() -> Self {
        ConsoleClientBuilder {
            client_factory: stdio_factory(&[cache_root().join("bin").join("virtx-uvm")]),
            image: None,
            snapshot: None,
            mounts: Vec::new(),
            network: true,
            ports: Vec::new(),
            vcpus: None,
            memory_mib: None,
            gpu: None,
            gpu_memory_mib: None,
            disk_gib: None,
        }
    }
}

impl ConsoleClientBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drive the server over `client`.
    ///
    /// Any [`Client`]: a [`StdioClient`] over a server it
    /// started, a virtio port into a guest, or both ends in one process for a test. The
    /// client owns whatever the channel needs, including a process.
    pub fn client(mut self, client: impl Client + 'static) -> Self {
        self.client_factory = Box::new(move || Ok(Box::new(client)));
        self
    }

    /// Drive a server this console starts itself: `cmd`, over its own pipes.
    ///
    /// `cmd` is a program and its arguments: `["virtx-local-console"]`,
    /// `["sh", "-c", "…"]`. For an environment or directory, build the [`Command`], pass it
    /// to [`StdioClient::new`], and hand the result to [`client`](Self::client).
    ///
    /// Started by [`build`](Self::build), which fails if it cannot be (including an empty
    /// `cmd`).
    pub fn cmd(mut self, cmd: &[impl AsRef<OsStr>]) -> Self {
        self.client_factory = stdio_factory(cmd);
        self
    }

    /// Give the session a tree, mounted at `at`.
    ///
    /// `at` is where commands see the tree, and the path [`read`](ConsoleClient::read) and
    /// [`write`](ConsoleClient::write) join onto. It must be absolute, and the mount point must
    /// be expressible as a URL (see [`Mount::url`]), or [`build`](Self::build) fails.
    ///
    /// The mount is held by value so the tree stays up for the whole session (see
    /// [`Mount`]); pass an `Arc<..>` to share it. A plain [`PathBuf`] is a [`Mount`] for an
    /// existing host directory, with nothing to put up or take down.
    ///
    /// **Order is kept**: mount a nested tree after the tree containing it.
    ///
    /// Writable; see [`mount_readonly`](Self::mount_readonly).
    pub fn mount(mut self, mount: impl Mount + 'static, at: impl Into<PathBuf>) -> Self {
        self.mounts.push(Mounted {
            mount: Box::new(mount),
            at: at.into(),
            readonly: false,
        });
        self
    }

    /// Give the session a tree it may read and not write, at `at`.
    ///
    /// Otherwise as [`mount`](Self::mount). A [`write`](ConsoleClient::write) under it is
    /// refused, and a VM backend mounts it read-only so commands cannot write there either.
    pub fn mount_readonly(mut self, mount: impl Mount + 'static, at: impl Into<PathBuf>) -> Self {
        self.mounts.push(Mounted {
            mount: Box::new(mount),
            at: at.into(),
            readonly: true,
        });
        self
    }

    /// The base the session's commands run in.
    ///
    /// ```no_run
    /// # use virtx::console::ConsoleClient;
    /// # use virtx::image::Recipe;
    /// # async fn f() -> anyhow::Result<()> {
    /// let console = ConsoleClient::builder()
    ///     .image(
    ///         Recipe::new("alpine:3.20")
    ///             .step("apk add --no-cache jq"),
    ///     )
    ///     .build()
    ///     .await?;
    /// # Ok(()) }
    /// ```
    pub fn image(mut self, image: impl Into<ImageSource>) -> Self {
        self.image = Some(image.into());
        self
    }

    /// Start this session on what a previous one wrote, rather than from scratch.
    pub fn snapshot(mut self, snapshot: Vec<u8>) -> Self {
        self.snapshot = Some(snapshot);
        self
    }

    /// Whether the session's commands reach a network at all; on unless turned off.
    ///
    /// ```no_run
    /// # use virtx::console::ConsoleClient;
    /// # async fn f() -> anyhow::Result<()> {
    /// let console = ConsoleClient::builder().network(true).build().await?;
    /// # Ok(()) }
    /// ```
    ///
    /// **The server gives exactly this or refuses:** [`build`](Self::build) fails with
    /// [`UNSUPPORTED_NETWORK`](crate::protocol::Error::UNSUPPORTED_NETWORK). A host backend
    /// cannot take the network away, so it refuses `false`. What "on" reaches is on
    /// [`InitCall::network`](crate::protocol::InitCall::network).
    pub fn network(mut self, network: bool) -> Self {
        self.network = network;
        self
    }

    /// Ports on the server's machine that lead into the session, as docker's `-p` spells
    /// them (`"8080:80"`, host first). Replaces what an earlier call said.
    ///
    /// ```no_run
    /// # use virtx::console::ConsoleClient;
    /// # async fn f() -> anyhow::Result<()> {
    /// // A VNC server in the session, at 127.0.0.1:5901 here.
    /// let console = ConsoleClient::builder()
    ///     .network(true)
    ///     .ports(["5901:5900".parse()?])
    ///     .build()
    ///     .await?;
    /// # Ok(()) }
    /// ```
    ///
    /// Ports with [`network`](Self::network) off are refused. See [`Port`] for what a
    /// connection to one reaches.
    pub fn ports(mut self, ports: impl IntoIterator<Item = Port>) -> Self {
        self.ports = ports.into_iter().collect();
        self
    }

    /// How many vCPUs the session's machine gets.
    pub fn vcpus(mut self, vcpus: u8) -> Self {
        self.vcpus = Some(vcpus);
        self
    }

    /// How much memory the session's machine gets, in mebibytes.
    ///
    /// ```no_run
    /// # use virtx::console::ConsoleClient;
    /// # async fn f() -> anyhow::Result<()> {
    /// let console = ConsoleClient::builder()
    ///     .vcpus(4)
    ///     .memory_mib(4096)
    ///     .build()
    ///     .await?;
    /// # Ok(()) }
    /// ```
    pub fn memory_mib(mut self, memory_mib: u32) -> Self {
        self.memory_mib = Some(memory_mib);
        self
    }

    /// Whether the session's commands get a GPU; `false` forbids one.
    ///
    /// **The image must bring the guest's half of Vulkan**: a loader (`libvulkan.so.1`) and
    /// the venus driver that reaches the host's GPU. The server attaches the device but cannot
    /// install libraries; without the loader nothing finds a device, and without venus Vulkan
    /// falls back to a CPU renderer.
    ///
    /// | | Alpine | Debian |
    /// |---|---|---|
    /// | loader | `vulkan-loader` | `libvulkan1` |
    /// | venus driver | `mesa-vulkan-virtio` | `mesa-vulkan-drivers`, trixie or later |
    ///
    /// ```no_run
    /// # use virtx::{console::ConsoleClient, image::Recipe};
    /// # async fn f() -> anyhow::Result<()> {
    /// let console = ConsoleClient::builder()
    ///     .image(
    ///         Recipe::new("alpine:latest")
    ///             .step("apk add --no-cache vulkan-loader mesa-vulkan-virtio vulkan-tools"),
    ///     )
    ///     .gpu(true)
    ///     .build()
    ///     .await?;
    /// # Ok(()) }
    /// ```
    pub fn gpu(mut self, gpu: bool) -> Self {
        self.gpu = Some(gpu);
        self
    }

    /// How much memory the session's GPU may hold, in MiB, in addition to
    /// [`memory_mib`](Self::memory_mib).
    ///
    /// Commands see a device of this size. Only valid with a GPU (see
    /// [`InitCall::gpu_memory_mib`](crate::console::InitCall::gpu_memory_mib)).
    ///
    /// ```no_run
    /// # use virtx::{console::ConsoleClient, image::Recipe};
    /// # async fn f() -> anyhow::Result<()> {
    /// let console = ConsoleClient::builder()
    ///     // A loader and the venus driver -- see `gpu`.
    ///     .image(
    ///         Recipe::new("alpine:latest").step("apk add --no-cache vulkan-loader mesa-vulkan-virtio"),
    ///     )
    ///     .gpu(true)
    ///     .gpu_memory_mib(8192)
    ///     .build()
    ///     .await?;
    /// # Ok(()) }
    /// ```
    pub fn gpu_memory_mib(mut self, gpu_memory_mib: u32) -> Self {
        self.gpu_memory_mib = Some(gpu_memory_mib);
        self
    }

    /// How much the session's commands may write, in GiB, on top of what the image ships.
    ///
    /// A ceiling, not an allocation (see
    /// [`InitCall::disk_gib`](crate::console::InitCall::disk_gib)).
    ///
    /// ```no_run
    /// # use virtx::console::ConsoleClient;
    /// # async fn f() -> anyhow::Result<()> {
    /// let console = ConsoleClient::builder()
    ///     .disk_gib(32)
    ///     .build()
    ///     .await?;
    /// # Ok(()) }
    /// ```
    pub fn disk_gib(mut self, disk_gib: u32) -> Self {
        self.disk_gib = Some(disk_gib);
        self
    }

    /// Opens the channel (starting the server process over stdio) and sends `init`.
    ///
    /// Panics outside a Tokio runtime when it starts a process (the default, or
    /// [`cmd`](Self::cmd)), since the runtime reaps the child.
    ///
    /// The server may have to build an [`image`](Self::image) before answering `init`.
    pub async fn build(self) -> anyhow::Result<ConsoleClient> {
        ConsoleClient::new(self).await
    }
}

/// One of a session's trees: the mount this end holds, and where the session sees it.
struct Tree {
    /// Never read: holding it keeps the tree up for the session's lifetime (see [`Mount`]).
    #[allow(dead_code)]
    mount: Box<dyn Mount>,

    /// Where the session sees it, as `init` named it; not necessarily the host mount point.
    path: PathBuf,
}

/// A console: something to run commands in.
///
/// An [`exec`](Self::exec) per command, and drop it to end the session, or
/// [`close`](Self::close) it to wait until the session is over. The session was described at
/// build, booting happens under the first command that needs it, and everything goes away
/// with the console. [`start`](Self::start) and [`stop`](Self::stop)
/// are optional resource management.
///
/// Every method that reaches the server is one message, [`exec`](Self::exec) included; only
/// [`start`](Self::start) and [`stop`](Self::stop) go unanswered. One console is not
/// concurrent: its methods take `&mut self` (see [`Client`]). A command that should run while
/// the console does other things is [`spawn`](Self::spawn)ed instead, and its output
/// [`poll`](Self::poll)ed; [`exec_stream`](Self::exec_stream) does both for a command whose
/// output is wanted as it is written.
///
/// ```no_run
/// use virtx::console::ConsoleClient;
///
/// # #[tokio::main]
/// # async fn main() -> anyhow::Result<()> {
/// // Starts the server and sends `init`; nothing boots yet.
/// let mut console = ConsoleClient::builder()
///     .build()
///     .await?;
///
/// let result = console.exec(["sh", "-c", "echo hi"], None).await?;
/// assert_eq!(result.stdout, b"hi\n");
/// # Ok(())
/// # }
/// ```
pub struct ConsoleClient {
    /// Always present; ending replaces it rather than emptying it — see
    /// [`ConsoleClient::drop`](ConsoleClient#impl-Drop-for-ConsoleClient).
    client: Box<dyn Client>,

    /// Every tree this session was given, in the order named.
    mounts: Vec<Tree>,

    /// Jobs whose [`ExecStream`] was dropped before their end, killed before the next call
    /// goes out: a drop cannot wait for an answer.
    abandoned: Vec<JobId>,
}

impl ConsoleClient {
    pub fn builder() -> ConsoleClientBuilder {
        ConsoleClientBuilder::default()
    }

    /// Open a channel and send `init` on it.
    ///
    /// `init` is sent here, not by the caller, because there is no useful console between
    /// having a channel and describing the session: a `ConsoleClient` that exists is one the
    /// server has answered. Boots and mounts nothing.
    ///
    /// On failure the client is dropped without `quit`; over stdio the server sees its
    /// input close and exits.
    pub async fn new(builder: ConsoleClientBuilder) -> anyhow::Result<Self> {
        let ConsoleClientBuilder {
            client_factory,
            image,
            mounts,
            network,
            ports,
            snapshot,
            vcpus,
            memory_mib,
            gpu,
            gpu_memory_mib,
            disk_gib,
        } = builder;

        let mut client = client_factory()?;

        // Name every tree before sending anything: an unnameable mount is this end's own
        // mistake, not something to hear back from a server.
        let mut specs = Vec::with_capacity(mounts.len());
        let mut held = Vec::with_capacity(mounts.len());
        for mounted in mounts {
            specs.push(named(&mounted)?);
            held.push(Tree {
                mount: mounted.mount,
                path: mounted.at,
            });
        }

        let session = InitCall {
            mounts: specs,
            image,
            network,
            ports,
            snapshot,
            vcpus,
            memory_mib,
            gpu,
            gpu_memory_mib,
            disk_gib,
        };

        client.init(session).await?;

        Ok(ConsoleClient {
            client,
            mounts: held,
            abandoned: Vec::new(),
        })
    }

    /// Where each of this session's trees appears, in the order named.
    ///
    /// Join [`read`](Self::read) and [`write`](Self::write) paths onto these, not onto the
    /// host mount point, which differs on a guest backend. Empty when nothing is mounted.
    pub fn mounts(&self) -> impl ExactSizeIterator<Item = &Path> {
        self.mounts.iter().map(|tree| tree.path.as_path())
    }

    /// Boot the far end now, to hide the cold start.
    ///
    /// Optional: [`exec`](Self::exec), [`read`](Self::read) and [`write`](Self::write) boot
    /// on demand. Sending this early lets the boot overlap with the caller's other work.
    ///
    /// `Ok` means the message went out, not that the boot succeeded; a failed boot surfaces
    /// as [`BOOT_FAILED`](crate::protocol::Error::BOOT_FAILED) on the next call that needs one.
    pub async fn start(&mut self) -> Result<(), Failure> {
        self.client.start().await
    }

    /// Release what booting took (guest memory, socket, scratch directory) while idle.
    ///
    /// Not an ending and not owed: dropping the console releases everything. The next
    /// command pays a cold start again, so use it before long idle stretches only.
    ///
    /// Every job still running ends with it.
    ///
    /// `Ok` means the message went out; nothing answers it.
    pub async fn stop(&mut self) -> Result<(), Failure> {
        self.client.stop().await?;
        self.abandoned.clear();
        Ok(())
    }

    /// Run one command, and return everything it produced.
    ///
    /// The command is an argv (`["echo", "hi"]`) with no shell; for shell semantics send
    /// `["sh", "-c", ".."]`.
    ///
    /// It runs in the session's current directory, kept by the server; to run elsewhere,
    /// say so in the command (`sh -c 'cd there && ..'`).
    ///
    /// `timeout_ms` is the only way to bound it; `None` may run forever. A command killed at
    /// it is [`TIMED_OUT`](crate::protocol::Error::TIMED_OUT), with what it wrote until then in
    /// [`Error::output`](crate::protocol::Error::output). Dropping this future instead leaves the
    /// command running and its answer owed: the server answers one call at a time, so the
    /// next call waits for the command to end and its late answer is skipped. A command that
    /// may need stopping is [`spawn`](Self::spawn)ed, and its job killed.
    pub async fn exec(
        &mut self,
        cmd: impl IntoIterator<Item = impl AsRef<str>>,
        timeout_ms: Option<u64>,
    ) -> Result<ExecResp, Failure> {
        self.settle().await?;
        self.client.exec(argv(cmd, timeout_ms)).await
    }

    /// Start one command and return its job at once, leaving it running.
    ///
    /// The command is as for [`exec`](Self::exec), and so is `timeout_ms`. Its output waits on
    /// the server until [`poll`](Self::poll)ed, and the console takes any other call
    /// meanwhile, another `spawn` included.
    ///
    /// The server holds the job until the poll that reports its end, so poll every job
    /// spawned to its end (or [`kill`](Self::kill) it first). [`stop`](Self::stop) ends them
    /// all.
    pub async fn spawn(
        &mut self,
        cmd: impl IntoIterator<Item = impl AsRef<str>>,
        timeout_ms: Option<u64>,
    ) -> Result<JobId, Failure> {
        self.settle().await?;
        Ok(self.client.spawn(argv(cmd, timeout_ms)).await?.job)
    }

    /// What `job` wrote since the last poll, and its [`code`](PollResp::code) once it has
    /// ended; the job is gone after that answer.
    ///
    /// `wait_ms` waits that long for new output or the end when there is none yet, rather
    /// than answering empty at once. `max_bytes` bounds each stream, leaving the rest for the
    /// next poll.
    ///
    /// A job killed at its `timeout_ms` ends in [`TIMED_OUT`](crate::protocol::Error::TIMED_OUT) instead, with
    /// these bytes in [`Error::output`](crate::protocol::Error::output).
    pub async fn poll(
        &mut self,
        job: JobId,
        wait_ms: Option<u64>,
        max_bytes: Option<u64>,
    ) -> Result<PollResp, Failure> {
        self.settle().await?;
        let poll = PollCall {
            job,
            wait_ms,
            max_bytes,
        };
        self.client.poll(poll).await
    }

    /// Send `signal` (`None` for `SIGKILL`) to `job` and everything it started.
    ///
    /// The next [`poll`](Self::poll) reports how it ended.
    pub async fn kill(&mut self, job: JobId, signal: Option<i32>) -> Result<(), Failure> {
        self.settle().await?;
        self.client.kill(KillCall { job, signal }).await?;
        Ok(())
    }

    /// Run one command and yield its output as it is written.
    ///
    /// [`spawn`](Self::spawn) and then [`poll`](Self::poll) until it ends: each item is what
    /// arrived since the last, and the last carries the [`code`](PollResp::code). A command
    /// killed at `timeout_ms` ends the stream with [`TIMED_OUT`](crate::protocol::Error::TIMED_OUT) and its last
    /// bytes in [`Error::output`](crate::protocol::Error::output).
    ///
    /// The stream holds the console. Dropping it before the job's end, between items or while
    /// one is awaited, kills the job before this console's next call; an answer that was on its
    /// way is skipped when it arrives.
    pub async fn exec_stream(
        &mut self,
        cmd: impl IntoIterator<Item = impl AsRef<str>>,
        timeout_ms: Option<u64>,
    ) -> Result<ExecStream<'_>, Failure> {
        let job = self.spawn(cmd, timeout_ms).await?;
        Ok(ExecStream {
            job,
            state: Streaming::Idle(self),
        })
    }

    /// Kill `job` and collect it, so the server no longer holds it: what to do with a job
    /// whose output nobody will read.
    ///
    /// Collecting is bounded, since a process that left the job's group can hold its output
    /// open past the kill; such a job is left to the server. A job already gone is not an
    /// error. `Err` is only a broken channel.
    pub async fn discard(&mut self, job: JobId) -> Result<(), Failure> {
        match self.client.kill(KillCall { job, signal: None }).await {
            Ok(_) => {}
            // Already gone, or a refusal that will not change by asking again.
            Err(Failure::Refused(_)) => return Ok(()),
            Err(broken) => return Err(broken),
        }
        for _ in 0..DISCARD_POLLS {
            let poll = PollCall {
                job,
                wait_ms: Some(STREAM_WAIT_MS),
                max_bytes: None,
            };
            match self.client.poll(poll).await {
                Ok(chunk) if chunk.code.is_none() => {}
                Ok(_) | Err(Failure::Refused(_)) => break,
                Err(broken) => return Err(broken),
            }
        }
        Ok(())
    }

    /// [`discard`](Self::discard) every job whose stream was dropped, before the call that
    /// comes next.
    pub(crate) async fn settle(&mut self) -> Result<(), Failure> {
        while let Some(&job) = self.abandoned.last() {
            self.discard(job).await?;
            self.abandoned.pop();
        }
        Ok(())
    }

    /// Read part of a file where commands run.
    ///
    /// The path is in the session's filesystem, under one of [`mounts`](Self::mounts): the
    /// file a command would open by that name.
    ///
    /// `offset` defaults to the beginning and `len` to the rest. A file too large for one
    /// message arrives in pieces; compare [`ReadResp::size`] with what arrived.
    pub async fn read(
        &mut self,
        path: impl AsRef<str>,
        offset: Option<u64>,
        len: Option<u64>,
    ) -> Result<ReadResp, Failure> {
        self.settle().await?;
        let read = ReadCall {
            path: path.as_ref().to_string(),
            offset,
            len,
        };
        self.client.read(read).await
    }

    /// Take everything this session has written, as a blob another session can start on.
    ///
    /// Pass the bytes to [`ConsoleClientBuilder::snapshot`] to carry the session on: a
    /// layer tar of the session's writes, kept as bytes by the caller.
    ///
    /// Cumulative since the session began, not since the last snapshot.
    pub async fn snapshot(&mut self) -> Result<Vec<u8>, Failure> {
        self.settle().await?;
        self.client.snapshot().await.map(|answer| answer.blob)
    }

    /// Put bytes in a file where commands run; returns its size afterwards.
    ///
    /// The path is under one of [`mounts`](Self::mounts), as for [`read`](Self::read).
    ///
    /// `None` makes the file exactly `data` (created or truncated); `Some(0)` writes the
    /// same bytes but keeps whatever lay past them.
    pub async fn write(
        &mut self,
        path: impl AsRef<str>,
        data: impl Into<Vec<u8>>,
        offset: Option<u64>,
    ) -> Result<WriteResp, Failure> {
        self.settle().await?;
        let write = WriteCall {
            path: path.as_ref().to_string(),
            data: data.into(),
            offset,
        };
        self.client.write(write).await
    }

    /// End the session and return once it is over: `quit` said, the server gone, and this
    /// console's mounts let go of.
    ///
    /// Dropping also ends the session, but on a task nothing waits for, so a program exiting
    /// right after may leave its mounts up (on Windows, a Dokan mount point stays a junction
    /// to a gone volume).
    ///
    /// Mounts are let go of only after the server exits, since it may still be serving them;
    /// one that something else holds stays up. Returns how the server ended (over stdio, its
    /// exit status); the mounts are let go of either way.
    ///
    /// Bounded over stdio: a server still running
    /// [`QUIT_GRACE`](crate::protocol::stdio::QUIT_GRACE) after `quit` is killed, so a server
    /// that hangs on its way out cannot keep this from returning.
    pub async fn close(mut self) -> Result<(), Failure> {
        // Leave a spent client and no mounts behind, so dropping `self` has nothing to end.
        let mut client = std::mem::replace(&mut self.client, Box::new(Spent));
        let mounts = std::mem::take(&mut self.mounts);
        drop(self);

        let ended = client.quit().await;
        drop(client);
        // Dropping a mount's guard blocks until it is unmounted, so not on this task's thread.
        let _ = tokio::task::spawn_blocking(move || drop(mounts)).await;
        ended
    }
}

impl Drop for ConsoleClient {
    /// Say `quit`, so the server exits gracefully; it also releases what a
    /// [`stop`](ConsoleClient::stop) would, so no `stop` is needed. The outcome (over stdio,
    /// the exit status) is discarded, since no caller is left to act on it.
    ///
    /// Owed exactly once, when the console goes away, so it is a `Drop` rather than a
    /// method to remember.
    ///
    /// The mounts go after the client: the server serves them until it hears the ending (a
    /// guest's virtio-fs share holds files open), and a binding's guard unmounts then waits
    /// for every holder, which dropped here would block the task's thread, deadlocking a
    /// current-thread runtime.
    fn drop(&mut self) {
        hang_up(&mut self.client, std::mem::take(&mut self.mounts));
    }
}

/// How long one poll of an [`ExecStream`] waits for output. It answers as soon as any
/// arrives, so this bounds only how often a silent command costs a round trip.
const STREAM_WAIT_MS: u64 = 1000;

/// How many polls, of [`STREAM_WAIT_MS`] each, a killed job is given to end before
/// [`ConsoleClient::discard`] leaves it to the server.
const DISCARD_POLLS: usize = 5;

/// An argv and a timeout, as the `params` of `exec` and `spawn`.
fn argv(cmd: impl IntoIterator<Item = impl AsRef<str>>, timeout_ms: Option<u64>) -> ExecCall {
    ExecCall {
        cmd: cmd.into_iter().map(|s| s.as_ref().to_string()).collect(),
        timeout_ms,
    }
}

/// A command's output as it is written: [`ConsoleClient::exec_stream`].
///
/// A [`Stream`] of what each poll brought, skipping polls that brought nothing; the last item
/// carries the [`code`](PollResp::code), or is the refusal that ended the job. For use
/// without a `StreamExt` in scope, [`next`](Self::next) is the same.
pub struct ExecStream<'a> {
    job: JobId,
    state: Streaming<'a>,
}

/// Where an [`ExecStream`] is: holding the console between polls, lending it to one, or done.
enum Streaming<'a> {
    Idle(&'a mut ConsoleClient),
    /// The poll owns the console while it is out, and hands it back with the answer.
    Polling(BoxFuture<'a, (&'a mut ConsoleClient, Result<PollResp, Failure>)>),
    Done,
}

/// The console as one poll of an [`ExecStream`] holds it. A poll dropped before its answer
/// leaves the job to be killed before the console's next call, as a stream dropped between
/// items does.
struct Lent<'a> {
    console: Option<&'a mut ConsoleClient>,
    job: JobId,
}

impl<'a> Lent<'a> {
    /// The console back, answered: nothing is left to kill.
    fn give_back(mut self) -> &'a mut ConsoleClient {
        self.console
            .take()
            .expect("a lent console is given back once")
    }
}

impl Drop for Lent<'_> {
    fn drop(&mut self) {
        if let Some(console) = self.console.take() {
            console.abandoned.push(self.job);
        }
    }
}

impl ExecStream<'_> {
    /// The job this stream is following.
    pub fn job(&self) -> JobId {
        self.job
    }

    /// The next piece of output; `None` once the job has ended.
    pub async fn next(&mut self) -> Option<Result<PollResp, Failure>> {
        std::future::poll_fn(|cx| Pin::new(&mut *self).poll_next(cx)).await
    }
}

impl Stream for ExecStream<'_> {
    type Item = Result<PollResp, Failure>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let job = self.job;
        loop {
            match std::mem::replace(&mut self.state, Streaming::Done) {
                Streaming::Done => return Poll::Ready(None),
                Streaming::Idle(console) => {
                    let mut lent = Lent {
                        console: Some(console),
                        job,
                    };
                    self.state = Streaming::Polling(Box::pin(async move {
                        let poll = PollCall {
                            job,
                            wait_ms: Some(STREAM_WAIT_MS),
                            max_bytes: None,
                        };
                        let console = lent.console.as_deref_mut().expect("lent until answered");
                        let answer = console.client.poll(poll).await;
                        (lent.give_back(), answer)
                    }));
                }
                Streaming::Polling(mut polling) => {
                    let (console, answer) = match polling.as_mut().poll(cx) {
                        Poll::Pending => {
                            self.state = Streaming::Polling(polling);
                            return Poll::Pending;
                        }
                        Poll::Ready(ready) => ready,
                    };
                    match answer {
                        Ok(chunk) if chunk.is_empty() => self.state = Streaming::Idle(console),
                        Ok(chunk) => {
                            if chunk.code.is_none() {
                                self.state = Streaming::Idle(console);
                            }
                            return Poll::Ready(Some(Ok(chunk)));
                        }
                        // A refusal is the job's end (a timeout) or its absence; either way
                        // there is nothing more to poll.
                        Err(failure) => return Poll::Ready(Some(Err(failure))),
                    }
                }
            }
        }
    }
}

impl Drop for ExecStream<'_> {
    /// A job still running when its stream goes is killed before the console's next call. A
    /// poll still out does this through the console it was lent.
    fn drop(&mut self) {
        if let Streaming::Idle(console) = &mut self.state {
            console.abandoned.push(self.job);
        }
    }
}

/// Start `cmd` as a console server, over its own pipes, once the factory is run.
pub(crate) fn stdio_factory(cmd: &[impl AsRef<OsStr>]) -> ClientFactory {
    // Owned, since the closure outlives this borrow.
    let cmd: Vec<OsString> = cmd.iter().map(|s| s.as_ref().to_owned()).collect();

    Box::new(move || {
        let (program, args) = cmd
            .split_first()
            .context("a console server needs a program to run")?;

        let mut server = Command::new(program);
        server.args(args);

        let client = StdioClient::new(server).context("starting the console server")?;
        Ok(Box::new(client))
    })
}

/// Stands in for a client once the session has ended.
struct Spent;

impl Client for Spent {
    fn call(&mut self, _: Call) -> BoxFuture<'_, Result<Response, Failure>> {
        Box::pin(async { Err(Failure::broken("the session has ended")) })
    }

    fn notify(&mut self, _: Notification) -> BoxFuture<'_, Result<(), Failure>> {
        Box::pin(async { Err(Failure::broken("the session has ended")) })
    }
}

/// Say `quit` on a task, leaving a client behind that answers nothing, and drop `keep`
/// only after the client.
///
/// Swapping (not taking) lets a type with a destructor hand its client to a task, and keeps
/// the field a plain `Box` rather than an `Option` every method must check. Off a runtime,
/// or if the task never runs, the client is dropped without `quit`, closing the server's input.
///
/// `keep` is what the server may still be using (a console's mounts).
pub(crate) fn hang_up<K: Send + 'static>(client: &mut Box<dyn Client>, keep: K) {
    // Zero-sized, so boxing it allocates nothing.
    let client = std::mem::replace(client, Box::new(Spent));

    // A tuple drops its fields in order, so however it is dropped (off a runtime, as an
    // unpolled task, or at the task's end) the client goes before what it was serving.
    let ending = (client, keep);
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        runtime.spawn(async move {
            let mut ending = ending;
            let _ = ending.0.quit().await;
        });
    }
}

/// A tree the caller handed over, held by the builder until `init`.
struct Mounted {
    mount: Box<dyn Mount>,
    at: PathBuf,
    readonly: bool,
}

/// A mount this end holds, as the spec string the server is told to realize.
fn named(mounted: &Mounted) -> anyhow::Result<MountSpec> {
    let url = mounted.mount.url().with_context(|| {
        format!(
            "a mount point that is not an absolute UTF-8 path cannot be named: {:?}",
            mounted.mount.mountpoint()
        )
    })?;

    let at = mounted.at.to_str().with_context(|| {
        format!(
            "a tree can only be mounted at a UTF-8 path: {:?}",
            mounted.at
        )
    })?;

    // The error already names the spec.
    let spec = MountSpec::new(url, at)?;

    Ok(if mounted.readonly {
        spec.read_only()
    } else {
        spec
    })
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use futures_core::future::BoxFuture;

    use super::*;
    use crate::protocol::{Call, Error, InitResp, Method, Notification, Response};

    /// What a [`Recorder`] was handed, readable while it is lent out.
    ///
    /// `methods` is every message in order, notifications included; `calls` holds only the
    /// calls' contents.
    #[derive(Clone)]
    struct Log {
        methods: Arc<Mutex<Vec<Method>>>,
        calls: Arc<Mutex<Vec<Call>>>,
    }

    impl Log {
        fn methods(&self) -> Vec<Method> {
            self.methods.lock().unwrap().clone()
        }

        /// The `n`th call, counted over calls alone.
        fn call(&self, n: usize) -> Call {
            self.calls.lock().unwrap()[n].clone()
        }
    }

    /// A client over canned answers, recording everything it was handed.
    struct Recorder {
        answers: Vec<Response>,
        log: Log,
    }

    impl Client for Recorder {
        fn call(&mut self, call: Call) -> BoxFuture<'_, Result<Response, Failure>> {
            self.log.methods.lock().unwrap().push(call.method());
            self.log.calls.lock().unwrap().push(call);
            let answer = if self.answers.is_empty() {
                // Not a panic, so tests need not script an answer for the ending.
                Err(Failure::broken("nothing left to answer with"))
            } else {
                match self.answers.remove(0) {
                    Response::Error(error) => Err(Failure::Refused(error)),
                    answer => Ok(answer),
                }
            };
            Box::pin(async move { answer })
        }

        fn notify(&mut self, notification: Notification) -> BoxFuture<'_, Result<(), Failure>> {
            self.log.methods.lock().unwrap().push(notification.method());
            Box::pin(async { Ok(()) })
        }
    }

    /// A [`Recorder`] whose first poll never answers, as a silent command's does not within
    /// its wait.
    struct Silent {
        recorder: Recorder,
        stalled: bool,
    }

    impl Client for Silent {
        fn call(&mut self, call: Call) -> BoxFuture<'_, Result<Response, Failure>> {
            if matches!(call, Call::Poll(_)) && !self.stalled {
                self.stalled = true;
                self.recorder
                    .log
                    .methods
                    .lock()
                    .unwrap()
                    .push(call.method());
                self.recorder.log.calls.lock().unwrap().push(call);
                return Box::pin(std::future::pending());
            }
            self.recorder.call(call)
        }

        fn notify(&mut self, notification: Notification) -> BoxFuture<'_, Result<(), Failure>> {
            self.recorder.notify(notification)
        }
    }

    fn recorder(answers: Vec<Response>) -> (Recorder, Log) {
        let log = Log {
            methods: Arc::new(Mutex::new(Vec::new())),
            calls: Arc::new(Mutex::new(Vec::new())),
        };
        (
            Recorder {
                answers,
                log: log.clone(),
            },
            log,
        )
    }

    /// A default `init` response.
    fn initialized() -> Response {
        Response::Init(InitResp::default())
    }

    /// A successful `exec` response.
    fn ran(stdout: &[u8]) -> Response {
        Response::Exec(ExecResp {
            code: 0,
            stdout: stdout.to_vec(),
            ..ExecResp::default()
        })
    }

    /// Every channel failure surfaces at build, saying which one it was.
    #[tokio::test]
    async fn a_stdio_console_starts_its_server_when_it_is_built() {
        let Err(e) = ConsoleClient::builder()
            .cmd(&["virtx-no-such-console"])
            .build()
            .await
        else {
            panic!("a console over a program that does not exist should not build");
        };
        assert!(e.to_string().contains("starting the console server"), "{e}");

        // An empty command also fails at build.
        let Err(e) = ConsoleClient::builder().cmd(&[] as &[&str]).build().await else {
            panic!("a console over no program at all should not build");
        };
        assert!(e.to_string().contains("needs a program to run"), "{e}");

        // So does a program that does not speak the protocol, since build awaits `init`:
        // `cat` echoes the request back, which a client cannot answer. Arguments reach the
        // program as given.
        let Err(e) = ConsoleClient::builder().cmd(&["cat", "-u"]).build().await else {
            panic!("a console over a program that cannot answer should not build");
        };
        assert!(e.to_string().contains("cannot answer"), "{e}");
    }

    /// After `init`, each caller method is exactly one message, in order.
    #[tokio::test]
    async fn a_session_is_the_servers_to_keep() {
        let (client, log) = recorder(vec![initialized(), ran(b"hi\n")]);
        let mut console = ConsoleClient::builder()
            .client(client)
            .build()
            .await
            .unwrap();

        // Optional and unanswered; only moves when the boot happens.
        console.start().await.unwrap();
        assert_eq!(
            console.exec(["echo", "hi"], None).await.unwrap().stdout,
            b"hi\n"
        );
        // Releases boot resources; the session stays open.
        console.stop().await.unwrap();

        // Dropping sends one `quit` and no extra `stop`.
        drop(console);
        tokio::task::yield_now().await;

        assert_eq!(
            log.methods(),
            [
                Method::Init,
                Method::Start,
                Method::Exec,
                Method::Stop,
                Method::Quit
            ]
        );

        // `init` goes first, and an unconfigured console sends a default one.
        let Call::Init(init) = log.call(0) else {
            panic!("{:?} is not an init", log.call(0));
        };
        assert_eq!(init, InitCall::default());

        // The command goes out as written, with nothing added.
        let Call::Exec(exec) = log.call(1) else {
            panic!("{:?} is not an exec", log.call(1));
        };
        assert_eq!(
            exec,
            ExecCall {
                cmd: vec!["echo".into(), "hi".into()],
                timeout_ms: None,
            }
        );
    }

    /// A refused `init` yields no console, and so no `quit`.
    #[tokio::test]
    async fn a_console_the_server_will_not_have_does_not_exist() {
        let (client, log) = recorder(vec![Response::Error(Error::new(
            Error::UNSUPPORTED_MOUNT,
            "s3: this server realizes file:// and nothing else",
        ))]);
        let Err(e) = ConsoleClient::builder().client(client).build().await else {
            panic!("a console whose init was refused should not build");
        };
        assert!(e.to_string().contains("file://"), "{e}");

        // No `quit`: there is no `ConsoleClient` to owe one.
        tokio::task::yield_now().await;
        assert_eq!(log.methods(), [Method::Init]);
    }

    /// A never-started console still sends `quit` on drop: the server exists regardless of
    /// boot.
    #[tokio::test]
    async fn dropping_a_console_ends_its_session() {
        let (client, log) = recorder(vec![initialized()]);
        let console = ConsoleClient::builder()
            .client(client)
            .build()
            .await
            .unwrap();

        drop(console);

        // `quit` runs on a spawned task; let it.
        tokio::task::yield_now().await;
        assert_eq!(log.methods(), [Method::Init, Method::Quit]);
    }

    /// `close` returns with the session over: `quit` said, and the mounts let go of after it.
    #[tokio::test]
    async fn closing_a_console_returns_once_its_mounts_are_let_go() {
        /// A tree that notes what the server had been told when this end let go of it.
        struct Watched {
            at: PathBuf,
            log: Log,
            let_go_after: Arc<Mutex<Option<Vec<Method>>>>,
        }

        impl Mount for Watched {
            fn mountpoint(&self) -> &Path {
                &self.at
            }
        }

        impl Drop for Watched {
            fn drop(&mut self) {
                *self.let_go_after.lock().unwrap() = Some(self.log.methods());
            }
        }

        let (client, log) = recorder(vec![initialized()]);
        let let_go_after = Arc::new(Mutex::new(None));
        let console = ConsoleClient::builder()
            .client(client)
            .mount(
                Watched {
                    // Absolute on every host, as a mount point must be to be named.
                    at: std::env::temp_dir(),
                    log: log.clone(),
                    let_go_after: let_go_after.clone(),
                },
                "/work",
            )
            .build()
            .await
            .unwrap();

        console.close().await.unwrap();

        // Already let go of, with no yield: after `quit`, before `close` returned.
        assert_eq!(
            *let_go_after.lock().unwrap(),
            Some(vec![Method::Init, Method::Quit])
        );
        // And the console dropped inside `close` had nothing left to say.
        tokio::task::yield_now().await;
        assert_eq!(log.methods(), [Method::Init, Method::Quit]);
    }

    /// `init` names each mount as one spec string.
    ///
    /// Mount point and guest path differ on purpose, to show paths are spelled in the guest
    /// path.
    #[tokio::test]
    async fn a_console_names_its_mounts_and_where_it_put_them() {
        let (client, log) = recorder(vec![initialized()]);
        let console = ConsoleClient::builder()
            .client(client)
            .mount_readonly(PathBuf::from("/mnt/project"), "/work")
            .mount(PathBuf::from("/mnt/collected"), "/work/out")
            .build()
            .await
            .unwrap();

        let Call::Init(init) = log.call(0) else {
            panic!("{:?} is not an init", log.call(0));
        };
        assert_eq!(
            init.mounts
                .iter()
                .map(MountSpec::to_string)
                .collect::<Vec<_>>(),
            [
                "file:///mnt/project:/work:ro",
                "file:///mnt/collected:/work/out"
            ]
        );

        // `mounts()` yields the guest paths, in order.
        assert_eq!(
            console.mounts().collect::<Vec<_>>(),
            [Path::new("/work"), Path::new("/work/out")]
        );
    }

    /// An unnameable mount fails the build before anything is sent.
    #[tokio::test]
    async fn a_mount_this_end_cannot_name_is_not_a_session() {
        let (client, log) = recorder(vec![initialized()]);
        let Err(e) = ConsoleClient::builder()
            .client(client)
            .mount(PathBuf::from("relative/here"), "/work")
            .build()
            .await
        else {
            panic!("a console over a mount point with no URL should not build");
        };
        assert!(e.to_string().contains("cannot be named"), "{e}");
        assert_eq!(log.methods(), [] as [Method; 0]);

        // A relative guest path fails the same way.
        let (client, _) = recorder(vec![initialized()]);
        let Err(e) = ConsoleClient::builder()
            .client(client)
            .mount(PathBuf::from("/mnt/here"), "work")
            .build()
            .await
        else {
            panic!("a console over a relative guest path should not build");
        };
        assert!(e.to_string().contains("guest path is absolute"), "{e}");
    }

    /// What a poll answers with.
    fn polled(stdout: &[u8], code: Option<i32>) -> Response {
        Response::Poll(PollResp {
            stdout: stdout.to_vec(),
            code,
            ..PollResp::default()
        })
    }

    /// A stream yields what each poll brought, skips the polls that brought nothing, and ends
    /// with the one that carries the code.
    #[tokio::test]
    async fn a_stream_is_the_jobs_output_as_it_arrives() {
        let (client, log) = recorder(vec![
            initialized(),
            Response::Spawn(crate::protocol::SpawnResp { job: 7 }),
            polled(b"tick 1\n", None),
            polled(b"", None),
            polled(b"tick 2\n", None),
            polled(b"", Some(0)),
        ]);
        let mut console = ConsoleClient::builder()
            .client(client)
            .build()
            .await
            .unwrap();

        let mut stream = console
            .exec_stream(["sh", "-c", "for i in 1 2; do echo tick $i; done"], None)
            .await
            .unwrap();
        assert_eq!(stream.job(), 7);
        let mut seen = Vec::new();
        while let Some(chunk) = stream.next().await {
            seen.push(chunk.unwrap());
        }
        drop(stream);

        assert_eq!(
            seen.iter().map(|c| c.stdout.as_slice()).collect::<Vec<_>>(),
            [b"tick 1\n".as_slice(), b"tick 2\n", b""]
        );
        assert_eq!(seen.last().unwrap().code, Some(0));

        // Every poll names the job and waits rather than spinning.
        assert_eq!(
            log.methods(),
            [
                Method::Init,
                Method::Spawn,
                Method::Poll,
                Method::Poll,
                Method::Poll,
                Method::Poll
            ]
        );
        let Call::Poll(poll) = log.call(2) else {
            panic!("{:?} is not a poll", log.call(2));
        };
        assert_eq!(poll.job, 7);
        assert!(poll.wait_ms.is_some());

        // A finished stream leaves nothing for the next call to clean up.
        assert!(console.abandoned.is_empty());
    }

    /// A job killed at its timeout ends the stream in the refusal, which carries its last
    /// bytes.
    #[tokio::test]
    async fn a_stream_ends_in_its_timeout() {
        let written = ExecResp {
            code: 137,
            stdout: b"started\n".to_vec(),
            ..ExecResp::default()
        };
        let (client, _) = recorder(vec![
            initialized(),
            Response::Spawn(crate::protocol::SpawnResp { job: 0 }),
            Response::Error(
                Error::new(Error::TIMED_OUT, "killed after 1000ms").with_output(&written),
            ),
        ]);
        let mut console = ConsoleClient::builder()
            .client(client)
            .build()
            .await
            .unwrap();

        let mut stream = console
            .exec_stream(["sh", "-c", "echo started; sleep 5"], Some(1000))
            .await
            .unwrap();
        let Some(Err(Failure::Refused(refused))) = stream.next().await else {
            panic!("a timed-out job should end its stream in a refusal");
        };
        assert_eq!(refused.code, Error::TIMED_OUT);
        assert_eq!(refused.output(), Some(written));
        assert!(stream.next().await.is_none());
    }

    /// A stream dropped while a poll is out has the job killed before the console's next call
    /// too: that is where a stream over a silent command spends its time.
    #[tokio::test]
    async fn dropping_a_stream_mid_poll_kills_its_job() {
        let (recorder, log) = recorder(vec![
            initialized(),
            Response::Spawn(crate::protocol::SpawnResp { job: 3 }),
            // The poll that never answers takes none of these.
            Response::Kill(crate::protocol::KillResp {}),
            polled(b"", Some(137)),
            ran(b"hi\n"),
        ]);
        let client = Silent {
            recorder,
            stalled: false,
        };
        let mut console = ConsoleClient::builder()
            .client(client)
            .build()
            .await
            .unwrap();

        let mut stream = console.exec_stream(["sleep", "1000"], None).await.unwrap();
        {
            let mut next = std::pin::pin!(stream.next());
            let polled = std::future::poll_fn(|cx| Poll::Ready(next.as_mut().poll(cx))).await;
            assert!(polled.is_pending(), "the poll should still be waiting");
        }
        drop(stream);

        assert_eq!(
            console.exec(["echo", "hi"], None).await.unwrap().stdout,
            b"hi\n"
        );
        assert_eq!(
            log.methods(),
            [
                Method::Init,
                Method::Spawn,
                Method::Poll,
                Method::Kill,
                Method::Poll,
                Method::Exec
            ]
        );
        let Call::Kill(kill) = log.call(3) else {
            panic!("{:?} is not a kill", log.call(3));
        };
        assert_eq!(kill.job, 3);
    }

    /// A stream dropped before its job ended has the job killed, and collected, before the
    /// console's next call goes out.
    #[tokio::test]
    async fn dropping_a_stream_kills_its_job() {
        let (client, log) = recorder(vec![
            initialized(),
            Response::Spawn(crate::protocol::SpawnResp { job: 3 }),
            polled(b"serving\n", None),
            // What the next call sends first: the kill, then polls until the end.
            Response::Kill(crate::protocol::KillResp {}),
            polled(b"", None),
            polled(b"", Some(137)),
            ran(b"hi\n"),
        ]);
        let mut console = ConsoleClient::builder()
            .client(client)
            .build()
            .await
            .unwrap();

        let mut stream = console.exec_stream(["serve"], None).await.unwrap();
        assert_eq!(stream.next().await.unwrap().unwrap().stdout, b"serving\n");
        drop(stream);

        assert_eq!(
            console.exec(["echo", "hi"], None).await.unwrap().stdout,
            b"hi\n"
        );
        assert_eq!(
            log.methods(),
            [
                Method::Init,
                Method::Spawn,
                Method::Poll,
                Method::Kill,
                Method::Poll,
                Method::Poll,
                Method::Exec
            ]
        );
        let Call::Kill(kill) = log.call(3) else {
            panic!("{:?} is not a kill", log.call(3));
        };
        assert_eq!(
            kill,
            KillCall {
                job: 3,
                signal: None
            }
        );
    }
}
