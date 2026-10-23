//! [`Client`] behind a C ABI, as `include/virtx.h` declares it.
//!
//! Every fallible function returns 0 on success and -1 on failure, and writes to its `out` a
//! string C frees with [`virtx_string_free`]: the output as JSON on success, the error's
//! message on failure. A null `out` discards it.

use std::ffi::{CStr, CString, c_char, c_int};

use serde::{Serialize, de::DeserializeOwned};
use tokio::{process::Command, runtime::Runtime};

use crate::{Client, image::ImageName};

/// A `Client` and the runtime it runs on: C calls in without one.
pub struct VirtxClient {
    runtime: Runtime,
    client: Client,
}

/// Start `program` with `args_len` arguments from `args` as the server, over its stdin and
/// stdout, and write the client to `client`.
///
/// # Safety
///
/// `program` and each of the `args_len` pointers in `args` are NUL-terminated UTF-8. `client`
/// is writable. `error` is null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn virtx_client_spawn(
    program: *const c_char,
    args: *const *const c_char,
    args_len: usize,
    client: *mut *mut VirtxClient,
    error: *mut *mut c_char,
) -> c_int {
    let opened = (|| {
        let mut command = Command::new(unsafe { text(program) }?);
        for i in 0..args_len {
            command.arg(unsafe { text(*args.add(i)) }?);
        }
        let runtime = Runtime::new().map_err(|e| e.to_string())?;
        let opened = runtime.block_on(async { Client::spawn(command) });
        let opened = opened.map_err(|e| format!("starting the server: {e}"))?;
        Ok(VirtxClient {
            runtime,
            client: opened,
        })
    })();
    unsafe { open(opened, client, error) }
}

/// Reach the server at `url`, an `http` or `https` URL, and write the client to `client`.
///
/// # Safety
///
/// `url` is NUL-terminated UTF-8. `client` is writable. `error` is null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn virtx_client_connect(
    url: *const c_char,
    client: *mut *mut VirtxClient,
    error: *mut *mut c_char,
) -> c_int {
    let opened = (|| {
        let url = unsafe { text(url) }?;
        let runtime = Runtime::new().map_err(|e| e.to_string())?;
        let opened = runtime.block_on(async { Client::connect(url) });
        let opened = opened.map_err(|e| format!("{e:#}"))?;
        Ok(VirtxClient {
            runtime,
            client: opened,
        })
    })();
    unsafe { open(opened, client, error) }
}

/// `image.list`: writes the references to `out`, as
/// `[{"reference": "name:tag", "digest": "algorithm:hex", "size": n}, ...]`.
///
/// # Safety
///
/// `client` is one `virtx_client_spawn` or `virtx_client_connect` wrote, not yet freed. `out`
/// is null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn virtx_client_list_image(
    client: *mut VirtxClient,
    out: *mut *mut c_char,
) -> c_int {
    let VirtxClient { runtime, client } = unsafe { &*client };
    let output = runtime
        .block_on(client.list_image())
        .map_err(|e| e.to_string());
    unsafe { answer(output, out) }
}

/// `image.build`: builds `recipe`, as
/// `{"v": 1, "base": "alpine:3.20", "steps": [{"run": "..."}]}`, and points `reference`
/// (`name:tag`) at it, or a reference the server picks if `reference` is null. Writes
/// `{"reference": "name:tag", "digest": "algorithm:hex"}` to `out`.
///
/// # Safety
///
/// `client` is as in `virtx_client_list_image`. `recipe` is NUL-terminated UTF-8, and
/// `reference` is null or one too. `out` is null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn virtx_client_build_image(
    client: *mut VirtxClient,
    recipe: *const c_char,
    reference: *const c_char,
    out: *mut *mut c_char,
) -> c_int {
    let VirtxClient { runtime, client } = unsafe { &*client };
    let output = (|| {
        let recipe = unsafe { json(recipe) }?;
        let reference = match reference.is_null() {
            true => None,
            false => Some(unsafe { text(reference) }?),
        };
        runtime
            .block_on(client.build_image(recipe, reference))
            .map_err(|e| e.to_string())
    })();
    unsafe { answer(output, out) }
}

/// `image.remove`: forgets `image`, as `{"type": "reference", "reference": "name:tag"}` or
/// `{"type": "digest", "digest": "algorithm:hex"}`. Writes `null` to `out`.
///
/// # Safety
///
/// `client` is as in `virtx_client_list_image`. `image` is NUL-terminated UTF-8. `out` is null
/// or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn virtx_client_remove_image(
    client: *mut VirtxClient,
    image: *const c_char,
    out: *mut *mut c_char,
) -> c_int {
    let VirtxClient { runtime, client } = unsafe { &*client };
    let output = (|| {
        let image: ImageName = unsafe { json(image) }?;
        runtime
            .block_on(client.remove_image(image))
            .map_err(|e| e.to_string())
    })();
    unsafe { answer(output, out) }
}

/// End the session and free `client`. Null does nothing.
///
/// # Safety
///
/// `client` is null, or as in `virtx_client_list_image`; it is not used again.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn virtx_client_free(client: *mut VirtxClient) {
    if client.is_null() {
        return;
    }
    let VirtxClient { runtime, client } = *unsafe { Box::from_raw(client) };
    runtime.block_on(client.close());
}

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
unsafe fn text<'a>(ptr: *const c_char) -> Result<&'a str, String> {
    if ptr.is_null() {
        return Err("a string argument is null".to_string());
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .map_err(|e| format!("a string argument is not UTF-8: {e}"))
}

/// The JSON at `ptr`, read as a `T`.
unsafe fn json<T: DeserializeOwned>(ptr: *const c_char) -> Result<T, String> {
    serde_json::from_str(unsafe { text(ptr) }?).map_err(|e| e.to_string())
}

/// Write `opened` to `client`, or its error to `error`.
unsafe fn open(
    opened: Result<VirtxClient, String>,
    client: *mut *mut VirtxClient,
    error: *mut *mut c_char,
) -> c_int {
    match opened {
        Ok(opened) => {
            unsafe { *client = Box::into_raw(Box::new(opened)) };
            0
        }
        Err(message) => {
            unsafe { write(error, message) };
            -1
        }
    }
}

/// Write `output` as JSON, or its error, to `out`.
unsafe fn answer<T: Serialize>(output: Result<T, String>, out: *mut *mut c_char) -> c_int {
    let (status, written) =
        match output.and_then(|o| serde_json::to_string(&o).map_err(|e| e.to_string())) {
            Ok(json) => (0, json),
            Err(message) => (-1, message),
        };
    unsafe { write(out, written) };
    status
}

/// Write `string` to `out` for C to free, unless `out` is null.
unsafe fn write(out: *mut *mut c_char, string: String) {
    if out.is_null() {
        return;
    }
    // An interior NUL would end the string early in C; it cannot be one.
    let string = CString::new(string.replace('\0', "")).expect("no NUL");
    unsafe { *out = string.into_raw() };
}

#[cfg(test)]
mod tests {
    use std::ptr;

    use super::*;

    #[test]
    fn a_failed_call_writes_its_error() {
        let mut out = ptr::null_mut();
        let status = unsafe { answer::<()>(Err("gone".to_string()), &mut out) };
        assert_eq!(status, -1);
        assert_eq!(unsafe { CStr::from_ptr(out) }.to_str().unwrap(), "gone");
        unsafe { virtx_string_free(out) };
    }

    #[test]
    fn a_call_writes_its_output_as_json() {
        let mut out = ptr::null_mut();
        let status = unsafe { answer(Ok(vec![1, 2]), &mut out) };
        assert_eq!(status, 0);
        assert_eq!(unsafe { CStr::from_ptr(out) }.to_str().unwrap(), "[1,2]");
        unsafe { virtx_string_free(out) };
    }

    #[test]
    fn spawning_nothing_fails_with_a_message() {
        let mut client = ptr::null_mut();
        let mut error = ptr::null_mut();
        let program = c"/nonexistent/virtx-server";
        let status = unsafe {
            virtx_client_spawn(program.as_ptr(), ptr::null(), 0, &mut client, &mut error)
        };
        assert_eq!(status, -1);
        assert!(client.is_null());
        assert!(!error.is_null());
        unsafe { virtx_string_free(error) };
    }
}
