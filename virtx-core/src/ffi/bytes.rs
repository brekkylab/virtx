//! JSON across the ABI: what C hands in, and what the library hands back for C to free.

use std::{ffi::c_int, ptr, slice};

use serde::{Serialize, de::DeserializeOwned};

use super::{VIRTX_ERROR, VIRTX_OK};

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
pub(crate) unsafe fn json<T: DeserializeOwned>(ptr: *const u8, len: usize) -> Result<T, String> {
    if ptr.is_null() {
        return Err("a JSON argument is null".to_string());
    }
    let bytes = unsafe { slice::from_raw_parts(ptr, len) };
    serde_json::from_slice(bytes).map_err(|e| e.to_string())
}

/// `output` as a status, and its JSON or its error's.
pub(crate) fn encode<T: Serialize>(output: Result<T, String>) -> (c_int, Vec<u8>) {
    let encoded = output.and_then(|output| serde_json::to_vec(&output).map_err(|e| e.to_string()));
    match encoded {
        Ok(bytes) => (VIRTX_OK, bytes),
        Err(message) => failure(message),
    }
}

/// `VIRTX_ERROR`, and `message` as a JSON string.
pub(super) fn failure(message: impl Into<String>) -> (c_int, Vec<u8>) {
    let bytes = serde_json::to_vec(&message.into()).expect("a string encodes");
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
