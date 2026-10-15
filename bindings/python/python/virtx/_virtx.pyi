from os import PathLike
from types import TracebackType
from typing import Self

_Path = str | PathLike[str]
_Content = bytes | str

ERROR_CODES: dict[str, int]

class VirtxError(Exception): ...

class ConsoleRefused(VirtxError):
    code: int
    stdout: bytes | None
    """What a command killed at its timeout wrote until then; ``None`` on any other refusal."""
    stderr: bytes | None
    truncated: bool | None

class ConsoleBroken(VirtxError): ...

class Step:
    @staticmethod
    def run(cmd: str) -> Step: ...
    @staticmethod
    def copy(src: _Path, dst: str) -> Step: ...
    @staticmethod
    def env(key: str, value: str) -> Step: ...
    @staticmethod
    def workdir(dir: str) -> Step: ...

class Recipe:
    """A base and the steps over it. Every method returns a new ``Recipe``."""

    def __init__(self, base: str, steps: list[Step | str] = ...) -> None: ...
    @staticmethod
    def from_dockerfile(content: str) -> Recipe: ...
    @property
    def base(self) -> str: ...
    def step(self, step: Step | str) -> Recipe: ...
    def steps(self, steps: list[Step | str]) -> Recipe: ...

class ImageSource:
    """An image to run on: a ``Recipe`` to build, a ref, or a digest. Builds nothing itself."""

    @staticmethod
    def reference(reference: str) -> ImageSource: ...
    @staticmethod
    def digest(digest: str) -> ImageSource: ...
    @staticmethod
    def recipe(recipe: Recipe) -> ImageSource: ...

_ImageSource = ImageSource | Recipe

class BuildImageResult:
    reference: str
    digest: str

class ImageEntry:
    digest: str
    refs: list[str]

class ImageClient:
    """Builds, lists and removes images. Closed by ``close()``, ``async with``, or dropping it."""

    @staticmethod
    async def try_new() -> ImageClient: ...
    @staticmethod
    async def try_from_cmd(cmd: list[str]) -> ImageClient: ...
    async def version(self) -> str: ...
    async def build(self, recipe: Recipe, reference: str | None = None) -> BuildImageResult: ...
    async def list(self) -> list[ImageEntry]: ...
    async def remove(self, image: _ImageSource) -> None: ...
    async def close(self) -> None: ...
    async def __aenter__(self) -> Self: ...
    async def __aexit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        tb: TracebackType | None,
    ) -> None: ...

class Directory:
    """A tree assembled in place. Mounting it with ``HostMount`` takes it: the mount owns the
    tree, and this ``Directory`` is empty afterwards and refuses further use.
    """

    def __init__(self) -> None: ...
    def add_file(self, path: _Path, content: _Content) -> None: ...
    def remove_file(self, path: _Path) -> None: ...
    def mount(self, path: _Path, host_dir: _Path) -> None: ...
    def unmount(self, path: _Path) -> None: ...
    def with_file(self, path: _Path, content: _Content) -> Self: ...
    def with_mount(self, path: _Path, host_dir: _Path) -> Self: ...

class HostMount:
    """A ``Directory`` mounted on this host until ``unmount()`` or until nothing holds it."""

    def __init__(self, fs: Directory, mountpoint: _Path) -> None: ...
    @property
    def mountpoint(self) -> str: ...
    async def unmount(self) -> None:
        """Take the mount down now, whatever consoles still hold it; call it after closing
        them. A repeat call returns at once."""

def mount_support() -> None:
    """Raise ``OSError``, saying what to install, if this host cannot mount.

    ``HostMount`` checks the same before it mounts; this is for knowing ahead.
    """

class ExecResult:
    code: int
    stdout: bytes
    stderr: bytes
    truncated: bool

class ReadResult:
    data: bytes
    size: int

class Chunk:
    """What a job wrote since the last poll. ``code`` is set on the last one, after which the
    job is gone."""

    stdout: bytes
    stderr: bytes
    truncated: bool
    code: int | None

class Job:
    """A spawned command. The server holds it until the poll that carries its ``code``, so poll
    it to its end or discard it; garbage collection does not kill it. As an ``async with``
    block, it is discarded when the block is left."""

    @property
    def id(self) -> int: ...
    async def poll(self, wait_ms: int | None = None, max_bytes: int | None = None) -> Chunk:
        """What the job wrote since the last poll, waiting up to ``wait_ms`` when there is
        nothing yet; ``max_bytes`` bounds each stream. A job killed at its timeout raises
        ``ConsoleRefused``, with its last bytes."""
    async def kill(self, signal: int | None = None) -> None:
        """Send ``signal`` (``SIGKILL`` when ``None``) to the job and everything it started;
        the next poll reports how it ended."""
    async def discard(self) -> None:
        """Kill the job and collect it, dropping what it wrote, so the server no longer holds
        it. A job already gone, or a console already closed, leaves nothing to do."""
    async def __aenter__(self) -> Self: ...
    async def __aexit__(self, exc_type: object, exc: object, tb: object) -> None: ...

class ExecStream:
    """A command's output as it is written: each ``Chunk`` is what arrived since the last, and
    the last carries the ``code``. A command killed at its timeout raises ``ConsoleRefused``
    instead, with its last bytes. Closed with ``aclose()``, or garbage-collected, before its
    command ended, it kills the command."""

    def __aiter__(self) -> Self: ...
    async def __anext__(self) -> Chunk: ...
    async def aclose(self) -> None: ...

class ConsoleClientBuilder:
    """Filled in place and emptied by ``build()``. Every setter returns this builder."""

    def __init__(self) -> None: ...
    def cmd(self, cmd: list[str]) -> Self: ...
    def mount(self, mount: HostMount | _Path, at: _Path) -> Self: ...
    def mount_readonly(self, mount: HostMount | _Path, at: _Path) -> Self: ...
    def image(self, image: _ImageSource) -> Self: ...
    def snapshot(self, snapshot: bytes) -> Self: ...
    def network(self, network: bool) -> Self: ...
    def ports(self, ports: list[str]) -> Self:
        """Ports on the server's machine that lead into the session, as docker's ``-p``
        spells them: ``"8080:80"``, host first."""
    def vcpus(self, vcpus: int) -> Self: ...
    def memory_mib(self, memory_mib: int) -> Self: ...
    def gpu(self, gpu: bool) -> Self: ...
    def gpu_memory_mib(self, gpu_memory_mib: int) -> Self: ...
    def disk_gib(self, disk_gib: int) -> Self: ...
    async def build(self) -> ConsoleClient: ...

class ConsoleClient:
    @staticmethod
    def builder() -> ConsoleClientBuilder: ...
    @property
    def mounts(self) -> list[str]: ...
    async def start(self) -> None: ...
    async def stop(self) -> None: ...
    async def exec(self, cmd: list[str], timeout_ms: int | None = None) -> ExecResult:
        """Run a command to its end. One killed at ``timeout_ms`` raises ``ConsoleRefused``
        with what it wrote until then."""
    async def spawn(self, cmd: list[str], timeout_ms: int | None = None) -> Job:
        """Start a command and return its ``Job`` at once; the console takes other calls while
        it runs."""
    def job(self, id: int) -> Job:
        """A handle to the job this console spawned under ``id``, for one whose ``Job`` was let
        go of. Nothing is asked of the server: an id it does not hold raises ``UNKNOWN_JOB`` at
        the first call."""
    def exec_stream(self, cmd: list[str], timeout_ms: int | None = None) -> ExecStream:
        """Run a command and iterate its output as it is written:
        ``async for chunk in console.exec_stream(cmd)``. Spawned on the first iteration,
        which raises any refusal to start it."""
    async def read(
        self, path: str, offset: int | None = None, len: int | None = None
    ) -> ReadResult: ...
    async def write(self, path: str, data: _Content, offset: int | None = None) -> int: ...
    async def snapshot(self) -> bytes: ...
    async def close(self) -> None:
        """End the session, and return once the server is gone and this console has let go
        of its mounts. Leaving a console to garbage collection does not wait for that."""
    async def __aenter__(self) -> Self: ...
    async def __aexit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        tb: TracebackType | None,
    ) -> None: ...

async def ensure_virtx() -> str:
    """Fetch the console server into virtx's cache if missing; resolves to its directory.

    Console builders and ``ImageClient`` start ``virtx-uvm`` from that cache, which a host
    that installed only this package lacks, so such a host calls this first. An existing
    server, fetched or installed by hand, is left alone.
    """
