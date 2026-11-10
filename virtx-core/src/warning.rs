//! What the library says beside a call's answer, such as a forwarded connection that reached
//! nothing in its sandbox: a warning, handed to the hook its embedder set, which logs it as its
//! language logs. Without one it is dropped.

use std::sync::{PoisonError, RwLock};

type Hook = Box<dyn Fn(&str) + Send + Sync>;

/// What each warning is handed to, set by its embedder; `None` drops them.
static HOOK: RwLock<Option<Hook>> = RwLock::new(None);

/// Hand `message` to the hook, if one is set. On a runtime, from a blocking thread of it: a
/// hook may wait on its language's own thread, which must not hold up the runtime's.
pub(crate) fn warn(message: String) {
    let call = move || {
        if let Some(hook) = &*HOOK.read().unwrap_or_else(PoisonError::into_inner) {
            hook(&message);
        }
    };
    match tokio::runtime::Handle::try_current() {
        Ok(runtime) => drop(runtime.spawn_blocking(call)),
        Err(_) => call(),
    }
}

#[cfg(feature = "ffi")]
pub(crate) mod ffi {
    //! The hook behind the C ABI: a function C names, called with each warning.

    use std::{
        ffi::{CString, c_char},
        sync::PoisonError,
    };

    use super::{HOOK, Hook};

    /// Call `warn` with each warning the library has from now on, in place of the function set
    /// before: a NUL-terminated UTF-8 message for a person, valid only during the call, from
    /// any thread, the library's own included. Null drops them, as the library does until this
    /// is called. One for the process, as the library is.
    ///
    /// `warn` may block, but must not call into this library.
    #[unsafe(no_mangle)]
    pub extern "C" fn virtx_set_warn(warn: Option<unsafe extern "C" fn(message: *const c_char)>) {
        let hook = warn.map(|warn| -> Hook {
            Box::new(move |message: &str| {
                let message = CString::new(message.replace('\0', " ")).expect("no NUL is left");
                unsafe { warn(message.as_ptr()) };
            })
        });
        *HOOK.write().unwrap_or_else(PoisonError::into_inner) = hook;
    }
}
