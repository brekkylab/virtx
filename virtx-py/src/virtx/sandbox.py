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


class PortProtocol(str, Enum):
    """How a sandbox's port is reached from outside it."""

    TCP = "tcp"
    """TCP, through a ``PortForwarder`` opened with its ``url``."""

    HTTP = "http"
    """HTTP served in the sandbox, reached at its ``url`` as HTTP by a browser as by any client,
    WebSockets included."""


@dataclass(frozen=True)
class SandboxPort:
    """A port opened in a sandbox, and the ``url`` it is reached by: a ``PortForwarder``'s for
    TCP, a browser's for HTTP."""

    port: int
    url: str

    label: str | None = None
    """What it is for, in words of whoever opened it."""

    protocol: PortProtocol = PortProtocol.TCP

    address: str | None = None
    """A ``host:port`` any TCP client reaches a TCP port at directly, from a server that gives it
    one. Not secret, as ``url`` is: what listens there answers whoever connects."""

    @classmethod
    def from_dict(cls, value: Mapping[str, Any]) -> SandboxPort:
        return cls(
            value["port"],
            value["url"],
            value.get("label"),
            PortProtocol(value.get("protocol", "tcp")),
            value.get("address"),
        )


@dataclass(frozen=True)
class InboundRule:
    """One of a port's inbound rules, virtx cloud's: the addresses it lets in, and whose they
    are."""

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


def inbound_rules(rules: Iterable[InboundRule | Mapping[str, Any]]) -> list[dict[str, Any]]:
    """``rules``, each an ``InboundRule`` or its ``{source, description?}``, as the wire has
    them."""
    return [r.to_dict() if isinstance(r, InboundRule) else dict(r) for r in rules]


@dataclass(frozen=True)
class PortInbound:
    """A port's inbound rules, as virtx cloud answers them: whom a TCP port lets in at its public
    ``address``, and an HTTP one at its ``url``. A connection from an address no rule's source
    holds is not let in there, so a port with none is reached there by no one."""

    port: int

    rules: list[InboundRule]
    """Each source a CIDR, as the server keeps it."""

    address: str | None = None
    """Its public ``host:port``, once a rule has given it one, as ``SandboxPort.address``."""

    @classmethod
    def from_dict(cls, value: Mapping[str, Any]) -> PortInbound:
        return cls(
            value["port"],
            [InboundRule.from_dict(rule) for rule in value["rules"]],
            value.get("address"),
        )


@dataclass(frozen=True)
class PortSpec:
    """A port to open in a sandbox as ``create_sandbox`` makes it: its number, what it is for,
    how it is reached, and whom it lets in. ``ports`` takes a plain ``int`` for TCP that says
    nothing more."""

    port: int
    label: str | None = None
    protocol: PortProtocol | str = PortProtocol.TCP

    inbound: Iterable[InboundRule | Mapping[str, Any]] | None = None
    """Its inbound rules, set once it is open (``PortInbound``): virtx cloud's. ``None`` leaves
    them to the server, and ``[]`` lets in no one."""

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
            spec["inbound"] = inbound_rules(self.inbound)
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
    """Its opened ports: answered by ``create_sandbox`` and ``start`` only, each ``open_port``
    adding one and ``close_port`` dropping it."""

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

    async def open_port(
        self,
        port: int,
        *,
        label: str | None = None,
        protocol: PortProtocol | str | None = None,
        inbound: Iterable[InboundRule | Mapping[str, Any]] | None = None,
    ) -> SandboxPort:
        """Open the port ``port`` in it, as those it was made with are -- TCP unless ``protocol``
        says HTTP, ``label`` saying what it is for, ``inbound`` whom it lets in -- and answer
        where it is reached from, kept in ``ports`` in place of one for the same port. A program
        in it listening on ``port`` already is reached once it listens again."""
        opened = await self.client.open_sandbox_port(
            self.id, port, label=label, protocol=protocol, inbound=inbound
        )
        at = next((i for i, known in enumerate(self.ports) if known.port == port), None)
        if at is None:
            self.ports.append(opened)
        else:
            self.ports[at] = opened
        return opened

    async def close_port(self, port: int) -> None:
        """Close the port ``port`` in it, as ``Client.close_sandbox_port`` does, and drop it from
        ``ports``."""
        await self.client.close_sandbox_port(self.id, port)
        self.ports[:] = [known for known in self.ports if known.port != port]

    async def inbound(self, port: int) -> PortInbound:
        """The inbound rules of its port ``port``, as ``Client.get_sandbox_port_inbound``
        answers them: virtx cloud's."""
        return await self.client.get_sandbox_port_inbound(self.id, port)

    async def set_inbound(
        self, port: int, rules: Iterable[InboundRule | Mapping[str, Any]]
    ) -> PortInbound:
        """Make the inbound rules of its port ``port`` ``rules``, all of them, as
        ``Client.set_sandbox_port_inbound`` does."""
        return await self.client.set_sandbox_port_inbound(self.id, port, rules)

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
