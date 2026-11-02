//! Why a client's request failed, whichever endpoint it went to.

use std::fmt;

use crate::rpc::ErrorObject;

/// Why a call failed.
#[derive(Debug)]
pub enum Error {
    /// The server answered with an error.
    Call(ErrorObject),
    /// The request did not reach the server, or its response did not come back: the
    /// connection failed or the session ended.
    Transport(String),
    /// The server refused the request with an HTTP status: the status, and the body, for a
    /// person. A `401` or a `403` from any endpoint says the headers are not what it wants: a
    /// client may set new ones and send the request again.
    Status(u16, String),
    /// The params did not encode, or the result did not read as the type asked for.
    Json(String),
    /// The server's image registry answered with an error, or with what is not an image.
    Registry(String),
    /// A file on this machine could not be read or written, or is not what it should be.
    File(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Call(error) => write!(f, "{error}"),
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
