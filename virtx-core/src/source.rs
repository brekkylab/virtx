//! Where a call failed, as every client's error names it.

use serde::{Deserialize, Serialize};

/// Where a call failed, which every error names whatever else it carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// The server answered the request with an error: its HTTP status says which.
    Server,
    /// The job the request started failed, or was removed before it ended.
    Job,
    /// The request or its answer did not get through: the connection failed or the session
    /// ended.
    Transport,
    /// This side failed: an argument, a file on this machine, or an answer it could not read.
    Client,
}
