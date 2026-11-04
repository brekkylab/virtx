"""Sandboxes: a VM booted from an image, with volumes mounted into it and a disk of its own that
outlives the VM -- it stops when unused or told to, and starts again as it was left, under the
same id, until it is deleted; and the processes run in one, each a job whose log is its output."""

from __future__ import annotations

from collections.abc import AsyncIterator, Iterable, Mapping
from dataclasses import dataclass, field
from enum import Enum
from typing import TYPE_CHECKING, Any

from ._library import VirtxError, args
from .files import Files
from .image import ImageJob, ImageJobEntry
from .job import LogEntry, StartedJob, Truncated
from .volume import Volume

if TYPE_CHECKING:
    from typing_extensions import Self

    from .client import Client


@dataclass(frozen=True)
class SandboxPort:
    """A TCP port in a sandbox, opened as it was made, and the ``url`` a ``PortForwarder``
    reaches it by."""

    port: int
    url: str

    @classmethod
    def from_dict(cls, value: Mapping[str, Any]) -> SandboxPort:
        return cls(value["port"], value["url"])


@dataclass(frozen=True)
class SandboxVolume:
    """A volume, a ``Volume`` or its name, mounted at ``path`` in a sandbox."""

    volume: str | Volume
    path: str
    readonly: bool = False

    def to_dict(self) -> dict[str, Any]:
        """As the wire has it."""
        volume = self.volume.name if isinstance(self.volume, Volume) else self.volume
        return {"volume": volume, "path": self.path, "readonly": self.readonly}


class SandboxState(str, Enum):
    """Whether a sandbox's VM is up."""

    RUNNING = "running"
    STOPPED = "stopped"
    """Its VM is down and its disk kept: a job started in it, or ``Sandbox.start``, starts
    it."""


@dataclass(frozen=True)
class Sandbox(Files):
    """One sandbox the server holds, running or stopped, and the client that reaches it: what
    starts and stops it, runs processes in it and reads and writes its files while it runs.
    Dropping it leaves the sandbox; ``remove`` deletes it, as leaving ``async with`` does:
    ``async with await client.create_sandbox(...) as sandbox:`` makes one for a block."""

    client: Client = field(repr=False, compare=False)
    """The client its requests go through."""

    id: str

    name: str | None
    """The name it was given, which ``find_sandbox`` finds it by."""

    image: str
    """The digest of the image it stands on, as ``algorithm:hex``: what it boots every time it
    starts, whatever its tag points at by then."""

    state: SandboxState

    ephemeral: bool
    """Whether it is deleted when it stops, rather than kept."""

    auto_stop_minutes: int
    """How long it runs unused before it stops."""

    auto_delete_days: int | None
    """How long it is kept stopped and unused before it is deleted; ``None`` keeps it until it
    is deleted."""

    used_at: str
    """When it was last used, as RFC 3339."""

    ports: list[SandboxPort] = field(default_factory=list)
    """Its opened ports: answered by ``create_sandbox`` and ``start`` only."""

    @property
    def running(self) -> bool:
        """Whether its VM is up."""
        return self.state == SandboxState.RUNNING

    @classmethod
    def from_dict(cls, client: Client, value: Mapping[str, Any]) -> Sandbox:
        return cls(
            client,
            value["id"],
            value.get("name"),
            value["image"],
            SandboxState(value["state"]),
            value["ephemeral"],
            value["auto_stop_minutes"],
            value.get("auto_delete_days"),
            value["used_at"],
            [SandboxPort.from_dict(port) for port in value.get("ports", ())],
        )

    @property
    def _root(self) -> dict[str, str]:
        return {"sandbox": self.id}

    async def start(self) -> Sandbox:
        """Start it if it is stopped -- its VM booted over its disk as it was left -- and answer
        it once it runs, with where its ports are now. A job started in it while it is stopped
        starts it too."""
        return await self.client.start_sandbox(self.id)

    async def stop(self) -> Sandbox:
        """Stop it if it runs: its jobs end, and its disk is kept -- or, if it is ephemeral,
        deleted with it. Answered once it has stopped."""
        return await self.client.stop_sandbox(self.id)

    async def open_port(self, port: int) -> SandboxPort:
        """Open the TCP port ``port`` in it, as those it was made with are, and answer where it
        is reached from, kept in ``ports`` in place of one for the same port. A program in it
        listening on ``port`` already is reached once it listens again."""
        opened = await self.client.open_sandbox_port(self.id, port)
        at = next((i for i, known in enumerate(self.ports) if known.port == port), None)
        if at is None:
            self.ports.append(opened)
        else:
            self.ports[at] = opened
        return opened

    def commit(self, reference: str) -> StartedJob[ImageJob, ImageJobEntry]:
        """Make an image of it, as ``Client.commit_image`` does, to point ``reference`` at."""
        return self.client.commit_image(self.id, reference)

    def exec(
        self, cmd: Iterable[str], *, stdin: bool = False, timeout_ms: int | None = None
    ) -> StartedJob[SandboxJob, SandboxJobEntry]:
        """Run ``cmd`` in it, its stdin open for ``SandboxJob.write_stdin`` if ``stdin``, killed
        after ``timeout_ms`` if one is given. Awaited, the job; iterated, its output and how it
        ended, as ``SandboxJob.log``'s. Sent when first awaited or iterated."""
        request = args(sandbox=self.id, cmd=list(cmd), stdin=stdin or None, timeout_ms=timeout_ms)

        async def start() -> SandboxJob:
            output = await self.client._call("virtx_client_exec_sandbox", request)
            return SandboxJob.from_dict(self.client, self.id, output)

        async def run() -> AsyncIterator[SandboxJob | LogEntry[SandboxJobEntry]]:
            async for item in self.client._items("virtx_client_exec_sandbox_log", request):
                if "job" in item:
                    yield SandboxJob.from_dict(self.client, self.id, item["job"])
                else:
                    yield sandbox_job_entry(item)

        return StartedJob(start, lambda job: job.log(), run)

    async def jobs(self) -> list[SandboxJob]:
        """Every job in it."""
        items = await self.client._call("virtx_client_list_sandbox_job", args(sandbox=self.id))
        return [SandboxJob.from_dict(self.client, self.id, item) for item in items]

    async def job(self, id: str) -> SandboxJob:
        """The job ``id`` in it."""
        output = await self.client._call(
            "virtx_client_get_sandbox_job", args(sandbox=self.id, job=id)
        )
        return SandboxJob.from_dict(self.client, self.id, output)

    async def remove(self) -> None:
        """Delete it: stopped first if it runs, and its disk removed. Its volumes stay."""
        await self.client._call("virtx_client_remove_sandbox", args(id=self.id))

    async def __aenter__(self) -> Self:
        return self

    async def __aexit__(self, *exc: object) -> None:
        """Delete it, as ``remove`` does; one deleted already is left so."""
        try:
            await self.remove()
        except VirtxError as error:
            if error.status != 404:
                raise


class SandboxJobKind(str, Enum):
    """What a sandbox job does."""

    EXEC = "exec"


@dataclass(frozen=True)
class SandboxJob:
    """A process run in a sandbox, as ``Sandbox.exec``, ``Sandbox.jobs`` and ``Sandbox.job``
    answer it, and the client and sandbox that reach it: what signals it, writes to its stdin
    and reads its log. Dropping it leaves the job; ``remove`` removes it."""

    client: Client = field(repr=False, compare=False)
    """The client its requests go through."""

    sandbox_id: str
    """The id of the sandbox it runs in."""

    id: str
    """The job's id, which every request about it names it by, beside its sandbox's."""

    kind: SandboxJobKind

    pid: int
    """Its process's id in the sandbox."""

    cmd: list[str]

    running: bool
    """Whether its process runs still."""

    code: int | None = None
    """Once it has ended, its exit code."""

    @classmethod
    def from_dict(cls, client: Client, sandbox_id: str, value: Mapping[str, Any]) -> SandboxJob:
        return cls(
            client,
            sandbox_id,
            value["id"],
            SandboxJobKind(value["kind"]),
            value["pid"],
            list(value["cmd"]),
            value["running"],
            value.get("code"),
        )

    def _args(self, **fields: Any) -> bytes:
        return args(sandbox=self.sandbox_id, job=self.id, **fields)

    async def get(self) -> SandboxJob:
        """It as the server has it now."""
        output = await self.client._call("virtx_client_get_sandbox_job", self._args())
        return SandboxJob.from_dict(self.client, self.sandbox_id, output)

    async def signal(self, signal: str) -> None:
        """Send ``signal``, such as ``"SIGTERM"``, to its process and every process it
        started."""
        await self.client._call("virtx_client_signal_sandbox_job", self._args(signal=signal))

    async def write_stdin(self, data: str | None = None, eof: bool = False) -> None:
        """Write ``data`` to its process's stdin, then close it if ``eof``. Returns once it is
        written: await it before the next write, so the writes land in order."""
        await self.client._call(
            "virtx_client_write_sandbox_job_stdin", self._args(data=data, eof=eof or None)
        )

    async def log(self, after: int | None = None) -> AsyncIterator[LogEntry[SandboxJobEntry]]:
        """Its log, its process's output, from its start or after its ``after``th entry, until
        its ``SandboxJobEnd``. Closing it leaves the job as it is."""
        async for item in self.client._items(
            "virtx_client_sandbox_job_log", self._args(after=after)
        ):
            yield sandbox_job_entry(item)

    async def remove(self) -> None:
        """Cancel it if it runs, killing its process, and forget it."""
        await self.client._call("virtx_client_remove_sandbox_job", self._args())


@dataclass(frozen=True)
class Stdout:
    """A line the process wrote to stdout."""

    line: str


@dataclass(frozen=True)
class Stderr:
    """A line the process wrote to stderr."""

    line: str


@dataclass(frozen=True)
class SandboxJobEnd:
    """How the process ended, last in its log."""

    code: int


SandboxJobEntry = Stdout | Stderr | Truncated | SandboxJobEnd
"""An entry of a sandbox job's log."""


def sandbox_job_entry(item: Mapping[str, Any]) -> LogEntry[SandboxJobEntry]:
    """An item of ``SandboxJob.log``'s stream, as ``{"id": n, "stdout": "..."}`` and the
    like."""
    entry: SandboxJobEntry
    if "stdout" in item:
        entry = Stdout(item["stdout"])
    elif "stderr" in item:
        entry = Stderr(item["stderr"])
    elif "truncated" in item:
        entry = Truncated()
    else:
        entry = SandboxJobEnd(item["end"]["code"])
    return LogEntry(item["id"], entry)
