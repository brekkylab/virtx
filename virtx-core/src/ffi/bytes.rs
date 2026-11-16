//! JSON across the ABI: what C hands in, and what the library hands back for C to free.

use std::{ffi::c_int, ptr, slice};

use serde::{Serialize, de::DeserializeOwned};

use super::{VIRTX_ERROR, VIRTX_OK};
use crate::{Error, Source};

/// Why a call failed, as C reads it: `{"message": "...", "source": "server"}`, `source` where
/// it failed (`server`, `job`, `transport` or `client`), with `"status": 404` beside it when
/// the server refused the request with that HTTP status, and `"body"` where the refusal had a
/// body that was not a JSON error.
#[derive(Debug, Serialize)]
pub(crate) struct Failure {
    message: String,
    source: Source,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    body: Option<String>,
}

impl From<Error> for Failure {
    fn from(error: Error) -> Failure {
        let status = error.status();
        let body = match &error {
            Error::Refusal { body, .. } => body.clone(),
            _ => None,
        };
        Failure {
            message: error.to_string(),
            source: error.source_kind(),
            status,
            body,
        }
    }
}

/// Bytes this library wrote, for C to free with `virtx_bytes_free`.
#[repr(C)]
pub struct VirtxBytes {
    pub data: *mut u8,
    pub len: usize,
}

/// Free bytes this library wrote. Null `data` does nothing.
///
/// # Safety
///
/// `bytes` is as this library wrote it, not yet freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn virtx_bytes_free(bytes: VirtxBytes) {
    if !bytes.data.is_null() {
        drop(unsafe { Box::from_raw(ptr::slice_from_raw_parts_mut(bytes.data, bytes.len)) });
    }
}

/// The `len` bytes of JSON at `ptr`, read as a `T`.
pub(crate) unsafe fn json<T: DeserializeOwned>(ptr: *const u8, len: usize) -> Result<T, Error> {
    if ptr.is_null() {
        return Err(Error::Client("a JSON argument is null".into()));
    }
    let bytes = unsafe { slice::from_raw_parts(ptr, len) };
    serde_json::from_slice(bytes).map_err(|e| Error::Client(e.to_string()))
}

/// `output` as a status, and its JSON or its error's.
pub(crate) fn encode<T: Serialize>(output: Result<T, Error>) -> (c_int, Vec<u8>) {
    match output
        .and_then(|output| serde_json::to_vec(&output).map_err(|e| Error::Client(e.to_string())))
    {
        Ok(bytes) => (VIRTX_OK, bytes),
        Err(error) => failure(error),
    }
}

/// `VIRTX_ERROR`, and `error` as a [`Failure`]'s JSON.
pub(crate) fn failure(error: Error) -> (c_int, Vec<u8>) {
    let bytes = serde_json::to_vec(&Failure::from(error)).expect("a failure encodes");
    (VIRTX_ERROR, bytes)
}

/// Write `bytes` to `out` for C to free, unless `out` is null.
pub(crate) unsafe fn write(out: *mut VirtxBytes, bytes: Vec<u8>) {
    if out.is_null() {
        return;
    }
    let bytes = Box::into_raw(bytes.into_boxed_slice());
    let written = VirtxBytes {
        data: bytes.cast(),
        len: bytes.len(),
    };
    unsafe { *out = written };
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn written(error: Error) -> Value {
        let (status, bytes) = failure(error);
        assert_eq!(status, VIRTX_ERROR);
        serde_json::from_slice(&bytes).unwrap()
    }

    #[test]
    fn a_failure_carries_the_status_the_server_refused_with() {
        assert_eq!(
            written(Error::Status(404, "not_found: no such file".into())),
            json!({
                "message": "the server answered 404 Not Found: not_found: no such file",
                "source": "server",
                "status": 404,
            })
        );
        assert_eq!(
            written(Error::Refusal {
                status: 503,
                description: "not a virtx answer (text/html, 51 bytes)".into(),
                body: Some("<html>503</html>".into()),
            }),
            json!({
                "message": "the server answered 503 Service Unavailable; not a virtx answer (text/html, 51 bytes)",
                "source": "server",
                "status": 503,
                "body": "<html>503</html>",
            })
        );
        assert_eq!(
            written(Error::Transport("the session ended".into())),
            json!({"message": "the session ended", "source": "transport"})
        );
        assert_eq!(
            written(Error::Job("step 1 exited 3".into())),
            json!({"message": "step 1 exited 3", "source": "job"})
        );
        assert_eq!(
            written(Error::Client("not JSON".into())),
            json!({"message": "not JSON", "source": "client"})
        );
    }
}
