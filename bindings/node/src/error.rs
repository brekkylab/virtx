//! How virtx failures reach JavaScript, from consoles and image clients alike: an `Error`
//! whose `code` names the kind, as Node's own errors carry `ENOENT`.

use napi::{
    Env, JsError, JsValue,
    bindgen_prelude::{Buffer, Object},
};
use virtx::protocol::{Error, ExecResp, Failure};

pub type Result<T> = napi::Result<T, String>;

/// A failure on its way out of a promise: the coded error, and what a command killed at its
/// timeout wrote, which [`into_js`](Self::into_js) sets on the error as `stdout`, `stderr` and
/// `truncated`.
pub struct Thrown {
    error: napi::Error<String>,
    output: Option<ExecResp>,
}

impl From<napi::Error<String>> for Thrown {
    fn from(error: napi::Error<String>) -> Self {
        Thrown {
            error,
            output: None,
        }
    }
}

impl Thrown {
    /// The JavaScript error, made on the main thread since no other can make one.
    pub fn into_js(self, env: &Env) -> napi::Error {
        let error = JsError::from(self.error).into_unknown(*env);
        if let Some(output) = self.output {
            // Best effort: without them it is still the error.
            if let Ok(mut object) = error.coerce_to_object() {
                let _ = set_output(&mut object, output);
            }
        }
        napi::Error::from(error)
    }
}

fn set_output(object: &mut Object, output: ExecResp) -> napi::Result<()> {
    object.set("stdout", Buffer::from(output.stdout))?;
    object.set("stderr", Buffer::from(output.stderr))?;
    object.set("truncated", output.truncated)
}

/// A refusal's code is virtx's name for its number (`TIMED_OUT`, `NOT_FOUND`), so callers
/// compare readable strings; a number with no name is `CONSOLE_REFUSED`, with the number in the
/// message. A broken channel, on which nothing more will be heard, is `CONSOLE_BROKEN`.
pub fn failure(failure: Failure) -> Thrown {
    let output = match &failure {
        Failure::Refused(error) => error.output(),
        Failure::Broken(_) => None,
    };
    let error = match failure {
        Failure::Refused(error) => match name(error.code) {
            Some(name) => napi::Error::new(name.to_string(), error.message),
            None => napi::Error::new(
                "CONSOLE_REFUSED".to_string(),
                format!("{} ({})", error.message, error.code),
            ),
        },
        Failure::Broken(error) => {
            napi::Error::new("CONSOLE_BROKEN".to_string(), format!("{error:#}"))
        }
    };
    Thrown { error, output }
}

/// Building a console may fail with a [`Failure`] underneath (the server refusing `init`),
/// which keeps its code as any refusal does. Anything else is `VIRTX_ERROR`.
pub fn anyhow(error: anyhow::Error) -> napi::Error<String> {
    match error.downcast::<Failure>() {
        // `init` refusals carry no output to lose.
        Ok(f) => failure(f).error,
        Err(error) => napi::Error::new("VIRTX_ERROR".to_string(), format!("{error:#}")),
    }
}

/// Coded by the error's [`ErrorKind`](std::io::ErrorKind) name.
pub fn io(error: std::io::Error) -> napi::Error<String> {
    napi::Error::new(format!("{:?}", error.kind()), error.to_string())
}

pub fn invalid(reason: impl ToString) -> napi::Error<String> {
    napi::Error::new("INVALID_ARG".to_string(), reason)
}

fn name(code: i64) -> Option<&'static str> {
    Some(match code {
        Error::TIMED_OUT => "TIMED_OUT",
        Error::NOT_EXECUTABLE => "NOT_EXECUTABLE",
        Error::BOOT_FAILED => "BOOT_FAILED",
        Error::NOT_FOUND => "NOT_FOUND",
        Error::IS_A_DIRECTORY => "IS_A_DIRECTORY",
        Error::IO_FAILED => "IO_FAILED",
        Error::UNSUPPORTED_MOUNT => "UNSUPPORTED_MOUNT",
        Error::MOUNT_FAILED => "MOUNT_FAILED",
        Error::UNSUPPORTED_NETWORK => "UNSUPPORTED_NETWORK",
        Error::UNSUPPORTED_IMAGE => "UNSUPPORTED_IMAGE",
        Error::UNKNOWN_IMAGE => "UNKNOWN_IMAGE",
        Error::UNSUPPORTED_MACHINE => "UNSUPPORTED_MACHINE",
        Error::UNKNOWN_JOB => "UNKNOWN_JOB",
        Error::TOO_MANY_JOBS => "TOO_MANY_JOBS",
        Error::INVALID_REQUEST => "INVALID_REQUEST",
        Error::METHOD_NOT_FOUND => "METHOD_NOT_FOUND",
        Error::INVALID_PARAMS => "INVALID_PARAMS",
        Error::INTERNAL_ERROR => "INTERNAL_ERROR",
        _ => return None,
    })
}

/// A JavaScript number where virtx takes a `u64` (offset, length, timeout).
///
/// napi converts numbers to `i64`; a negative one is rejected here rather than wrapped into a
/// huge length.
pub fn unsigned(value: Option<i64>, what: &str) -> Result<Option<u64>> {
    value
        .map(|v| u64::try_from(v).map_err(|_| invalid(format!("{what} must not be negative"))))
        .transpose()
}
