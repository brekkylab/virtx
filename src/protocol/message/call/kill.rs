use serde::{Deserialize, Serialize};

use super::JobId;

/// Signal a job, and everything it started. The `params` of `kill`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KillCall {
    pub job: JobId,

    /// The signal, by its Linux number. `None` is `SIGKILL` (9), what a timeout sends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
}

/// The signal went out. The `result` of `kill`.
///
/// Not the ending: the next [`PollCall`](super::PollCall) reports that, as for any other
/// job. A job that has already ended is not signalled, and is answered the same.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KillResp {}
