//! Why a client's request failed, whichever endpoint it went to, and where.

#[cfg(feature = "client")]
use std::fmt;

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

/// Why a call failed. Each variant is one [`Source`]: what made the error says where it
/// failed by the variant it picks.
#[cfg(feature = "client")]
#[derive(Debug)]
pub enum Error {
    /// The request did not reach the server, or its response did not come back: the
    /// connection failed or the session ended.
    Transport(String),
    /// The server refused the request with an HTTP status: the status, and why, for a person,
    /// as `code: message` where the server answered its error as `PROTOCOL.md`'s Resources do,
    /// and as the codes and messages of its `errors` where its registry refused. A `401` or a
    /// `403` from any endpoint says the headers are not what it wants: a client may set new ones
    /// and send the request again.
    Status(u16, String),
    /// An HTTP response that is not the protocol's error: its status, what it was rather than
    /// its body for a person, and its raw body if it had one.
    Refusal {
        status: u16,
        description: String,
        body: Option<String>,
    },
    /// A job ended without doing what it was started for: why, for a person, as the job's
    /// `error` says.
    Job(String),
    /// This side failed: an argument that is not what the call takes, a file on this machine
    /// that could not be read or written, or an answer it could not read.
    Client(String),
}

#[cfg(feature = "client")]
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Transport(reason) | Error::Job(reason) | Error::Client(reason) => {
                f.write_str(reason)
            }
            Error::Status(status, body) => {
                let reason = http::StatusCode::from_u16(*status)
                    .ok()
                    .and_then(|status| status.canonical_reason())
                    .unwrap_or("");
                write!(f, "the server answered {status} {reason}")?;
                if !body.is_empty() {
                    write!(f, ": {body}")?;
                }
                Ok(())
            }
            Error::Refusal {
                status,
                description,
                ..
            } => {
                let reason = http::StatusCode::from_u16(*status)
                    .ok()
                    .and_then(|status| status.canonical_reason())
                    .unwrap_or("");
                write!(f, "the server answered {status} {reason}")?;
                if !description.is_empty() {
                    write!(f, "; {description}")?;
                }
                Ok(())
            }
        }
    }
}

#[cfg(feature = "client")]
impl std::error::Error for Error {}

#[cfg(feature = "client")]
impl Error {
    /// Where it failed.
    pub fn source_kind(&self) -> Source {
        match self {
            Error::Status(..) | Error::Refusal { .. } => Source::Server,
            Error::Job(_) => Source::Job,
            Error::Transport(_) => Source::Transport,
            Error::Client(_) => Source::Client,
        }
    }

    /// The HTTP status the server refused the request with, if it did.
    pub fn status(&self) -> Option<u16> {
        match self {
            Error::Status(status, _) | Error::Refusal { status, .. } => Some(*status),
            _ => None,
        }
    }
}

#[cfg(all(test, feature = "client"))]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_reads_as_its_status_and_why() {
        let refused = Error::Status(409, "in_use: held".into());
        assert_eq!(
            refused.to_string(),
            "the server answered 409 Conflict: in_use: held"
        );
        assert_eq!(
            Error::Status(404, String::new()).to_string(),
            "the server answered 404 Not Found"
        );
        // A status HTTP gives no reason has none.
        assert_eq!(
            Error::Status(599, "odd".into()).to_string(),
            "the server answered 599 : odd"
        );
        assert_eq!(Error::Transport("gone".into()).to_string(), "gone");
    }

    #[test]
    fn a_refusal_gives_its_status() {
        assert_eq!(Error::Status(404, "not found".into()).status(), Some(404));
        assert_eq!(
            Error::Refusal {
                status: 503,
                description: "not a virtx answer".into(),
                body: None,
            }
            .status(),
            Some(503)
        );
        assert_eq!(Error::Transport("connection reset".into()).status(), None);
        assert_eq!(Error::Client("bad json".into()).status(), None);
    }

    #[test]
    fn an_error_names_where_it_failed() {
        let cases = [
            (Error::Status(409, "in_use: held".into()), Source::Server),
            (Error::Job("step 1 exited 3".into()), Source::Job),
            (
                Error::Transport("connection reset".into()),
                Source::Transport,
            ),
            (Error::Client("not UTF-8".into()), Source::Client),
        ];
        for (error, source) in cases {
            assert_eq!(error.source_kind(), source, "{error:?}");
        }
    }
}
