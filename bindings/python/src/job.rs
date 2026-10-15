//! A command left running: `Job`, the `Chunk`s its output comes back in, and `ExecStream`,
//! which follows one to its end.
//!
//! Both hold a clone of their console's slot and lock it for one call at a time, so the console
//! takes other calls between two polls. `ExecStream` is not virtx's own
//! [`ExecStream`](virtx::console::ExecStream), which borrows the console for its whole life:
//! it spawns and polls through the slot instead.

use std::sync::Arc;

use pyo3::{exceptions::PyStopAsyncIteration, prelude::*};
use pyo3_async_runtimes::tokio::{future_into_py, get_runtime};
use tokio::sync::Mutex;
use virtx::protocol::{JobId, PollResp};

use crate::{
    console::{Slot, held, release},
    error,
};

/// How long one poll of an `ExecStream` waits for output. It answers as soon as any arrives,
/// so this bounds only how often a silent command costs a round trip.
const STREAM_WAIT_MS: u64 = 1000;

/// A spawned command, polled for its output until the poll that carries its `code`.
///
/// Garbage collection does not kill it: a job may outlive the object that started it. As an
/// `async with` block, it is discarded when the block is left; `ConsoleClient.job(id)` makes
/// another handle to it from its id.
#[pyclass(name = "Job", module = "virtx", frozen)]
pub struct PyJob {
    console: Slot,
    #[pyo3(get)]
    id: JobId,
}

impl PyJob {
    pub fn new(console: Slot, id: JobId) -> Self {
        PyJob { console, id }
    }
}

impl Drop for PyJob {
    fn drop(&mut self) {
        release(&mut self.console);
    }
}

#[pymethods]
impl PyJob {
    /// What the job wrote since the last poll, and its `code` once it has ended.
    #[pyo3(signature = (wait_ms = None, max_bytes = None))]
    fn poll<'py>(
        &self,
        py: Python<'py>,
        wait_ms: Option<u64>,
        max_bytes: Option<u64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let (console, id) = (self.console.clone(), self.id);
        future_into_py(py, async move {
            let mut slot = console.lock().await;
            let resp = held(&mut slot)?.poll(id, wait_ms, max_bytes).await;
            resp.map(PyChunk::from).map_err(error::failure)
        })
    }

    /// Send `signal` (`None` for `SIGKILL`); the next poll reports how the job ended.
    #[pyo3(signature = (signal = None))]
    fn kill<'py>(&self, py: Python<'py>, signal: Option<i32>) -> PyResult<Bound<'py, PyAny>> {
        let (console, id) = (self.console.clone(), self.id);
        future_into_py(py, async move {
            let mut slot = console.lock().await;
            held(&mut slot)?
                .kill(id, signal)
                .await
                .map_err(error::failure)
        })
    }

    /// Kill the job and collect it, dropping what it wrote, so the server no longer holds it.
    /// A job already gone, or a console already closed, leaves nothing to do.
    fn discard<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let (console, id) = (self.console.clone(), self.id);
        future_into_py(py, async move { discard(&console, id).await })
    }

    fn __aenter__<'py>(slf: Bound<'py, Self>) -> PyResult<Bound<'py, PyAny>> {
        let py = slf.py();
        let slf = slf.unbind();
        future_into_py(py, async move { Ok(slf) })
    }

    /// Leaving the block discards the job, whether or not it raised; the exception, if any,
    /// goes on.
    fn __aexit__<'py>(
        &self,
        py: Python<'py>,
        _exc_type: Bound<'py, PyAny>,
        _exc: Bound<'py, PyAny>,
        _tb: Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.discard(py)
    }

    fn __repr__(&self) -> String {
        format!("Job(id={})", self.id)
    }
}

#[pyclass(name = "Chunk", module = "virtx", frozen, get_all)]
pub struct PyChunk {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    truncated: bool,
    code: Option<i32>,
}

impl From<PollResp> for PyChunk {
    fn from(resp: PollResp) -> Self {
        PyChunk {
            stdout: resp.stdout,
            stderr: resp.stderr,
            truncated: resp.truncated,
            code: resp.code,
        }
    }
}

#[pymethods]
impl PyChunk {
    fn __repr__(&self) -> String {
        format!(
            "Chunk(stdout=<{} bytes>, stderr=<{} bytes>, truncated={}, code={})",
            self.stdout.len(),
            self.stderr.len(),
            if self.truncated { "True" } else { "False" },
            self.code.map_or("None".to_string(), |c| c.to_string()),
        )
    }
}

/// Where an [`ExecStream`](PyExecStream) is.
enum Streaming {
    /// Not spawned yet, so a refusal to spawn is raised by the first `__anext__`.
    Ready(Vec<String>, Option<u64>),
    Running(JobId),
    /// Ended, refused or closed: nothing left to poll.
    Done,
}

struct Stream {
    console: Slot,
    state: Mutex<Streaming>,
}

impl Stream {
    /// The next poll that brought anything, or `None` once the job has ended.
    async fn next(&self) -> PyResult<Option<PollResp>> {
        let mut state = self.state.lock().await;
        loop {
            let job = match &mut *state {
                Streaming::Done => return Ok(None),
                Streaming::Running(job) => *job,
                Streaming::Ready(cmd, timeout_ms) => {
                    let (cmd, timeout_ms) = (std::mem::take(cmd), *timeout_ms);
                    *state = Streaming::Done;
                    let mut slot = self.console.lock().await;
                    let job = held(&mut slot)?.spawn(cmd, timeout_ms).await;
                    let job = job.map_err(error::failure)?;
                    *state = Streaming::Running(job);
                    job
                }
            };
            let answer = {
                let mut slot = self.console.lock().await;
                match held(&mut slot) {
                    Ok(console) => console
                        .poll(job, Some(STREAM_WAIT_MS), None)
                        .await
                        .map_err(error::failure),
                    Err(closed) => Err(closed),
                }
            };
            // A refusal is the job's end (a timeout) or its absence: nothing more to poll.
            let chunk = answer.inspect_err(|_| *state = Streaming::Done)?;
            if chunk.code.is_some() {
                *state = Streaming::Done;
            }
            if !chunk.is_empty() {
                return Ok(Some(chunk));
            }
        }
    }

    /// Kill the job if it is still running, and collect it.
    async fn close(&self) {
        let mut state = self.state.lock().await;
        if let Streaming::Running(job) = std::mem::replace(&mut *state, Streaming::Done) {
            reap(&self.console, job).await;
        }
    }
}

/// A stream let go of while its job runs kills the job, on the runtime since this may run in
/// a Python finalizer.
impl Drop for Stream {
    fn drop(&mut self) {
        if let Streaming::Running(job) = std::mem::replace(self.state.get_mut(), Streaming::Done) {
            let console = self.console.clone();
            get_runtime().spawn(async move { reap(&console, job).await });
        }
        release(&mut self.console);
    }
}

/// [`discard`](virtx::console::ConsoleClient::discard) `job`. A closed console has ended its
/// jobs with it, so there is nothing to do.
async fn discard(console: &Slot, job: JobId) -> PyResult<()> {
    match console.lock().await.as_mut() {
        Some(console) => console.discard(job).await.map_err(error::failure),
        None => Ok(()),
    }
}

/// [`discard`] for a stream let go of, where a broken channel has nobody to tell.
async fn reap(console: &Slot, job: JobId) {
    let _ = discard(console, job).await;
}

/// A command's output as it is written: an async iterator of `Chunk`s, the last carrying the
/// `code`. A command killed at its timeout raises `ConsoleRefused` instead, with its last
/// bytes. Closed, or garbage-collected, before its command ended, it kills the command.
#[pyclass(name = "ExecStream", module = "virtx", frozen)]
pub struct PyExecStream(Arc<Stream>);

impl PyExecStream {
    pub fn new(console: Slot, cmd: Vec<String>, timeout_ms: Option<u64>) -> Self {
        PyExecStream(Arc::new(Stream {
            console,
            state: Mutex::new(Streaming::Ready(cmd, timeout_ms)),
        }))
    }
}

#[pymethods]
impl PyExecStream {
    fn __aiter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __anext__<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let stream = self.0.clone();
        future_into_py(py, async move {
            match stream.next().await? {
                Some(chunk) => Ok(PyChunk::from(chunk)),
                None => Err(PyStopAsyncIteration::new_err(())),
            }
        })
    }

    /// Kill the command if it is still running, and resolve once the server has let it go.
    fn aclose<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let stream = self.0.clone();
        future_into_py(py, async move {
            stream.close().await;
            Ok(())
        })
    }
}

pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyJob>()?;
    m.add_class::<PyChunk>()?;
    m.add_class::<PyExecStream>()?;
    Ok(())
}
