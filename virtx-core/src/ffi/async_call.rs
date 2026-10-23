//! A request in flight, the poll and waker C drives it with, and the progress it queues.

use std::{
    collections::VecDeque,
    ffi::{c_char, c_int, c_void},
    future::Future,
    mem,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    thread::{self, Thread},
};

use serde::Serialize;
use tokio::{
    runtime::Handle,
    task::{AbortHandle, JoinSet},
};

use super::{
    VIRTX_ERROR, VIRTX_OK, VIRTX_PENDING,
    text::{encode, write},
};

/// What a pending call wakes once it has an answer.
///
/// Every waker handed to `virtx_call_poll` is the library's from then on: it calls `wake` at
/// most once, from any thread, and then `release`, unless null, exactly once. `data` stays
/// valid until `release`. `wake` only schedules a poll: it must not call into this library.
#[repr(C)]
pub struct VirtxWaker {
    pub data: *mut c_void,
    pub wake: unsafe extern "C" fn(data: *mut c_void),
    pub release: Option<unsafe extern "C" fn(data: *mut c_void)>,
}

/// A request in flight: polled until it answers, then freed. Its progress, if it sends any,
/// is taken with `virtx_call_progress`.
pub struct VirtxCall {
    shared: Arc<Mutex<Shared>>,
    task: AbortHandle,
}

/// What a call's task and C share.
struct Shared {
    state: State,
    /// Progress not yet taken, as JSON, oldest first.
    progress: VecDeque<String>,
    /// Progress came while no waker was kept to wake: the next poll wakes at once.
    unseen: bool,
}

enum State {
    /// Running, with the waker of the last poll.
    Pending(Option<Waker>),
    /// Answered, not yet polled.
    Ready(c_int, String),
    /// Answered and polled, or freed.
    Taken,
}

/// A `VirtxWaker`, released when dropped.
struct Waker(VirtxWaker);

// `data` is C's to make usable from any thread, as `VirtxWaker` requires.
unsafe impl Send for Waker {}

impl Waker {
    fn wake(self) {
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

fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Where a call's task queues its progress, for C to take with `virtx_call_progress`.
#[derive(Clone)]
pub(crate) struct ProgressQueue(Arc<Mutex<Shared>>);

impl ProgressQueue {
    /// Queue `value`, as JSON, and wake the waker kept, if any.
    pub(crate) fn push(&self, value: &impl Serialize) {
        let Ok(value) = serde_json::to_string(value) else {
            return;
        };
        let waker = {
            let mut shared = lock(&self.0);
            shared.progress.push_back(value);
            let waker = match &mut shared.state {
                State::Pending(waker) => waker.take(),
                _ => None,
            };
            shared.unseen = waker.is_none();
            waker
        };
        // Outside the lock, as in `Completion::finish`.
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

/// Answers a call: by `finish`, or with an error when dropped first (its task cancelled by the
/// client's release, or panicked).
struct Completion(Arc<Mutex<Shared>>);

impl Completion {
    fn finish(&self, status: c_int, output: String) {
        let waker = {
            let state = &mut lock(&self.0).state;
            match mem::replace(state, State::Taken) {
                State::Pending(waker) => {
                    *state = State::Ready(status, output);
                    waker
                }
                done => {
                    *state = done;
                    None
                }
            }
        };
        // Outside the lock, so a `wake` that polls at once finds the answer.
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

impl Drop for Completion {
    fn drop(&mut self) {
        self.finish(VIRTX_ERROR, "the call ended before it answered".to_string());
    }
}

/// Run the call `make` makes, given where to queue its progress, on `runtime`, tracked in
/// `calls`, and hand C the call.
pub(super) fn start<T: Serialize, F>(
    runtime: &Handle,
    calls: &Mutex<JoinSet<()>>,
    make: impl FnOnce(ProgressQueue) -> F,
) -> *mut VirtxCall
where
    F: Future<Output = Result<T, String>> + Send + 'static,
{
    let shared = Arc::new(Mutex::new(Shared {
        state: State::Pending(None),
        progress: VecDeque::new(),
        unseen: false,
    }));
    let call = make(ProgressQueue(Arc::clone(&shared)));
    let completion = Completion(Arc::clone(&shared));
    let mut calls = calls.lock().unwrap_or_else(PoisonError::into_inner);
    // Answered calls stay in the set until reaped.
    while calls.try_join_next().is_some() {}
    let task = calls.spawn_on(
        async move {
            let (status, output) = encode(call.await);
            completion.finish(status, output);
        },
        runtime,
    );
    Box::into_raw(Box::new(VirtxCall { shared, task }))
}

/// Take `call`'s answer: `VIRTX_OK` or `VIRTX_ERROR`, with the output or the error written to
/// `out`, or `VIRTX_PENDING`, keeping `waker` (and releasing the one kept before) to wake once
/// there is one, or progress to take. A call answers once; polled again, it fails.
///
/// # Safety
///
/// `call` is one a request returned, not yet freed, and polled by one thread at a time.
/// `waker` is as `VirtxWaker` says. `out` is null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn virtx_call_poll(
    call: *mut VirtxCall,
    waker: VirtxWaker,
    out: *mut *mut c_char,
) -> c_int {
    let waker = Waker(waker);
    let mut shared = lock(&unsafe { &*call }.shared);
    let (status, output) = match mem::replace(&mut shared.state, State::Taken) {
        // Progress came since the last poll, with no waker to tell: tell this one.
        State::Pending(kept) if mem::take(&mut shared.unseen) => {
            shared.state = State::Pending(None);
            drop(shared);
            drop(kept);
            waker.wake();
            return VIRTX_PENDING;
        }
        State::Pending(kept) => {
            shared.state = State::Pending(Some(waker));
            drop(shared);
            drop(kept);
            return VIRTX_PENDING;
        }
        State::Ready(status, output) => (status, output),
        State::Taken => (VIRTX_ERROR, "the call already answered".to_string()),
    };
    drop(shared);
    drop(waker);
    unsafe { write(out, output) };
    status
}

/// Take `call`'s oldest progress not yet taken: `VIRTX_OK`, with it written to `out` as JSON,
/// or `VIRTX_PENDING` if there is none now. Every progress a call sends is queued before it
/// answers, so what is left once it has is taken here too.
///
/// # Safety
///
/// As `virtx_call_poll`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn virtx_call_progress(call: *mut VirtxCall, out: *mut *mut c_char) -> c_int {
    let progress = lock(&unsafe { &*call }.shared).progress.pop_front();
    match progress {
        Some(progress) => {
            unsafe { write(out, progress) };
            VIRTX_OK
        }
        None => VIRTX_PENDING,
    }
}

/// Block until `call` answers, and take its answer as `virtx_call_poll` does. Its progress
/// stays queued, for `virtx_call_progress`.
///
/// # Safety
///
/// As `virtx_call_poll`. Not called from a `wake`, which would block the runtime it runs on.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn virtx_call_wait(call: *mut VirtxCall, out: *mut *mut c_char) -> c_int {
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
        match unsafe { virtx_call_poll(call, waker, out) } {
            // A wake before the park leaves its token, so the park returns at once.
            VIRTX_PENDING => thread::park(),
            status => return status,
        }
    }
}

/// Free `call`, cancelling it if it has not answered, and release its waker without waking it.
/// A `wake` already under way may still finish after this returns. Null does nothing.
///
/// # Safety
///
/// `call` is null, or one a request returned, not yet freed; it is not used again.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn virtx_call_free(call: *mut VirtxCall) {
    if call.is_null() {
        return;
    }
    let call = unsafe { Box::from_raw(call) };
    let kept = mem::replace(&mut lock(&call.shared).state, State::Taken);
    drop(kept);
    call.task.abort();
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::CStr,
        ptr,
        sync::atomic::{AtomicUsize, Ordering::SeqCst},
        time::{Duration, Instant},
    };

    use tokio::{runtime::Runtime, sync::oneshot};

    use super::{super::virtx_string_free, *};

    #[derive(Default)]
    struct Counts {
        wakes: AtomicUsize,
        releases: AtomicUsize,
    }

    impl Counts {
        fn get(&self) -> (usize, usize) {
            (self.wakes.load(SeqCst), self.releases.load(SeqCst))
        }

        /// Until `wakes` reaches `n`, which a runtime thread does.
        fn await_wakes(&self, n: usize) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while self.wakes.load(SeqCst) < n {
                assert!(Instant::now() < deadline, "never woken");
                thread::sleep(Duration::from_millis(1));
            }
        }
    }

    fn waker(counts: &Arc<Counts>) -> VirtxWaker {
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

    fn poll(call: *mut VirtxCall, counts: &Arc<Counts>) -> (c_int, Option<String>) {
        let mut out = ptr::null_mut();
        let status = unsafe { virtx_call_poll(call, waker(counts), &mut out) };
        (status, unsafe { taken(out) })
    }

    unsafe fn taken(out: *mut c_char) -> Option<String> {
        if out.is_null() {
            return None;
        }
        let text = unsafe { CStr::from_ptr(out) }.to_str().unwrap().to_owned();
        unsafe { virtx_string_free(out) };
        Some(text)
    }

    /// A call that answers what `send` is given.
    fn pending(
        runtime: &Runtime,
        calls: &Mutex<JoinSet<()>>,
    ) -> (*mut VirtxCall, oneshot::Sender<u32>) {
        let (send, receive) = oneshot::channel();
        let call = start(runtime.handle(), calls, |_| async move {
            receive.await.map_err(|e| e.to_string())
        });
        (call, send)
    }

    fn progress(call: *mut VirtxCall) -> (c_int, Option<String>) {
        let mut out = ptr::null_mut();
        let status = unsafe { virtx_call_progress(call, &mut out) };
        (status, unsafe { taken(out) })
    }

    #[test]
    fn progress_wakes_the_call_and_is_taken_in_order_even_once_answered() {
        let runtime = Runtime::new().unwrap();
        let calls = Mutex::new(JoinSet::new());
        let (send, receive) = oneshot::channel::<()>();
        let (sent, has_sent) = oneshot::channel::<()>();
        let call = start(runtime.handle(), &calls, |progress| async move {
            progress.push(&1);
            let _ = sent.send(());
            receive.await.map_err(|e| e.to_string())?;
            progress.push(&2);
            Ok(3)
        });
        let counts: Arc<Counts> = Arc::default();

        runtime.block_on(has_sent).unwrap();
        // 1 came with no waker kept: the poll that keeps one wakes it at once.
        assert_eq!(poll(call, &counts), (VIRTX_PENDING, None));
        assert_eq!(counts.get(), (1, 1));
        assert_eq!(progress(call), (VIRTX_OK, Some("1".to_string())));
        assert_eq!(progress(call), (VIRTX_PENDING, None));

        assert_eq!(poll(call, &counts), (VIRTX_PENDING, None));
        send.send(()).unwrap();
        counts.await_wakes(2);
        let mut out = ptr::null_mut();
        assert_eq!(unsafe { virtx_call_wait(call, &mut out) }, VIRTX_OK);
        assert_eq!(unsafe { taken(out) }, Some("3".to_string()));
        assert_eq!(progress(call), (VIRTX_OK, Some("2".to_string())));
        assert_eq!(progress(call), (VIRTX_PENDING, None));

        unsafe { virtx_call_free(call) };
    }

    #[test]
    fn a_call_wakes_its_last_waker_once_and_answers_once() {
        let runtime = Runtime::new().unwrap();
        let calls = Mutex::new(JoinSet::new());
        let (call, send) = pending(&runtime, &calls);
        let (first, last, after) = Default::default();
        let (first, last, after): (Arc<Counts>, Arc<Counts>, Arc<Counts>) = (first, last, after);

        assert_eq!(poll(call, &first), (VIRTX_PENDING, None));
        assert_eq!(poll(call, &last), (VIRTX_PENDING, None));
        assert_eq!(first.get(), (0, 1), "replaced, so released unwoken");

        send.send(7).unwrap();
        last.await_wakes(1);
        assert_eq!(last.get(), (1, 1));

        assert_eq!(poll(call, &after), (0, Some("7".to_string())));
        assert_eq!(poll(call, &after).0, VIRTX_ERROR);
        assert_eq!(after.get(), (0, 2), "a ready poll keeps no waker");

        unsafe { virtx_call_free(call) };
    }

    #[test]
    fn a_freed_call_releases_its_waker_unwoken() {
        let runtime = Runtime::new().unwrap();
        let calls = Mutex::new(JoinSet::new());
        let (call, send) = pending(&runtime, &calls);
        let counts = Arc::default();

        assert_eq!(poll(call, &counts), (VIRTX_PENDING, None));
        unsafe { virtx_call_free(call) };
        let _ = send.send(7);
        runtime.block_on(calls.into_inner().unwrap().shutdown());
        assert_eq!(counts.get(), (0, 1));
    }

    #[test]
    fn a_call_cancelled_with_its_client_answers_with_an_error() {
        let runtime = Runtime::new().unwrap();
        let calls = Mutex::new(JoinSet::new());
        let (call, _send) = pending(&runtime, &calls);
        let counts = Arc::default();

        assert_eq!(poll(call, &counts), (VIRTX_PENDING, None));
        runtime.block_on(calls.lock().unwrap().shutdown());
        assert_eq!(counts.get(), (1, 1));
        assert_eq!(poll(call, &counts).0, VIRTX_ERROR);

        unsafe { virtx_call_free(call) };
    }

    #[test]
    fn waiting_blocks_until_the_call_answers() {
        let runtime = Runtime::new().unwrap();
        let calls = Mutex::new(JoinSet::new());
        let (call, send) = pending(&runtime, &calls);

        let sender = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            send.send(7).unwrap();
        });
        let mut out = ptr::null_mut();
        let status = unsafe { virtx_call_wait(call, &mut out) };
        assert_eq!((status, unsafe { taken(out) }), (0, Some("7".to_string())));

        sender.join().unwrap();
        unsafe { virtx_call_free(call) };
    }
}
