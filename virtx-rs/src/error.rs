use std::fmt;

/// Why a call, or opening a client, failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// `libvirtx_core` did not open, or lacks a function this crate calls.
    Library(String),
    /// The library answered with an error: the server's, the transport's, or its own.
    Call(String),
    /// What crosses the C ABI did not encode or read back.
    Json(String),
    /// An argument cannot cross the C ABI: a string with a NUL in it.
    Argument(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Library(message)
            | Error::Call(message)
            | Error::Json(message)
            | Error::Argument(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for Error {}
