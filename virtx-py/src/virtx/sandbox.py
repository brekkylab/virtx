"""Sandboxes: a VM booted from an image, with volumes mounted into it and a disk of its own that
outlives the VM -- it stops when unused or told to, and starts again as it was left, under the
same id, until it is deleted; and the processes run in one, each a job whose log is its output."""

from __future__ import annotations

import builtins
import sys
from collections.abc import AsyncIterator, Iterable, Mapping
from contextlib import aclosing
from dataclasses import dataclass, field
from enum import Enum
from typing import TYPE_CHECKING, Any, Self

from ._library import VirtxError, args
from .files import Files
from .job import LogEntry, StartedJob, Truncated
from .volume import Volume

if TYPE_CHECKING:
    from .client import Client


class PortProtocol(str, Enum):
    """How a sandbox's port is reached from outside it."""

    TCP = "tcp"
    """TCP: at a loopback address, or through a ``PortForwarder`` opened with its ``url``."""

    HTTP = "http"
    """HTTP served in the sandbox, reached at its ``url`` as HTTP by a browser as by any client,
    WebSockets included. A server that cannot reach a port so refuses it."""


@dataclass(frozen=True)
class InboundError:
    """Why a port's inbound rules were not set, in the shape of a refusal."""

    code: str
    """As a refusal's, such as ``quota`` or ``unavailable``."""

    message: str
    """For a person to read."""

    reason: str | None = None
    """What a ``quota`` is past, as a refusal's ``reason``."""

    @classmethod
    def from_dict(cls, value: Mapping[str, Any]) -> InboundError:
        return cls(value["code"], value["message"], value.get("reason"))


@dataclass(frozen=True)
class SandboxPort:
    """A port opened in a sandbox as it was made, and the ``url`` it is reached by: a
    ``PortForwarder``'s for TCP, a browser's for HTTP."""

    port: int
    url: str

    label: str | None = None
    """What it is for, as it was opened with."""

    protocol: PortProtocol = PortProtocol.TCP

    address: str | None = None
    """A ``host:port`` any TCP client reaches a TCP port at directly, from a server that gives it
    one. Not secret, as ``url`` is: what listens there answers whoever connects."""

    inbound_error: InboundError | None = None
    """Why its inbound rules were not set, from a server that made the port all the same: it lets
    no one in, and is reached at no ``address``, until they are."""

    @classmethod
    def from_dict(cls, value: Mapping[str, Any]) -> SandboxPort:
        why = value.get("inbound_error")
        return cls(
            value["port"],
            value["url"],
            value.get("label"),
            PortProtocol(value.get("protocol", "tcp")),
            value.get("address"),
            InboundError.from_dict(why) if why is not None else None,
        )


@dataclass(frozen=True)
class InboundRule:
    """One of a port's inbound rules: the addresses it lets in, and whose they are."""

    source: str
    """An IPv4 or IPv6 CIDR, as ``203.0.113.0/24``, or one address, as ``203.0.113.7``."""

    description: str | None = None
    """What it is for: whose they are."""

    def to_dict(self) -> dict[str, Any]:
        """As the wire has it."""
        rule: dict[str, Any] = {"source": self.source}
        if self.description is not None:
            rule["description"] = self.description
        return rule

    @classmethod
    def from_dict(cls, value: Mapping[str, Any]) -> InboundRule:
        return cls(value["source"], value.get("description"))


@dataclass(frozen=True)
class PortSpec:
    """A port to open in a sandbox as ``create_sandbox`` makes it: its number, what it is for,
    how it is reached, and whom it lets in. ``ports`` takes a plain ``int`` for a TCP port that
    says nothing more."""

    port: int

    label: str | None = None
    """What it is for, up to 128 characters, answered with it."""

    protocol: PortProtocol | str = PortProtocol.TCP

    inbound: Iterable[InboundRule | Mapping[str, Any]] | None = None
    """Its inbound rules, each an ``InboundRule`` or its ``{source, description?}``. ``None``
    leaves whom it lets in to the server, and ``[]`` lets in no one. A server that keeps no rules
    refuses them."""

    def to_dict(self) -> int | dict[str, Any]:
        """As the wire has it: the number alone when it says no more."""
        protocol = PortProtocol(self.protocol)
        if self.label is None and protocol == PortProtocol.TCP and self.inbound is None:
            return self.port
        spec: dict[str, Any] = {"port": self.port}
        if self.label is not None:
            spec["label"] = self.label
        if protocol != PortProtocol.TCP:
            spec["protocol"] = protocol.value
        if self.inbound is not None:
            spec["inbound"] = [
                r.to_dict() if isinstance(r, InboundRule) else dict(r) for r in self.inbound
            ]
        return spec


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


@dataclass(frozen=True)
class Sandbox(Files):
    """One sandbox the server holds, and the client that reaches it: what runs processes in it
    and reads and writes its files.
    Dropping it leaves the sandbox; ``remove`` deletes it, as leaving ``async with`` does:
    ``async with await client.create_sandbox(...) as sandbox:`` makes one for a block."""

    client: Client = field(repr=False, compare=False)
    """The client its requests go through."""

    id: str

    name: str | None
    """The name it was made with, which ``find_sandbox`` finds it by."""

    image: str
    """The digest of the image it stands on, as ``algorithm:hex``: what its VM boots every
    time, whatever its tag points at by then."""

    running: bool
    """Whether it takes processes: ``False`` once its VM has exited on its own. One whose VM the
    server stopped for want of use is booted again by the next job, and is ``True``."""

    ports: list[SandboxPort] = field(default_factory=list)
    """Its opened ports: answered by ``create_sandbox`` only."""

    abins: list[str] = field(default_factory=list)
    """virtx's own programs it has at ``/abin``."""

    @classmethod
    def from_dict(cls, client: Client, value: Mapping[str, Any]) -> Sandbox:
        return cls(
            client,
            value["id"],
            value.get("name"),
            value["image"],
            value["running"],
            [SandboxPort.from_dict(port) for port in value.get("ports", ())],
            list(value.get("abins", ())),
        )

    @property
    def _root(self) -> dict[str, str]:
        return {"sandbox": self.id}

    def exec(
        self, cmd: Iterable[str], *, stdin: bool = False, timeout_ms: int | None = None
    ) -> StartedJob[SandboxJob, SandboxJobEntry]:
        """Run ``cmd`` in it, its stdin open for ``SandboxJob.write_stdin`` if ``stdin``, killed
        after ``timeout_ms`` if one is given. Awaited, the job; iterated, its output and how it
        ended, as ``SandboxJob.log``'s; ``complete``, its exit code, as
        ``SandboxJob.complete``'s. Sent when first awaited, iterated or completed."""
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
        """Delete it: its VM stopped if it is up, and its disk removed. Its volumes stay."""
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


class SandboxJobEndReason(str, Enum):
    """Why a process did not end by itself: the server killed it, as ``SIGKILL`` does, its code
    ``137``."""

    TIMEOUT = "timeout"
    """Its ``timeout_ms`` ran out."""

    CANCELLED = "cancelled"
    """Its job was removed while it ran, or its sandbox parked or removed under it."""

    VM_EXITED = "vm_exited"
    """Its sandbox's VM exited on its own under it."""


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

    reason: SandboxJobEndReason | None = None
    """Once it has ended, why, if it did not end by itself."""

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
            _reason(value),
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

    async def complete(self, print: bool = False) -> int:
        """Follow it to its end, printing what its process writes to stdout and stderr as it
        wrote them if ``print``: its exit code, ``128 + signal`` for one a signal ended."""
        async with aclosing(self.log()) as log:
            async for item in log:
                match item.entry:
                    case Stdout(line) if print:
                        builtins.print(line)
                    case Stderr(line) if print:
                        builtins.print(line, file=sys.stderr)
                    case Truncated() if print:
                        builtins.print(
                            "(the log reached the server's limit; the rest is not kept)",
                            file=sys.stderr,
                        )
                    case SandboxJobEnd(code):
                        return code
        raise VirtxError("the job was removed before it ended", source="job")

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

    reason: SandboxJobEndReason | None = None
    """Why, if it did not end by itself."""


def _reason(value: Mapping[str, Any]) -> SandboxJobEndReason | None:
    """A job's, or its end's, ``reason``, if it has one."""
    reason = value.get("reason")
    return None if reason is None else SandboxJobEndReason(reason)


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
        entry = SandboxJobEnd(item["end"]["code"], _reason(item["end"]))
    return LogEntry(item["id"], entry)
