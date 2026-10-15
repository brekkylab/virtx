//! `ConsoleClient`, its builder, and their results.

use std::{path::PathBuf, sync::Arc};

use pyo3::{exceptions::PyValueError, prelude::*, types::PyBytes};
use pyo3_async_runtimes::tokio::{future_into_py, get_runtime};
use tokio::sync::Mutex;
use virtx::{
    console::{ConsoleClient, ConsoleClientBuilder},
    protocol::{ExecResp, Failure, JobId, Port, ReadResp},
};

use crate::{
    error::{self, VirtxError},
    fs::{Content, MountLike},
    image::ImageSourceLike,
    job::{PyExecStream, PyJob},
};

/// A [`ConsoleClientBuilder`], mutated in place (the Rust one is consumed per call and is not
/// `Clone`) and emptied by `build()`; every method returns this same object, so calls chain.
// The `Mutex` only supplies `Sync`, which a `pyclass` needs and the builder's client factory
// lacks; nothing contends for it.
#[pyclass(name = "ConsoleClientBuilder", module = "virtx")]
pub struct PyConsoleClientBuilder(std::sync::Mutex<Option<ConsoleClientBuilder>>);

impl PyConsoleClientBuilder {
    fn update<'py>(
        slf: PyRef<'py, Self>,
        f: impl FnOnce(ConsoleClientBuilder) -> PyResult<ConsoleClientBuilder>,
    ) -> PyResult<PyRef<'py, Self>> {
        {
            let mut held = slf.0.lock().unwrap();
            let builder = held.take().ok_or_else(built)?;
            *held = Some(f(builder)?);
        }
        Ok(slf)
    }
}

fn built() -> PyErr {
    PyValueError::new_err("this ConsoleClientBuilder has already been built")
}

#[pymethods]
impl PyConsoleClientBuilder {
    #[new]
    fn new() -> Self {
        PyConsoleClientBuilder(std::sync::Mutex::new(Some(ConsoleClientBuilder::new())))
    }

    fn cmd(slf: PyRef<'_, Self>, cmd: Vec<String>) -> PyResult<PyRef<'_, Self>> {
        Self::update(slf, |b| Ok(b.cmd(&cmd)))
    }

    fn mount(slf: PyRef<'_, Self>, mount: MountLike, at: PathBuf) -> PyResult<PyRef<'_, Self>> {
        Self::update(slf, |b| Ok(b.mount(mount.into_mount()?, at)))
    }

    fn mount_readonly(
        slf: PyRef<'_, Self>,
        mount: MountLike,
        at: PathBuf,
    ) -> PyResult<PyRef<'_, Self>> {
        Self::update(slf, |b| Ok(b.mount_readonly(mount.into_mount()?, at)))
    }

    fn image(slf: PyRef<'_, Self>, image: ImageSourceLike) -> PyResult<PyRef<'_, Self>> {
        Self::update(slf, |b| Ok(b.image(image)))
    }

    fn snapshot(slf: PyRef<'_, Self>, snapshot: Vec<u8>) -> PyResult<PyRef<'_, Self>> {
        Self::update(slf, |b| Ok(b.snapshot(snapshot)))
    }

    fn network(slf: PyRef<'_, Self>, network: bool) -> PyResult<PyRef<'_, Self>> {
        Self::update(slf, |b| Ok(b.network(network)))
    }

    /// Ports on the server's machine that lead into the session, as docker's `-p` spells
    /// them: `"8080:80"`, host first.
    fn ports(slf: PyRef<'_, Self>, ports: Vec<String>) -> PyResult<PyRef<'_, Self>> {
        let ports = ports
            .iter()
            .map(|port| {
                port.parse::<Port>()
                    .map_err(|e| PyValueError::new_err(e.to_string()))
            })
            .collect::<PyResult<Vec<_>>>()?;
        Self::update(slf, |b| Ok(b.ports(ports)))
    }

    fn vcpus(slf: PyRef<'_, Self>, vcpus: u8) -> PyResult<PyRef<'_, Self>> {
        Self::update(slf, |b| Ok(b.vcpus(vcpus)))
    }

    fn memory_mib(slf: PyRef<'_, Self>, memory_mib: u32) -> PyResult<PyRef<'_, Self>> {
        Self::update(slf, |b| Ok(b.memory_mib(memory_mib)))
    }

    fn gpu(slf: PyRef<'_, Self>, gpu: bool) -> PyResult<PyRef<'_, Self>> {
        Self::update(slf, |b| Ok(b.gpu(gpu)))
    }

    fn gpu_memory_mib(slf: PyRef<'_, Self>, gpu_memory_mib: u32) -> PyResult<PyRef<'_, Self>> {
        Self::update(slf, |b| Ok(b.gpu_memory_mib(gpu_memory_mib)))
    }

    fn disk_gib(slf: PyRef<'_, Self>, disk_gib: u32) -> PyResult<PyRef<'_, Self>> {
        Self::update(slf, |b| Ok(b.disk_gib(disk_gib)))
    }

    /// Announce the session to the server; the awaitable resolves to the resulting
    /// `ConsoleClient`.
    fn build<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let builder = self.0.lock().unwrap().take().ok_or_else(built)?;
        future_into_py(py, async move {
            let console = builder.build().await.map_err(error::anyhow)?;
            Ok(PyConsoleClient::new(console))
        })
    }
}

/// The console a slot holds, or the error for one that has been closed.
pub(crate) fn held(slot: &mut Option<ConsoleClient>) -> PyResult<&mut ConsoleClient> {
    slot.as_mut()
        .ok_or_else(|| VirtxError::new_err("this console has been closed"))
}

/// A console slot: the console, or `None` once closed.
pub(crate) type Slot = Arc<Mutex<Option<ConsoleClient>>>;

/// Drop the console if `slot` is its last holder, inside the runtime so `quit` goes out.
pub(crate) fn release(slot: &mut Slot) {
    if let Some(slot) = Arc::get_mut(slot) {
        let _entered = get_runtime().enter();
        slot.get_mut().take();
    }
}

#[pyclass(name = "ConsoleClient", module = "virtx", frozen)]
pub struct PyConsoleClient {
    console: Slot,

    /// [`ConsoleClient::mounts`], cached: fixed once the session is announced, and a getter
    /// that awaited the lock would have to be a coroutine.
    ///
    /// `str`, not `Path`: they are session paths, spelled as `read` and `write` take them.
    mounts: Vec<String>,
}

impl PyConsoleClient {
    fn new(console: ConsoleClient) -> Self {
        PyConsoleClient {
            mounts: console
                .mounts()
                .map(|p| p.to_string_lossy().into_owned())
                .collect(),
            console: Arc::new(Mutex::new(Some(console))),
        }
    }
}

impl Drop for PyConsoleClient {
    fn drop(&mut self) {
        release(&mut self.console);
    }
}

#[pymethods]
impl PyConsoleClient {
    #[staticmethod]
    fn builder() -> PyConsoleClientBuilder {
        PyConsoleClientBuilder::new()
    }

    #[getter]
    fn mounts(&self) -> Vec<String> {
        self.mounts.clone()
    }

    fn start<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let console = self.console.clone();
        future_into_py(py, async move {
            let mut slot = console.lock().await;
            held(&mut slot)?.start().await.map_err(error::failure)
        })
    }

    fn stop<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let console = self.console.clone();
        future_into_py(py, async move {
            let mut slot = console.lock().await;
            held(&mut slot)?.stop().await.map_err(error::failure)
        })
    }

    #[pyo3(signature = (cmd, timeout_ms = None))]
    fn exec<'py>(
        &self,
        py: Python<'py>,
        cmd: Vec<String>,
        timeout_ms: Option<u64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let console = self.console.clone();
        future_into_py(py, async move {
            let mut slot = console.lock().await;
            let resp = held(&mut slot)?.exec(cmd, timeout_ms).await;
            resp.map(PyExecResult::from).map_err(error::failure)
        })
    }

    /// Start a command and resolve to its `Job` at once, leaving it running.
    #[pyo3(signature = (cmd, timeout_ms = None))]
    fn spawn<'py>(
        &self,
        py: Python<'py>,
        cmd: Vec<String>,
        timeout_ms: Option<u64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let console = self.console.clone();
        future_into_py(py, async move {
            let id = {
                let mut slot = console.lock().await;
                let job = held(&mut slot)?.spawn(cmd, timeout_ms).await;
                job.map_err(error::failure)?
            };
            Ok(PyJob::new(console, id))
        })
    }

    /// A handle to the job this console spawned under `id`, for one whose `Job` was let go
    /// of. Nothing is asked of the server: an id it does not hold is `UNKNOWN_JOB` at the
    /// first call.
    fn job(&self, id: JobId) -> PyJob {
        PyJob::new(self.console.clone(), id)
    }

    /// Run a command and iterate its output as it is written; spawned on the first
    /// `__anext__`.
    #[pyo3(signature = (cmd, timeout_ms = None))]
    fn exec_stream(&self, cmd: Vec<String>, timeout_ms: Option<u64>) -> PyExecStream {
        PyExecStream::new(self.console.clone(), cmd, timeout_ms)
    }

    #[pyo3(signature = (path, offset = None, len = None))]
    fn read<'py>(
        &self,
        py: Python<'py>,
        path: String,
        offset: Option<u64>,
        len: Option<u64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let console = self.console.clone();
        future_into_py(py, async move {
            let mut slot = console.lock().await;
            let resp = held(&mut slot)?.read(path, offset, len).await;
            resp.map(PyReadResult::from).map_err(error::failure)
        })
    }

    /// Put `data` in a file, answering with the file's size afterwards.
    #[pyo3(signature = (path, data, offset = None))]
    fn write<'py>(
        &self,
        py: Python<'py>,
        path: String,
        data: Content,
        offset: Option<u64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let console = self.console.clone();
        let data = Vec::<u8>::from(data);
        future_into_py(py, async move {
            let mut slot = console.lock().await;
            let resp = held(&mut slot)?.write(path, data, offset).await;
            resp.map(|w| w.size).map_err(error::failure)
        })
    }

    fn snapshot<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let console = self.console.clone();
        future_into_py(py, async move {
            let mut slot = console.lock().await;
            held(&mut slot)?.snapshot().await.map_err(error::failure)
        })
    }

    /// Answer one protocol frame on this console's channel, for an extension that drives this
    /// session as a console of its own (see `virtx::console::Relay`); not for Python callers.
    ///
    /// Runs on this module's runtime, then calls `reply(answer, None)` with the answer frame,
    /// or `reply(None, reason)` when the channel is broken or this console closed. A callback
    /// rather than an awaitable, so the caller needs no event loop: it may be a thread of
    /// another extension's runtime.
    fn _relay(&self, frame: &[u8], reply: Py<PyAny>) {
        let console = self.console.clone();
        let frame = frame.to_vec();
        get_runtime().spawn(async move {
            let answer = match console.lock().await.as_mut() {
                Some(console) => console.relay(&frame).await.map_err(|f| match f {
                    // The other side wraps it as its own broken channel.
                    Failure::Broken(e) => format!("{e:#}"),
                    refused => refused.to_string(),
                }),
                None => Err("this console has been closed".to_string()),
            };
            Python::attach(|py| {
                let (frame, broken) = match answer {
                    Ok(frame) => (Some(PyBytes::new(py, &frame)), None),
                    Err(reason) => (None, Some(reason)),
                };
                if let Err(e) = reply.call1(py, (frame, broken)) {
                    e.write_unraisable(py, Some(reply.bind(py)));
                }
            });
        });
    }

    /// End the session and resolve once the server is gone and its mounts are let go of.
    /// Closing twice is the same as closing once.
    ///
    /// A garbage-collected console ends on a task nothing waits for, so a program exiting right
    /// after may leave its mounts up; close it, or use `async with`, when they must be down.
    fn close<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let console = self.console.clone();
        future_into_py(py, async move {
            let taken = console.lock().await.take();
            if let Some(console) = taken {
                // The server's exit status is not raised: the session is over either way.
                let _ = console.close().await;
            }
            Ok(())
        })
    }

    fn __aenter__<'py>(slf: Bound<'py, Self>) -> PyResult<Bound<'py, PyAny>> {
        let py = slf.py();
        let slf = slf.unbind();
        future_into_py(py, async move { Ok(slf) })
    }

    fn __aexit__<'py>(
        &self,
        py: Python<'py>,
        _exc_type: Bound<'py, PyAny>,
        _exc: Bound<'py, PyAny>,
        _tb: Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.close(py)
    }
}

#[pyclass(name = "ExecResult", module = "virtx", frozen, get_all)]
pub struct PyExecResult {
    code: i32,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    truncated: bool,
}

impl From<ExecResp> for PyExecResult {
    fn from(resp: ExecResp) -> Self {
        PyExecResult {
            code: resp.code,
            stdout: resp.stdout,
            stderr: resp.stderr,
            truncated: resp.truncated,
        }
    }
}

#[pymethods]
impl PyExecResult {
    fn __repr__(&self) -> String {
        format!(
            "ExecResult(code={}, stdout=<{} bytes>, stderr=<{} bytes>, truncated={})",
            self.code,
            self.stdout.len(),
            self.stderr.len(),
            if self.truncated { "True" } else { "False" },
        )
    }
}

#[pyclass(name = "ReadResult", module = "virtx", frozen, get_all)]
pub struct PyReadResult {
    data: Vec<u8>,
    size: u64,
}

impl From<ReadResp> for PyReadResult {
    fn from(resp: ReadResp) -> Self {
        PyReadResult {
            data: resp.data,
            size: resp.size,
        }
    }
}

#[pymethods]
impl PyReadResult {
    fn __repr__(&self) -> String {
        format!(
            "ReadResult(data=<{} bytes>, size={})",
            self.data.len(),
            self.size
        )
    }
}

pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyConsoleClientBuilder>()?;
    m.add_class::<PyConsoleClient>()?;
    m.add_class::<PyExecResult>()?;
    m.add_class::<PyReadResult>()?;
    Ok(())
}
