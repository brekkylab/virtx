//! Why a request could not be answered: a numeric code a program branches on, and a
//! sentence a person reads.
//!
//! The codes are the whole of what a requester branches on; a failure kind is a new code,
//! never a new message shape.

use std::fmt;

use bson::Bson;
use serde::{Deserialize, Serialize};

use super::ExecResp;

/// Why a request could not be answered with a result.
///
/// `data` is optional extra context, boxed because a [`Bson`] is several times the size
/// of the rest and would bloat every `Result<_, Failure>`; the box is invisible on the wire.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Error {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Box<Bson>>,
}

/// The `init` codes depend only on the frame and the server (its build, host and stored
/// images), so they are raised before any boot.
impl Error {
    /// `exec`, `poll`: the execution outlived its [`timeout_ms`](super::ExecCall::timeout_ms)
    /// and was killed.
    ///
    /// An error, not a result, because the command did not end by itself. What it wrote up
    /// to the kill is in `data`, read with [`output`](Self::output). Retry with more time, or
    /// give up.
    pub const TIMED_OUT: i64 = -32000;

    /// `exec`: the program was not there, or could not be started.
    pub const NOT_EXECUTABLE: i64 = -32001;

    /// `exec`, `read`, `write`, `snapshot`: the backend could not be brought up.
    ///
    /// Booting happens on demand ([`Start`](super::Notification::Start) only asks early),
    /// so this goes to the call that needed the boot, and says the failure was the
    /// session's rather than the command's or the path's.
    pub const BOOT_FAILED: i64 = -32002;

    /// `read`, `write`: nothing is at the path. For a `write`, a parent directory is
    /// missing, since the file itself is created.
    pub const NOT_FOUND: i64 = -32005;

    /// `read`, `write`: the path is a directory. The name is taken, and a retry will not
    /// turn it into a file.
    pub const IS_A_DIRECTORY: i64 = -32006;

    /// `read`, `write`: the file could not be read or written (permissions, full disk,
    /// backend gone mid-operation).
    ///
    /// A failed `write` says nothing about how much of `data` landed; `read` to find out.
    pub const IO_FAILED: i64 = -32007;

    /// `init`: a mount whose scheme this server has no provider for; the message names
    /// the entry.
    ///
    /// One code for all mounts, since the fix is the same: a different URL, or a build that
    /// has the provider.
    pub const UNSUPPORTED_MOUNT: i64 = -32008;

    /// `exec`, `read`, `write`, `snapshot`: a tree could not be put where `init` said it
    /// would be (no mount binding compiled in, no FUSE provider, mount point busy, store
    /// unreachable).
    ///
    /// Mounting happens at boot, so this reaches the call that needed one. The session is
    /// described correctly; the environment is what has to change.
    pub const MOUNT_FAILED: i64 = -32009;

    /// `init`: a network this server cannot give the way it was asked for.
    ///
    /// A server whose commands run on this host cannot take the network away, so it refuses
    /// `network: false` rather than running them with one; the fix is another backend.
    pub const UNSUPPORTED_NETWORK: i64 = -32010;

    /// `init`: this backend cannot swap the base image at all (its commands run on the
    /// server's own filesystem).
    pub const UNSUPPORTED_IMAGE: i64 = -32011;

    /// `init`: the session named a built image this server does not have; the client can
    /// build it.
    ///
    /// Checked at `init` because it is a local file test; whether a registry has a
    /// reference is a network round trip, so an unfetchable reference is a failed boot.
    pub const UNKNOWN_IMAGE: i64 = -32012;

    /// `init`: this server cannot make the machine shape asked for (a GPU it has no device
    /// for, more vCPUs or memory than it will give).
    ///
    /// A retry will not help: the request, or the server, has to change.
    pub const UNSUPPORTED_MACHINE: i64 = -32013;

    /// `poll`, `kill`: the server holds no job by that id. Its end was already reported, or
    /// a `stop` or an `init` ended it, or it was never spawned.
    pub const UNKNOWN_JOB: i64 = -32015;

    /// `spawn`: the server already holds as many jobs as it will. A job is held until the
    /// `poll` that reports its end, so poll one to it, killing it first if it should not
    /// finish ([`ConsoleClient::discard`](crate::console::ConsoleClient::discard) does both).
    pub const TOO_MANY_JOBS: i64 = -32016;

    /// The spec codes a peer can hit here. `-32700` (parse error) belongs to the frame
    /// reader.
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;

    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Error {
            code,
            message: message.into(),
            data: None,
        }
    }

    /// The same refusal, carrying what the command wrote before it was killed.
    pub fn with_output(mut self, output: &ExecResp) -> Self {
        // Serializing an `ExecResp` to a value cannot fail: every member has a BSON type.
        self.data = bson::serialize_to_bson(output).ok().map(Box::new);
        self
    }

    /// What the command wrote before it was killed: the `data` of a
    /// [`TIMED_OUT`](Self::TIMED_OUT). `None` for any other refusal, and for one from a server
    /// that sends no `data`.
    pub fn output(&self) -> Option<ExecResp> {
        let data = self.data.as_deref()?;
        bson::deserialize_from_bson(data.clone()).ok()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::Error;

    /// Colliding codes would be indistinguishable failures, and the compiler cannot catch it.
    #[test]
    fn every_error_code_is_distinct() {
        let codes = [
            ("TIMED_OUT", Error::TIMED_OUT),
            ("NOT_EXECUTABLE", Error::NOT_EXECUTABLE),
            ("BOOT_FAILED", Error::BOOT_FAILED),
            ("NOT_FOUND", Error::NOT_FOUND),
            ("IS_A_DIRECTORY", Error::IS_A_DIRECTORY),
            ("IO_FAILED", Error::IO_FAILED),
            ("UNSUPPORTED_MOUNT", Error::UNSUPPORTED_MOUNT),
            ("MOUNT_FAILED", Error::MOUNT_FAILED),
            ("UNSUPPORTED_NETWORK", Error::UNSUPPORTED_NETWORK),
            ("UNSUPPORTED_IMAGE", Error::UNSUPPORTED_IMAGE),
            ("UNKNOWN_IMAGE", Error::UNKNOWN_IMAGE),
            ("UNSUPPORTED_MACHINE", Error::UNSUPPORTED_MACHINE),
            ("UNKNOWN_JOB", Error::UNKNOWN_JOB),
            ("TOO_MANY_JOBS", Error::TOO_MANY_JOBS),
            ("INVALID_REQUEST", Error::INVALID_REQUEST),
            ("METHOD_NOT_FOUND", Error::METHOD_NOT_FOUND),
            ("INVALID_PARAMS", Error::INVALID_PARAMS),
            ("INTERNAL_ERROR", Error::INTERNAL_ERROR),
        ];
        let mut seen = std::collections::HashMap::new();
        for (name, code) in codes {
            if let Some(taken) = seen.insert(code, name) {
                panic!("{code} is both {taken} and {name}");
            }
        }
    }

    /// A killed command's output survives the frame byte for byte, and a refusal without it
    /// has none to give.
    #[test]
    fn a_timeout_carries_what_was_written() {
        use crate::protocol::{ExecResp, Message, Response};

        let written = ExecResp {
            code: 137,
            stdout: b"started\n".to_vec(),
            stderr: vec![0xff, 0x00],
            truncated: false,
        };
        let refused = Error::new(Error::TIMED_OUT, "killed after 1000ms").with_output(&written);

        let message = Message::Response {
            id: 2,
            result: Response::Error(refused),
        };
        let frame = bson::serialize_to_vec(&message).unwrap();
        let Message::Response {
            result: Response::Error(read),
            ..
        } = bson::deserialize_from_slice(&frame).unwrap()
        else {
            panic!("an error went out and something else came back");
        };
        assert_eq!(read.output(), Some(written));

        assert_eq!(Error::new(Error::TIMED_OUT, "old server").output(), None);
    }
}
