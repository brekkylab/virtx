use serde::{Deserialize, Serialize};

/// Names a job for as long as the server holds it: from the `spawn` that started it to the
/// `poll` that reports its end.
///
/// Never reused within a session, across a `stop` and the boot after it too, so an id kept
/// past its job names nothing rather than another job.
pub type JobId = u64;

/// A job started and left running. The `result` of `spawn`, whose `params` are an
/// [`ExecCall`](super::ExecCall).
///
/// Answered at once rather than at the command's end: what it writes is collected with
/// [`PollCall`](super::PollCall), and it is ended early with [`KillCall`](super::KillCall).
/// Between those calls the console is free for any other.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnResp {
    pub job: JobId,
}
