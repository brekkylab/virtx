use std::fmt;

use virtx_core::Source;

/// Why a call, or opening a client, failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// `libvirtx_core` did not open, or lacks a function this crate calls.
    Library(String),
    /// The server refused the request with this HTTP status, as the protocol lists them: `404`
    /// for what does not exist, `409` for what is in use or has ended, and the like; with the
    /// protocol's code where the server answered one (`not_found`, `in_use`), and the message.
    Status {
        status: u16,
        code: Option<String>,
        message: String,
    },
    /// The library answered with an error that is not the server refusing the request: where
    /// it failed, and why.
    Call(Source, String),
    /// What crosses the C ABI did not encode or read back.
    Json(String),
    /// An argument cannot cross the C ABI: a string with a NUL in it.
    Argument(String),
}

impl Error {
    /// Where it failed: [`Source::Server`] for a request the server refused, and for each
    /// [`Call`](Self::Call) what the library said; [`Source::Client`] for what failed here.
    pub fn source_kind(&self) -> Source {
        match self {
            Error::Status { .. } => Source::Server,
            Error::Call(source, _) => *source,
            Error::Library(_) | Error::Json(_) | Error::Argument(_) => Source::Client,
        }
    }

    /// The HTTP status the server refused the request with, if it did.
    pub fn status(&self) -> Option<u16> {
        match self {
            Error::Status { status, .. } => Some(*status),
            _ => None,
        }
    }

    /// The protocol's code the server refused the request with, if it answered one:
    /// `not_found`, `in_use` and the like -- what to branch on, not the message.
    pub fn code(&self) -> Option<&str> {
        match self {
            Error::Status { code, .. } => code.as_deref(),
            _ => None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Library(message)
            | Error::Status { message, .. }
            | Error::Call(_, message)
            | Error::Json(message)
            | Error::Argument(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for Error {}
