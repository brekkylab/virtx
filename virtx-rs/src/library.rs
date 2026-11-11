//! The functions of `virtx.h`: `libvirtx_core`'s, in this crate with the `embedded` feature, or
//! else found in the library opened at run time.
//!
//! Opened, one per process: every client is made by the library first opened, which stays open
//! until the process ends. A client handed between modules is only good in the same library, so
//! a module that shares one opens the path [`library_path`] answers.

use std::ffi::{OsStr, OsString, c_char, c_int, c_void};

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

/// The functions listed, each `name: fn(args) -> ret` or one of the three shapes most requests
/// have: `Plain` takes no arguments, `WithArgs` takes `len` bytes of JSON, and `WithData` takes a
/// body of bytes beside the JSON. Each is a field of `Api`, and a symbol of `virtx-core`'s.
macro_rules! api {
    (@each $($name:ident($($arg:ident: $ty:ty),*) $(-> $ret:ty)?;)*) => {
        /// The functions of `virtx.h`; and, opened, the library they were found in.
        pub(crate) struct Api {
            $(pub(crate) $name: unsafe extern "C" fn($($arg: $ty),*) $(-> $ret)?,)*
            #[cfg(not(feature = "embedded"))]
            path: OsString,
            #[cfg(not(feature = "embedded"))]
            _library: libloading::Library,
        }

        #[cfg(feature = "embedded")]
        impl Api {
            /// The functions `virtx-core` defines, linked into this crate.
            fn embedded() -> Api {
                // Declared under the types `virtx.h` gives them; the linker finds each in
                // `virtx-core`, where it is defined without mangling.
                unsafe extern "C" {
                    $(fn $name($($arg: $ty),*) $(-> $ret)?;)*
                }
                Api { $($name,)* }
            }
        }

        #[cfg(not(feature = "embedded"))]
        impl Api {
            fn open(path: &OsStr) -> Result<Api, Error> {
                let failed = |e: libloading::Error| {
                    Error::Library(format!("opening {}: {e}", path.display()))
                };
                // Safety: what runs on load is `libvirtx_core`'s, which has no initializer.
                let library = unsafe { libloading::Library::new(path) }.map_err(failed)?;
                // Safety: each type is the one `virtx.h` declares under that name.
                $(let $name = *unsafe {
                    library.get::<unsafe extern "C" fn($($arg: $ty),*) $(-> $ret)?>(
                        concat!(stringify!($name), "\0").as_bytes(),
                    )
                }
                .map_err(failed)?;)*
                Ok(Api { $($name,)* path: path.to_owned(), _library: library })
            }
        }
    };
    (@expand [$($done:tt)*] $name:ident: Plain, $($rest:tt)*) => {
        api!(@expand [$($done)* $name(client: *mut Opaque) -> *mut Opaque;] $($rest)*);
    };
    (@expand [$($done:tt)*] $name:ident: WithArgs, $($rest:tt)*) => {
        api!(@expand [$($done)*
            $name(client: *mut Opaque, args: *const u8, len: usize) -> *mut Opaque;
        ] $($rest)*);
    };
    (@expand [$($done:tt)*] $name:ident: WithData, $($rest:tt)*) => {
        api!(@expand [$($done)*
            $name(
                client: *mut Opaque,
                args: *const u8,
                args_len: usize,
                data: *const u8,
                data_len: usize
            ) -> *mut Opaque;
        ] $($rest)*);
    };
    (@expand [$($done:tt)*] $name:ident: fn($($arg:ident: $ty:ty),* $(,)?) $(-> $ret:ty)?, $($rest:tt)*) => {
        api!(@expand [$($done)* $name($($arg: $ty),*) $(-> $ret)?;] $($rest)*);
    };
    (@expand [$($done:tt)*]) => {
        api!(@each $($done)*);
    };
    ($($list:tt)*) => {
        api!(@expand [] $($list)*);
    };
}

api! {
    virtx_ensure_virtx: fn() -> *mut Opaque,
    virtx_client_spawn: fn(
        program: *const c_char,
        args: *const *const c_char,
        args_len: usize,
        client: *mut *mut Opaque,
        error: *mut VirtxBytes,
    ) -> c_int,
    virtx_client_connect: fn(
        url: *const c_char,
        auth: *const u8,
        auth_len: usize,
        headers: *const *const c_char,
        headers_len: usize,
        client: *mut *mut Opaque,
        error: *mut VirtxBytes,
    ) -> c_int,
    virtx_client_set_auth: fn(
        client: *mut Opaque,
        auth: *const u8,
        auth_len: usize,
        error: *mut VirtxBytes,
    ) -> c_int,
    virtx_client_set_headers: fn(
        client: *mut Opaque,
        headers: *const *const c_char,
        headers_len: usize,
        error: *mut VirtxBytes,
    ) -> c_int,
    virtx_client_retain: fn(client: *mut Opaque),
    virtx_client_release: fn(client: *mut Opaque) -> *mut Opaque,
    virtx_client_build_image: WithArgs,
    virtx_client_pull_image: WithArgs,
    virtx_client_push_image: WithArgs,
    virtx_client_list_image_job: Plain,
    virtx_client_get_image_job: WithArgs,
    virtx_client_remove_image_job: WithArgs,
    virtx_client_image_job_log: WithArgs,
    virtx_client_list_image: Plain,
    virtx_client_tag_image: WithArgs,
    virtx_client_untag_image: WithArgs,
    virtx_client_remove_image: WithArgs,
    virtx_client_download_image: WithArgs,
    virtx_client_upload_image: WithArgs,
    virtx_client_prune_image: Plain,
    virtx_client_list_snapshot: Plain,
    virtx_client_create_snapshot: WithArgs,
    virtx_client_get_snapshot: WithArgs,
    virtx_client_remove_snapshot: WithArgs,
    virtx_client_list_volume: Plain,
    virtx_client_create_volume: WithArgs,
    virtx_client_get_volume: WithArgs,
    virtx_client_remove_volume: WithArgs,
    virtx_client_list_volume_mount: WithArgs,
    virtx_client_create_volume_mount: WithArgs,
    virtx_client_remove_volume_mount: WithArgs,
    virtx_client_stat_file: WithArgs,
    virtx_client_file_exists: WithArgs,
    virtx_client_is_file: WithArgs,
    virtx_client_is_dir: WithArgs,
    virtx_client_read_dir: WithArgs,
    virtx_client_read_file: WithArgs,
    virtx_client_read_text: WithArgs,
    virtx_client_write_file: WithData,
    virtx_client_create_dir: WithArgs,
    virtx_client_create_dir_all: WithArgs,
    virtx_client_remove_file: WithArgs,
    virtx_client_rename_file: WithArgs,
    virtx_client_copy_file: WithArgs,
    virtx_client_list_sandbox: Plain,
    virtx_client_create_sandbox: WithArgs,
    virtx_client_get_sandbox: WithArgs,
    virtx_client_remove_sandbox: WithArgs,
    virtx_client_list_sandbox_job: WithArgs,
    virtx_client_exec_sandbox: WithArgs,
    virtx_client_get_sandbox_job: WithArgs,
    virtx_client_remove_sandbox_job: WithArgs,
    virtx_client_signal_sandbox_job: WithArgs,
    virtx_client_write_sandbox_job_stdin: WithArgs,
    virtx_client_sandbox_job_log: WithArgs,
    virtx_port_forwarder_open: fn(
        client: *mut Opaque,
        args: *const u8,
        args_len: usize,
        forwarder: *mut *mut Opaque,
        out: *mut VirtxBytes,
    ) -> c_int,
    virtx_port_forwarder_free: fn(forwarder: *mut Opaque),
    virtx_bytes_free: fn(bytes: VirtxBytes),
    virtx_future_poll: fn(future: *mut Opaque, waker: VirtxWaker, out: *mut VirtxBytes) -> c_int,
    virtx_future_wait: fn(future: *mut Opaque, out: *mut VirtxBytes) -> c_int,
    virtx_future_free: fn(future: *mut Opaque),
    virtx_stream_poll_next: fn(stream: *mut Opaque, waker: VirtxWaker, out: *mut VirtxBytes) -> c_int,
    virtx_stream_wait_next: fn(stream: *mut Opaque, out: *mut VirtxBytes) -> c_int,
    virtx_stream_free: fn(stream: *mut Opaque),
}

/// The variable naming the library to open when [`load`] was not called first. Nothing with
/// `embedded`.
pub const LIBRARY_ENV: &str = "VIRTX_CORE_LIB";

/// Open the library at `path` for this process's clients. Opening the one already open does
/// nothing; another is refused, since a process has one. With `embedded`, every path is another.
pub fn load(path: impl AsRef<OsStr>) -> Result<(), Error> {
    let path = path.as_ref();
    #[cfg(feature = "embedded")]
    {
        Err(Error::Library(format!(
            "the library is this crate's, so {} is not opened",
            path.display()
        )))
    }
    #[cfg(not(feature = "embedded"))]
    {
        let mut api = dynamic::API
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
}

/// The path the open library was opened by, as given; `None` before the first client or
/// [`load`], and always with `embedded`, which no other module shares.
pub fn library_path() -> Option<OsString> {
    #[cfg(feature = "embedded")]
    {
        None
    }
    #[cfg(not(feature = "embedded"))]
    {
        let api = dynamic::API
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        api.map(|api| api.path.clone())
    }
}

/// The functions: this crate's, or the open library's, opened first if none is, from
/// `$VIRTX_CORE_LIB` or else the platform's file name, which the system's loader looks up.
pub(crate) fn api() -> Result<&'static Api, Error> {
    #[cfg(feature = "embedded")]
    {
        static API: std::sync::OnceLock<Api> = std::sync::OnceLock::new();
        Ok(API.get_or_init(Api::embedded))
    }
    #[cfg(not(feature = "embedded"))]
    {
        use std::sync::PoisonError;

        if let Some(api) = *dynamic::API.lock().unwrap_or_else(PoisonError::into_inner) {
            return Ok(api);
        }
        let path = std::env::var_os(LIBRARY_ENV).unwrap_or_else(|| dynamic::FILE_NAME.into());
        load(&path)?;
        Ok(dynamic::API
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .expect("just opened"))
    }
}

#[cfg(not(feature = "embedded"))]
mod dynamic {
    use std::sync::Mutex;

    use super::Api;

    /// The library this process opened; never closed, since a client may outlive any one holder.
    pub(super) static API: Mutex<Option<&'static Api>> = Mutex::new(None);

    /// The library's file name, for the system's loader to look up.
    pub(super) const FILE_NAME: &str = if cfg!(windows) {
        "virtx_core.dll"
    } else if cfg!(target_os = "macos") {
        "libvirtx_core.dylib"
    } else {
        "libvirtx_core.so"
    };
}
