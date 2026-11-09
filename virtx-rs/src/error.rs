use std::fmt;

/// Why a call, or opening a client, failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// `libvirtx_core` did not open, or lacks a function this crate calls.
    Library(String),
    /// The server refused the request with this HTTP status, as `PROTOCOL.md` lists them: `404`
    /// for what does not exist, `409` for what is in use or has ended, and the like.
    Status(u16, String),
    /// The library answered with an error of its own or the transport's.
    Call(String),
    /// What crosses the C ABI did not encode or read back.
    Json(String),
    /// An argument cannot cross the C ABI: a string with a NUL in it.
    Argument(String),
}

impl Error {
    /// The HTTP status the server refused the request with, if it did.
    pub fn status(&self) -> Option<u16> {
        match self {
            Error::Status(status, _) => Some(*status),
            _ => None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Library(message)
            | Error::Status(_, message)
            | Error::Call(message)
            | Error::Json(message)
            | Error::Argument(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for Error {}
