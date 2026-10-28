//! `libvirtx_core`, opened at run time, and the functions of `virtx.h` found in it.
//!
//! One per process: every client is made by the library first opened, which stays open until
//! the process ends. A client handed between modules is only good in the same library, so a
//! module that shares one opens the path [`library_path`] answers.

use std::{
    ffi::{OsStr, OsString, c_char, c_int, c_void},
    sync::{Mutex, PoisonError},
};

use crate::Error;

/// `VirtxClient`, `VirtxFuture`, `VirtxStream` and `VirtxPortForwarder`: opaque, held only by
/// pointer.
#[repr(C)]
pub(crate) struct Opaque {
    _private: [u8; 0],
}

/// `VirtxBytes`: bytes the library wrote, freed with `virtx_bytes_free`.
#[repr(C)]
pub(crate) struct VirtxBytes {
    pub data: *mut u8,
    pub len: usize,
}

impl VirtxBytes {
    pub(crate) fn empty() -> VirtxBytes {
        VirtxBytes {
            data: std::ptr::null_mut(),
            len: 0,
        }
    }
}

/// `VirtxWaker`.
#[repr(C)]
pub(crate) struct VirtxWaker {
    pub data: *mut c_void,
    pub wake: unsafe extern "C" fn(data: *mut c_void),
    pub release: Option<unsafe extern "C" fn(data: *mut c_void)>,
}

pub(crate) const VIRTX_OK: c_int = 0;
pub(crate) const VIRTX_ERROR: c_int = -1;
pub(crate) const VIRTX_PENDING: c_int = 1;
pub(crate) const VIRTX_END: c_int = 2;

/// A request with no arguments, and one taking `args_len` bytes of JSON.
type Plain = unsafe extern "C" fn(client: *mut Opaque) -> *mut Opaque;
type WithArgs =
    unsafe extern "C" fn(client: *mut Opaque, args: *const u8, len: usize) -> *mut Opaque;

macro_rules! api {
    ($($name:ident: $ty:ty,)*) => {
        /// The functions of `virtx.h`, and the library they are in.
        pub(crate) struct Api {
            path: OsString,
            $(pub(crate) $name: $ty,)*
            _library: libloading::Library,
        }

        impl Api {
            fn open(path: &OsStr) -> Result<Api, Error> {
                let failed = |e: libloading::Error| {
                    Error::Library(format!("opening {}: {e}", path.display()))
                };
                // Safety: what runs on load is `libvirtx_core`'s, which has no initializer.
                let library = unsafe { libloading::Library::new(path) }.map_err(failed)?;
                // Safety: each type is the one `virtx.h` declares under that name.
                $(let $name = *unsafe {
                    library.get::<$ty>(concat!(stringify!($name), "\0").as_bytes())
                }
                .map_err(failed)?;)*
                Ok(Api { path: path.to_owned(), $($name,)* _library: library })
            }
        }
    };
}

api! {
    virtx_client_spawn: unsafe extern "C" fn(
        program: *const c_char,
        args: *const *const c_char,
        args_len: usize,
        client: *mut *mut Opaque,
        error: *mut VirtxBytes,
    ) -> c_int,
    virtx_client_connect:
        unsafe extern "C" fn(url: *const c_char, client: *mut *mut Opaque, error: *mut VirtxBytes) -> c_int,
    virtx_client_retain: unsafe extern "C" fn(client: *mut Opaque),
    virtx_client_release: unsafe extern "C" fn(client: *mut Opaque),
    virtx_client_build_image: WithArgs,
    virtx_client_list_snapshot: Plain,
    virtx_client_create_snapshot: WithArgs,
    virtx_client_remove_snapshot: WithArgs,
    virtx_client_list_volume: Plain,
    virtx_client_create_volume: WithArgs,
    virtx_client_remove_volume: WithArgs,
    virtx_client_list_volume_mount: WithArgs,
    virtx_client_create_volume_mount: WithArgs,
    virtx_client_remove_volume_mount: WithArgs,
    virtx_client_list_sandbox: Plain,
    virtx_client_init_sandbox: WithArgs,
    virtx_client_exec_sandbox: WithArgs,
    virtx_client_close_sandbox: WithArgs,
    virtx_port_forwarder_open: unsafe extern "C" fn(
        client: *mut Opaque,
        args: *const u8,
        args_len: usize,
        forwarder: *mut *mut Opaque,
        out: *mut VirtxBytes,
    ) -> c_int,
    virtx_port_forwarder_free: unsafe extern "C" fn(forwarder: *mut Opaque),
    virtx_bytes_free: unsafe extern "C" fn(bytes: VirtxBytes),
    virtx_future_poll:
        unsafe extern "C" fn(future: *mut Opaque, waker: VirtxWaker, out: *mut VirtxBytes) -> c_int,
    virtx_future_wait: unsafe extern "C" fn(future: *mut Opaque, out: *mut VirtxBytes) -> c_int,
    virtx_future_free: unsafe extern "C" fn(future: *mut Opaque),
    virtx_stream_poll_next:
        unsafe extern "C" fn(stream: *mut Opaque, waker: VirtxWaker, out: *mut VirtxBytes) -> c_int,
    virtx_stream_wait_next: unsafe extern "C" fn(stream: *mut Opaque, out: *mut VirtxBytes) -> c_int,
    virtx_stream_free: unsafe extern "C" fn(stream: *mut Opaque),
}

/// The library this process opened; never closed, since a client may outlive any one holder.
static API: Mutex<Option<&'static Api>> = Mutex::new(None);

/// The variable naming the library to open when [`load`] was not called first.
pub const LIBRARY_ENV: &str = "VIRTX_CORE_LIB";

/// The library's file name, for the system's loader to look up.
const FILE_NAME: &str = if cfg!(windows) {
    "virtx_core.dll"
} else if cfg!(target_os = "macos") {
    "libvirtx_core.dylib"
} else {
    "libvirtx_core.so"
};

/// Open the library at `path` for this process's clients. Opening the one already open does
/// nothing; another is refused, since a process has one.
pub fn load(path: impl AsRef<OsStr>) -> Result<(), Error> {
    let path = path.as_ref();
    let mut api = API.lock().unwrap_or_else(PoisonError::into_inner);
    match *api {
        Some(open) if open.path == path => Ok(()),
        Some(open) => Err(Error::Library(format!(
            "{} is open already, so {} is not opened",
            open.path.display(),
            path.display()
        ))),
        None => {
            *api = Some(Box::leak(Box::new(Api::open(path)?)));
            Ok(())
        }
    }
}

/// The path the open library was opened by, as given; `None` before the first client or
/// [`load`].
pub fn library_path() -> Option<OsString> {
    let api = API.lock().unwrap_or_else(PoisonError::into_inner);
    api.map(|api| api.path.clone())
}

/// The open library, opening it first if none is: `$VIRTX_CORE_LIB`, or else the platform's
/// file name, which the system's loader looks up.
pub(crate) fn api() -> Result<&'static Api, Error> {
    if let Some(api) = *API.lock().unwrap_or_else(PoisonError::into_inner) {
        return Ok(api);
    }
    let path = std::env::var_os(LIBRARY_ENV).unwrap_or_else(|| FILE_NAME.into());
    load(&path)?;
    Ok(API
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .expect("just opened"))
}
