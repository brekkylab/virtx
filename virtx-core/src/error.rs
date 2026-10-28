//! Why a client's request failed, whichever endpoint it went to.

use std::fmt;

use crate::rpc::ErrorObject;

/// Why a call failed.
#[derive(Debug)]
pub enum Error {
    /// The server answered with an error.
    Call(ErrorObject),
    /// The request did not reach the server, or its response did not come back: the
    /// connection failed, the server answered an HTTP error, or the session ended.
    Transport(String),
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
        }
    }
}

impl std::error::Error for Error {}
