//! A sandbox and the jobs in it, each with the client its requests go through.

use virtx_core::{
    FileRoot, LogEntry, Sandbox as SandboxInfo, SandboxExecArgs, SandboxJob as SandboxJobInfo,
    SandboxJobEntry,
};

use crate::{Call, Client, Error, Events, StartedJob, files};

/// A sandbox the server holds, as making it or asking for it answered, and the client that
/// reaches it: what is needed to run processes in it, and its files, through
/// [`Files`](crate::Files).
///
/// Cloning holds the same client once more. Dropping it leaves the sandbox as it is; it goes
/// with [`remove`](Self::remove).
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

    /// Start a process in it as `args` says; see [`SandboxExecArgs::new`]. Its output and how
    /// it ended are the job's [`log`](SandboxJob::log), or [`StartedJob::log`].
    pub fn exec(&self, args: &SandboxExecArgs) -> StartedJob<SandboxJob, SandboxJobEntry> {
        self.client.exec_sandbox(&self.info.id, args)
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

    /// Stop it, end its jobs, and forget it, keeping its snapshot and volumes.
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

    /// It as `entry`, its log's `end` entry, says it ended: with its exit code. `None` for any
    /// other entry.
    pub(crate) fn ended(&self, entry: &SandboxJobEntry) -> Option<Result<SandboxJob, Error>> {
        let SandboxJobEntry::End(end) = entry else {
            return None;
        };
        Some(Ok(SandboxJob::new(
            self.client.clone(),
            self.sandbox.clone(),
            SandboxJobInfo {
                running: false,
                code: Some(end.code),
                ..self.info.clone()
            },
        )))
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
