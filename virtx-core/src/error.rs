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
    /// The body did not encode, or the answer did not read as the type asked for.
    Json(String),
    /// The server's image registry answered with an error, or with what is not an image.
    Registry(String),
    /// A file on this machine could not be read or written, or a file is not what it should
    /// be: one read as text that is not UTF-8.
    File(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Transport(reason) => f.write_str(reason),
            Error::Json(reason) => f.write_str(reason),
            Error::Registry(reason) => f.write_str(reason),
            Error::File(reason) => f.write_str(reason),
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
        }
    }
}

impl std::error::Error for Error {}

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
}
