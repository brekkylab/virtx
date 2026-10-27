//! The waker C hands a poll, which a future or a stream wakes once it has something to take.

use std::{
    ffi::c_void,
    thread::{self, Thread},
};

use super::VIRTX_PENDING;

/// What a pending future or stream wakes once it has something to take.
///
/// Every waker handed to a poll is the library's from then on: it calls `wake` at most once,
/// from any thread, and then `release`, unless null, exactly once. `data` stays valid until
/// `release`. `wake` only schedules a poll: it must not call into this library.
#[repr(C)]
pub struct VirtxWaker {
    pub data: *mut c_void,
    pub wake: unsafe extern "C" fn(data: *mut c_void),
    pub release: Option<unsafe extern "C" fn(data: *mut c_void)>,
}

/// A `VirtxWaker`, released when dropped.
pub(super) struct Waker(VirtxWaker);

// `data` is C's to make usable from any thread, as `VirtxWaker` requires.
unsafe impl Send for Waker {}

impl Waker {
    pub(super) fn new(waker: VirtxWaker) -> Waker {
        Waker(waker)
    }

    pub(super) fn wake(self) {
        unsafe { (self.0.wake)(self.0.data) };
    }
}

impl Drop for Waker {
    fn drop(&mut self) {
        if let Some(release) = self.0.release {
            unsafe { release(self.0.data) };
        }
    }
}

/// Poll with a waker that unparks this thread, parking until woken, until `poll` answers other
/// than `VIRTX_PENDING`.
pub(super) fn block_on(mut poll: impl FnMut(VirtxWaker) -> i32) -> i32 {
    unsafe extern "C" fn unpark(data: *mut c_void) {
        unsafe { &*data.cast::<Thread>() }.unpark();
    }
    unsafe extern "C" fn release(data: *mut c_void) {
        drop(unsafe { Box::from_raw(data.cast::<Thread>()) });
    }
    loop {
        let waker = VirtxWaker {
            data: Box::into_raw(Box::new(thread::current())).cast(),
            wake: unpark,
            release: Some(release),
        };
        match poll(waker) {
            // A wake before the park leaves its token, so the park returns at once.
            VIRTX_PENDING => thread::park(),
            status => return status,
        }
    }
}

#[cfg(test)]
pub(super) mod testing {
    //! A waker that counts its wakes and releases, for the tests of what wakes one.

    use std::{
        ffi::c_void,
        ptr, slice,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering::SeqCst},
        },
        thread,
        time::{Duration, Instant},
    };

    use serde_json::Value;

    use super::VirtxWaker;
    use crate::ffi::{VirtxBytes, virtx_bytes_free};

    #[derive(Default)]
    pub struct Counts {
        wakes: AtomicUsize,
        releases: AtomicUsize,
    }

    impl Counts {
        pub fn get(&self) -> (usize, usize) {
            (self.wakes.load(SeqCst), self.releases.load(SeqCst))
        }

        /// Until `wakes` reaches `n`, which a runtime thread does.
        pub fn await_wakes(&self, n: usize) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while self.wakes.load(SeqCst) < n {
                assert!(Instant::now() < deadline, "never woken");
                thread::sleep(Duration::from_millis(1));
            }
        }
    }

    pub fn waker(counts: &Arc<Counts>) -> VirtxWaker {
        unsafe extern "C" fn wake(data: *mut c_void) {
            unsafe { &*data.cast::<Counts>() }
                .wakes
                .fetch_add(1, SeqCst);
        }
        unsafe extern "C" fn release(data: *mut c_void) {
            let counts = unsafe { Arc::from_raw(data.cast::<Counts>()) };
            counts.releases.fetch_add(1, SeqCst);
        }
        VirtxWaker {
            data: Arc::into_raw(Arc::clone(counts)).cast_mut().cast(),
            wake,
            release: Some(release),
        }
    }

    /// Where the library writes nothing until it writes bytes.
    pub fn empty() -> VirtxBytes {
        VirtxBytes {
            data: ptr::null_mut(),
            len: 0,
        }
    }

    /// The JSON the library wrote to `out`, read and freed.
    pub unsafe fn taken(out: VirtxBytes) -> Option<Value> {
        if out.data.is_null() {
            return None;
        }
        let bytes = unsafe { slice::from_raw_parts(out.data, out.len) };
        let value = serde_json::from_slice(bytes).unwrap();
        unsafe { virtx_bytes_free(out) };
        Some(value)
    }
}
