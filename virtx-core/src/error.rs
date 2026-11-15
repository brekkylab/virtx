//! Why a client's request failed, whichever endpoint it went to.

use std::fmt;

/// Why a call failed.
#[derive(Debug)]
pub enum Error {
    /// The request did not reach the server, or its response did not come back: the
    /// connection failed or the session ended.
    Transport(String),
    /// The server refused the request with an HTTP status: the status, and why, for a person,
    /// as `code: message` where the server answered its error as `PROTOCOL.md`'s Resources do. A
    /// `401` or a `403` from any endpoint says the headers are not what it wants: a client may
    /// set new ones and send the request again.
    Status(u16, String),
    /// An HTTP response that is not the protocol's error: its status, what it was rather than
    /// its body for a person, and its raw body if it had one.
    Refusal {
        status: u16,
        description: String,
        body: Option<String>,
    },
    /// The body did not encode, or the answer did not read as the type asked for.
    Json(String),
    /// The server's image registry answered with an error, or with what is not an image.
    Registry(String),
    /// A file on this machine could not be read or written, or a file is not what it should
    /// be: one read as text that is not UTF-8.
    File(String),
    /// A path given goes above its root, once its `.` and `..` are resolved, or names the root
    /// where that is not a path; or a name given is `.` or `..`. Sent, it would name another
    /// path than the one written, another resource's even, so nothing was sent.
    Path(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Transport(reason) => f.write_str(reason),
            Error::Json(reason) => f.write_str(reason),
            Error::Registry(reason) => f.write_str(reason),
            Error::File(reason) => f.write_str(reason),
            Error::Path(reason) => f.write_str(reason),
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

impl std::error::Error for Error {}

impl Error {
    /// The HTTP status the server refused the request with, if it did.
    pub fn status(&self) -> Option<u16> {
        match self {
            Error::Status(status, _) | Error::Refusal { status, .. } => Some(*status),
            _ => None,
        }
    }
}

#[cfg(test)]
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
        assert_eq!(Error::Json("bad json".into()).status(), None);
    }
}
