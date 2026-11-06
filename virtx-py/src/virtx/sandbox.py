"""Sandboxes: a VM booted from an image, standing on a snapshot, with volumes mounted into it;
and the processes run in one, each a job whose log is its output."""

from __future__ import annotations

from collections.abc import AsyncIterator, Iterable, Mapping
from dataclasses import dataclass, field
from enum import Enum
from typing import TYPE_CHECKING, Any

from .files import Files, SandboxFiles
from .job import LogEntry, StartedJob, Truncated

if TYPE_CHECKING:
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
    """A volume mounted at ``path`` in a sandbox."""

    volume: str
    path: str
    readonly: bool = False

    def to_dict(self) -> dict[str, Any]:
        """As the wire has it."""
        return {"volume": self.volume, "path": self.path, "readonly": self.readonly}


@dataclass(frozen=True)
class Sandbox(Files):
    """One sandbox the server holds, and the client that reaches it: what runs processes in it
    and reads and writes its files. Dropping it leaves the sandbox; ``remove`` removes it."""

    client: Client = field(repr=False, compare=False)
    """The client its requests go through."""

    id: str

    image: str
    """The digest of the image it booted, as ``algorithm:hex``."""

    snapshot: str | None
    """The snapshot it stands on, if it has one."""

    running: bool

    ports: list[SandboxPort] = field(default_factory=list)
    """Its opened ports: answered by ``create_sandbox`` only."""

    @classmethod
    def from_dict(cls, client: Client, value: Mapping[str, Any]) -> Sandbox:
        return cls(
            client,
            value["id"],
            value["image"],
            value.get("snapshot"),
            value["running"],
            [SandboxPort.from_dict(port) for port in value.get("ports", ())],
        )

    @property
    def root(self) -> SandboxFiles:
        """Its files, its root as its processes see it, for the client's files requests."""
        return SandboxFiles(self.id)

    def exec(
        self, cmd: Iterable[str], *, stdin: bool = False, timeout_ms: int | None = None
    ) -> StartedJob[SandboxJob, SandboxJobEntry]:
        """Run ``cmd`` in it, as ``Client.exec_sandbox`` does."""
        return self.client.exec_sandbox(self.id, cmd, stdin=stdin, timeout_ms=timeout_ms)

    async def jobs(self) -> list[SandboxJob]:
        """Every job in it."""
        return await self.client.list_sandbox_job(self.id)

    async def job(self, id: str) -> SandboxJob:
        """The job ``id`` in it."""
        return await self.client.get_sandbox_job(self.id, id)

    async def remove(self) -> None:
        """Stop it, end its jobs, and forget it, keeping its snapshot and volumes."""
        await self.client.remove_sandbox(self.id)


class SandboxJobKind(str, Enum):
    """What a sandbox job does."""

    EXEC = "exec"


@dataclass(frozen=True)
class SandboxJob:
    """A process run in a sandbox, as ``exec_sandbox`` and ``get_sandbox_job`` answer it, and
    the client and sandbox that reach it: what signals it, writes to its stdin and reads its
    log. Dropping it leaves the job; ``remove`` removes it."""

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

    async def get(self) -> SandboxJob:
        """It as the server has it now."""
        return await self.client.get_sandbox_job(self.sandbox_id, self.id)

    async def signal(self, signal: str) -> None:
        """Send ``signal``, such as ``"SIGTERM"``, to its process and every process it
        started."""
        await self.client.signal_sandbox_job(self.sandbox_id, self.id, signal)

    async def write_stdin(self, data: str | None = None, eof: bool = False) -> None:
        """Write ``data`` to its process's stdin, then close it if ``eof``, as
        ``Client.write_sandbox_job_stdin`` does."""
        await self.client.write_sandbox_job_stdin(self.sandbox_id, self.id, data, eof)

    def log(self, after: int | None = None) -> AsyncIterator[LogEntry[SandboxJobEntry]]:
        """Its log, from its start or after its ``after``th entry, until its
        ``SandboxJobEnd``."""
        return self.client.sandbox_job_log(self.sandbox_id, self.id, after)

    async def remove(self) -> None:
        """Cancel it if it runs, killing its process, and forget it."""
        await self.client.remove_sandbox_job(self.sandbox_id, self.id)


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
    """An item of ``sandbox_job_log``'s stream, as ``{"id": n, "stdout": "..."}`` and the
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
