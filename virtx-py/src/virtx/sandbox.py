"""Sandboxes: a VM booted from an image, with volumes mounted into it; and the processes run in
one, each a job whose log is its output."""

from __future__ import annotations

import builtins
import sys
from collections.abc import AsyncIterator, Iterable, Mapping
from contextlib import aclosing
from dataclasses import dataclass, field
from enum import Enum
from typing import TYPE_CHECKING, Any, TypeVar

from ._library import VirtxError, args
from .files import Files
from .job import LogEntry, StartedJob, Truncated
from .volume import Volume

if TYPE_CHECKING:
    from .client import Client


E = TypeVar("E", bound=Enum)


class PortProtocol(str, Enum):
    """How a sandbox's port is reached from outside it. A protocol this version does not know is
    read as the server's string itself."""

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

    url: str | None = None
    """Answered by ``create_sandbox`` alone, as secret as the credentials it carries: ``None``
    in a sandbox ``get_sandbox`` or ``list_sandbox`` answers."""

    label: str | None = None
    """What it is for, as it was opened with."""

    protocol: PortProtocol | str = PortProtocol.TCP
    """A ``str`` for a protocol this version does not know."""

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
            value.get("url"),
            value.get("label"),
            _known(PortProtocol, value.get("protocol", "tcp")),
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
class SandboxResources:
    """A sandbox's size as ``Sandbox.resize`` answers it: what its VM boots with from its next
    boot on, and whether its VM is ``up`` now, the size it booted with until it boots again."""

    vcpus: int
    memory_mib: int
    disk_gib: int
    up: bool

    @classmethod
    def from_dict(cls, value: Mapping[str, Any]) -> SandboxResources:
        return cls(value["vcpus"], value["memory_mib"], value["disk_gib"], value["up"])


@dataclass(frozen=True)
class Sandbox(Files):
    """One sandbox the server holds, and the client that reaches it: what runs processes in it
    and reads and writes its files. Dropping it leaves the sandbox; ``remove`` removes it."""

    client: Client = field(repr=False, compare=False)
    """The client its requests go through."""

    id: str

    name: str | None
    """The name it was made with, which ``find_sandbox`` finds it by."""

    image: str
    """The digest of the image it booted, as ``algorithm:hex``."""

    running: bool

    ports: list[SandboxPort] = field(default_factory=list)
    """Its opened ports, each with its ``url`` where ``create_sandbox`` answered it only."""

    abins: list[str] = field(default_factory=list)
    """virtx's own programs it has at ``/abin``."""

    error: str | None = None
    """Why it takes nothing but ``remove``, for one its server could not know again after a
    restart."""

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
            value.get("error"),
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
        ``SandboxJob.complete``'s. Sent when first awaited, iterated or completed, whatever else
        is awaited before: iterated first, the job is started and its log read as one request,
        where the server answers so; awaited first, its log is a second request."""
        request = args(sandbox=self.id, cmd=list(cmd), stdin=stdin or None, timeout_ms=timeout_ms)

        async def start() -> SandboxJob:
            output = await self.client._call("virtx_client_exec_sandbox", request)
            return SandboxJob.from_dict(self.client, self.id, output)

        async def run() -> AsyncIterator[SandboxJob | LogEntry[SandboxJobEntry]]:
            async for item in self.client._items("virtx_client_exec_sandbox_log", request):
                if "job" in item:
                    yield SandboxJob.from_dict(self.client, self.id, item["job"])
                elif (entry := sandbox_job_entry(item)) is not None:
                    yield entry

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

    async def resize(
        self,
        *,
        vcpus: int | None = None,
        memory_mib: int | None = None,
        disk_gib: int | None = None,
    ) -> SandboxResources:
        """Resize it: its ``vcpus`` and ``memory_mib`` from its VM's next boot on, and its disk
        grown, never shrunk, to ``disk_gib`` as its VM next boots. A VM up and unused is stopped
        for its vCPUs or memory to change, and one in use is refused ``409``; its disk alone
        stops nothing. What is left out stays as it is."""
        request = args(id=self.id, vcpus=vcpus, memory_mib=memory_mib, disk_gib=disk_gib)
        output = await self.client._call("virtx_client_resize_sandbox", request)
        return SandboxResources.from_dict(output)

    async def remove(self) -> None:
        """Stop it, end its jobs, and forget it, with everything it wrote outside its volumes."""
        await self.client._call("virtx_client_remove_sandbox", args(id=self.id))


class SandboxJobKind(str, Enum):
    """What a sandbox job does."""

    EXEC = "exec"


class SandboxJobEndReason(str, Enum):
    """Why a process did not end by itself: the server, or the kernel, killed it, as ``SIGKILL``
    does, its code ``137``. A reason this version does not know is read as the server's string
    itself."""

    TIMEOUT = "timeout"
    """Its ``timeout_ms`` ran out."""

    CANCELLED = "cancelled"
    """Its job was removed while it ran; its sandbox is still there."""

    SANDBOX_DELETED = "sandbox_deleted"
    """Its sandbox was removed under it, and everything it had with it."""

    VM_EXITED = "vm_exited"
    """Its sandbox's VM exited on its own under it."""

    OOM = "oom"
    """Its memory ran out: it, or a process it started, was killed for want of memory."""

    RESTARTED = "restarted"
    """Its server restarted while it ran, keeping its sandbox."""


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

    reason: SandboxJobEndReason | str | None = None
    """Once it has ended, why, if it did not end by itself: a ``str`` for a reason this version
    does not know."""

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
            if (entry := sandbox_job_entry(item)) is not None:
                yield entry

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

    reason: SandboxJobEndReason | str | None = None
    """Why, if it did not end by itself: a ``str`` for a reason this version does not know."""


def _known(kind: type[E], value: str) -> E | str:
    """``value`` as a member of ``kind``, or as the server said it where this version does not
    know it."""
    try:
        return kind(value)
    except ValueError:
        return value


def _reason(value: Mapping[str, Any]) -> SandboxJobEndReason | str | None:
    """A job's, or its end's, ``reason``, if it has one."""
    reason = value.get("reason")
    return None if reason is None else _known(SandboxJobEndReason, reason)


SandboxJobEntry = Stdout | Stderr | Truncated | SandboxJobEnd
"""An entry of a sandbox job's log."""


def sandbox_job_entry(item: Mapping[str, Any]) -> LogEntry[SandboxJobEntry] | None:
    """An item of ``SandboxJob.log``'s stream, as ``{"id": n, "stdout": "..."}`` and the
    like; ``None`` for an entry of a kind this version does not know, to be passed over."""
    entry: SandboxJobEntry
    if "stdout" in item:
        entry = Stdout(item["stdout"])
    elif "stderr" in item:
        entry = Stderr(item["stderr"])
    elif "truncated" in item:
        entry = Truncated()
    elif "end" in item:
        entry = SandboxJobEnd(item["end"]["code"], _reason(item["end"]))
    else:
        return None
    return LogEntry(item["id"], entry)
