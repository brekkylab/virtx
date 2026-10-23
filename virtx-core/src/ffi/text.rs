//! Strings across the ABI: what C hands in, and what the library hands back for C to free.

use std::ffi::{CStr, CString, c_char, c_int};

use serde::{Serialize, de::DeserializeOwned};

use super::{VIRTX_ERROR, VIRTX_OK};

/// Free a string this library wrote. Null does nothing.
///
/// # Safety
///
/// `string` is null, or a string this library wrote, not yet freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn virtx_string_free(string: *mut c_char) {
    if !string.is_null() {
        drop(unsafe { CString::from_raw(string) });
    }
}

/// The UTF-8 text at `ptr`.
pub(crate) unsafe fn text<'a>(ptr: *const c_char) -> Result<&'a str, String> {
    if ptr.is_null() {
        return Err("a string argument is null".to_string());
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .map_err(|e| format!("a string argument is not UTF-8: {e}"))
}

/// The JSON at `ptr`, read as a `T`.
pub(crate) unsafe fn json<T: DeserializeOwned>(ptr: *const c_char) -> Result<T, String> {
    serde_json::from_str(unsafe { text(ptr) }?).map_err(|e| e.to_string())
}

/// `output` as a status, and its JSON or its error.
pub(super) fn encode<T: Serialize>(output: Result<T, String>) -> (c_int, String) {
    match output.and_then(|o| serde_json::to_string(&o).map_err(|e| e.to_string())) {
        Ok(json) => (VIRTX_OK, json),
        Err(message) => (VIRTX_ERROR, message),
    }
}

/// Write `string` to `out` for C to free, unless `out` is null.
pub(super) unsafe fn write(out: *mut *mut c_char, string: String) {
    if out.is_null() {
        return;
    }
    // An interior NUL would end the string early in C; it cannot be one.
    let string = CString::new(string.replace('\0', "")).expect("no NUL");
    unsafe { *out = string.into_raw() };
}
