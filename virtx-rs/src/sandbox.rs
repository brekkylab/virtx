//! A sandbox and the jobs in it, each with the client its requests go through.

use virtx_core::{
    FileRoot, ImageJobEntry, InboundRule, LogEntry, PortInbound, Sandbox as SandboxInfo,
    SandboxExecArgs, SandboxExecItem, SandboxJob as SandboxJobInfo, SandboxJobEntry, SandboxPort,
};

use crate::{Call, Client, Events, ImageJob, StartedJob, files};

/// A sandbox the server holds, as making, finding or asking for it answered, and the client
/// that reaches it: what is needed to start and stop it, run processes in it, open its ports,
/// make an image of it, and reach its files, through [`Files`](crate::Files).
///
/// Cloning holds the same client once more. Dropping it leaves the sandbox as it is, running
/// or stopped; it is kept until [`remove`](Self::remove).
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

    /// The name it was given, which finds it again ([`Client::find_sandbox`]).
    pub fn name(&self) -> Option<&str> {
        self.info.name.as_deref()
    }

    /// What the server answered about it, as it was then: its image, whether it runs, when it
    /// stops and is deleted unused, and its ports, which only making and starting it answer.
    pub fn info(&self) -> &SandboxInfo {
        &self.info
    }

    /// Start it if it is stopped -- its VM booted over its disk as it was left -- and answer it
    /// once it runs, with where its ports are now. A job started in a stopped sandbox starts
    /// it too.
    pub fn start(&self) -> Call<Sandbox> {
        self.client.start_sandbox(&self.info.id)
    }

    /// Stop it if it runs: its jobs end, and its disk is kept -- or, for an ephemeral one,
    /// deleted with it.
    pub fn stop(&self) -> Call<Sandbox> {
        self.client.stop_sandbox(&self.info.id)
    }

    /// Open the TCP port `port` in it, as those it was made with are, and answer where it is
    /// reached from.
    pub fn open_port(&self, port: u16) -> Call<SandboxPort> {
        self.client.open_sandbox_port(&self.info.id, port)
    }

    /// Close the port `port` in it: its URL reaches it no more, and it is not opened again as
    /// it starts. One not open is refused `404`.
    pub fn close_port(&self, port: u16) -> Call<()> {
        self.client.close_sandbox_port(&self.info.id, port)
    }

    /// The inbound rules of its port `port`, as [`Client::get_sandbox_port_inbound`] answers
    /// them: virtx cloud's.
    pub fn inbound(&self, port: u16) -> Call<PortInbound> {
        self.client.get_sandbox_port_inbound(&self.info.id, port)
    }

    /// Make the inbound rules of its port `port` `rules`, all of them, as
    /// [`Client::set_sandbox_port_inbound`] does.
    pub fn set_inbound(&self, port: u16, rules: &[InboundRule]) -> Call<PortInbound> {
        self.client
            .set_sandbox_port_inbound(&self.info.id, port, rules)
    }

    /// Start making an image of it -- its image, with what it has written as one more layer --
    /// to point `reference` at: how a sandbox is kept to start others from. Of a running one,
    /// as it is when the job starts; of a stopped one, as it was left.
    pub fn commit(&self, reference: impl Into<String>) -> StartedJob<ImageJob, ImageJobEntry> {
        self.client.commit_image(&self.info.id, reference)
    }

    /// Start a process in it as `args` says; see [`SandboxExecArgs::new`]. Its output and how
    /// it ended are the job's [`log`](SandboxJob::log), or [`StartedJob::log`].
    pub fn exec(&self, args: &SandboxExecArgs) -> StartedJob<SandboxJob, SandboxJobEntry> {
        self.client.exec_sandbox(&self.info.id, args)
    }

    /// Start a process in it as `args` says, and read its log: its job, once, first, then each
    /// entry until its `end`. One request where the server answers an exec with its log (virtx
    /// cloud's), two elsewhere; [`exec`](Self::exec)'s log is always two. Dropping it leaves
    /// the job running.
    pub fn exec_with_log(&self, args: &SandboxExecArgs) -> Events<SandboxExecItem> {
        self.client.exec_sandbox_with_log(&self.info.id, args)
    }

    /// Every job in it.
    pub fn jobs(&self) -> Call<Vec<SandboxJob>> {
        self.client.list_sandbox_job(&self.info.id)
    }

    /// The job `id` in it.
    pub fn job(&self, id: impl AsRef<str>) -> Call<SandboxJob> {
        self.client.get_sandbox_job(&self.info.id, id)
    }

    /// The job `info` describes in it, reached through its client, without asking the server.
    pub fn job_from(&self, info: SandboxJobInfo) -> SandboxJob {
        SandboxJob::new(self.client.clone(), self.info.id.clone(), info)
    }

    /// Delete it: stopped first if it runs, and its disk removed. Its volumes stay.
    pub fn remove(&self) -> Call<()> {
        self.client.remove_sandbox(&self.info.id)
    }
}

impl files::sealed::Root for Sandbox {
    fn client(&self) -> &Client {
        &self.client
    }

    fn root(&self) -> FileRoot {
        FileRoot::Sandbox(self.info.id.clone())
    }
}

impl std::fmt::Debug for Sandbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Sandbox").field(&self.info).finish()
    }
}

/// A process run in a sandbox, as starting it or asking for it answered, and the client and
/// sandbox that reach it: what signals it, writes to its stdin and reads its log.
///
/// Cloning holds the same client once more. Dropping it leaves the job as it is; it goes with
/// [`remove`](Self::remove).
#[derive(Clone)]
pub struct SandboxJob {
    client: Client,
    sandbox: String,
    info: SandboxJobInfo,
}

impl SandboxJob {
    pub(crate) fn new(client: Client, sandbox: String, info: SandboxJobInfo) -> SandboxJob {
        SandboxJob {
            client,
            sandbox,
            info,
        }
    }

    /// The client its requests go through.
    pub fn client(&self) -> &Client {
        &self.client
    }

    pub fn id(&self) -> &str {
        &self.info.id
    }

    /// The id of the sandbox it runs in.
    pub fn sandbox_id(&self) -> &str {
        &self.sandbox
    }

    /// What the server answered about it: its process, and whether and how it had ended, as
    /// it was then.
    pub fn info(&self) -> &SandboxJobInfo {
        &self.info
    }

    /// It as the server has it now.
    pub fn get(&self) -> Call<SandboxJob> {
        self.client.get_sandbox_job(&self.sandbox, &self.info.id)
    }

    /// Send `signal`, such as `SIGTERM`, to its process and every process it started.
    pub fn signal(&self, signal: impl AsRef<str>) -> Call<()> {
        self.client
            .signal_sandbox_job(&self.sandbox, &self.info.id, signal)
    }

    /// Write `data` to its process's stdin, then close it if `eof`. Answers once it is written:
    /// a caller writes again only then, so the writes land in order.
    pub fn write_stdin(&self, data: Option<impl Into<String>>, eof: bool) -> Call<()> {
        self.client
            .write_sandbox_job_stdin(&self.sandbox, &self.info.id, data, eof)
    }

    /// Its log, its process's output, from its start or after its `after`th entry, until its
    /// `end` entry. Dropping it leaves the job as it is.
    pub fn log(&self, after: Option<u64>) -> Events<LogEntry<SandboxJobEntry>> {
        self.client
            .sandbox_job_log(&self.sandbox, &self.info.id, after)
    }

    /// Cancel it if it runs, killing its process, and forget it.
    pub fn remove(&self) -> Call<()> {
        self.client.remove_sandbox_job(&self.sandbox, &self.info.id)
    }
}

impl std::fmt::Debug for SandboxJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SandboxJob")
            .field("sandbox", &self.sandbox)
            .field("info", &self.info)
            .finish()
    }
}
