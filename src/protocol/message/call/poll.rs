use serde::{Deserialize, Serialize};

use super::JobId;
use crate::protocol::message::utils::bytes;

/// Collect what a job wrote since the last `poll`. The `params` of `poll`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PollCall {
    pub job: JobId,

    /// How long to wait for new output, or for the end, when there is none yet. `None` answers
    /// at once, so a client that polls in a loop sets this rather than spinning.
    ///
    /// The console takes no other call while a `poll` waits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_ms: Option<u64>,

    /// At most this many bytes of each stream; the rest waits for the next `poll`. `None` is
    /// as many as one message holds.
    ///
    /// `0` is [`INVALID_PARAMS`](crate::protocol::Error::INVALID_PARAMS): the end is reported
    /// only behind everything written before it, which a poll taking nothing never reaches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
}

/// What a job wrote since the last `poll`, and how it ended once it has. The `result` of
/// `poll`.
///
/// A job that outlived its [`timeout_ms`](super::ExecCall::timeout_ms) ends in
/// [`TIMED_OUT`](crate::protocol::Error::TIMED_OUT) instead, with this answer's bytes in the
/// error's `data` (see [`Error::output`](crate::protocol::Error::output)).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PollResp {
    #[serde(with = "bytes")]
    pub stdout: Vec<u8>,

    #[serde(with = "bytes")]
    pub stderr: Vec<u8>,

    /// The job wrote more than the server holds between two `poll`s, and the oldest of it was
    /// dropped before these bytes.
    #[serde(default)]
    pub truncated: bool,

    /// How the job ended, as [`ExecResp::code`](super::ExecResp::code); present once it has
    /// and everything it wrote has been handed back. The job is gone after this answer, and a
    /// later `poll` naming it is [`UNKNOWN_JOB`](crate::protocol::Error::UNKNOWN_JOB).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<i32>,
}

impl PollResp {
    /// Whether this answer carries nothing: no bytes, no drop and no ending.
    pub fn is_empty(&self) -> bool {
        self.stdout.is_empty() && self.stderr.is_empty() && !self.truncated && self.code.is_none()
    }
}
