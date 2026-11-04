"""Sandboxes: a VM booted from an image, with volumes mounted into it and a disk of its own that
outlives the VM -- it stops when unused or told to, and starts again as it was left, under the
same id, until it is deleted; and the processes run in one, each a job whose log is its output."""

from __future__ import annotations

from collections.abc import Mapping
from dataclasses import dataclass, field
from enum import Enum
from typing import TYPE_CHECKING, Any

from ._library import VirtxError
from .job import LogEntry, Truncated

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
class PortSpec:
    """A port to open in a sandbox as ``create_sandbox`` makes it: its number, what it is for,
    and how it is reached. ``ports`` takes a plain ``int`` for TCP named nothing."""

    port: int
    label: str | None = None
    protocol: PortProtocol | str = PortProtocol.TCP

    def to_dict(self) -> int | dict[str, Any]:
        """As the wire has it: the number alone when it says no more."""
        protocol = PortProtocol(self.protocol)
        if self.label is None and protocol == PortProtocol.TCP:
            return self.port
        spec: dict[str, Any] = {"port": self.port}
        if self.label is not None:
            spec["label"] = self.label
        if protocol != PortProtocol.TCP:
            spec["protocol"] = protocol.value
        return spec


@dataclass(frozen=True)
class SandboxVolume:
    """A volume mounted at ``path`` in a sandbox."""

    volume: str
    path: str
    readonly: bool = False

    def to_dict(self) -> dict[str, Any]:
        """As the wire has it."""
        return {"volume": self.volume, "path": self.path, "readonly": self.readonly}


class SandboxState(str, Enum):
    """Whether a sandbox's VM is up."""

    RUNNING = "running"
    STOPPED = "stopped"
    """Its VM is down and its disk kept: a job started in it, or ``start_sandbox``, starts it."""


@dataclass(frozen=True)
class Sandbox:
    """One sandbox the server holds, running or stopped.

    One a client answered is that client's: ``open_port`` is sent over it, and ``async with``
    deletes the sandbox on leaving, as ``async with await client.create_sandbox(...) as
    sandbox:`` makes one for a block."""

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
    """Its opened ports: answered by ``create_sandbox`` and ``start_sandbox`` only, each
    ``open_port`` adding one and ``close_port`` dropping it."""

    _client: Client | None = field(default=None, init=False, repr=False, compare=False)

    @property
    def running(self) -> bool:
        """Whether its VM is up."""
        return self.state == SandboxState.RUNNING

    def _held(self) -> Client:
        if self._client is None:
            raise VirtxError("the sandbox was not answered by a client")
        return self._client

    async def open_port(
        self,
        port: int,
        *,
        label: str | None = None,
        protocol: PortProtocol | str | None = None,
    ) -> SandboxPort:
        """Open the port ``port`` in it, as ``Client.open_sandbox_port`` does, and keep the
        answer in ``ports``, in place of one for the same port."""
        opened = await self._held().open_sandbox_port(self.id, port, label=label, protocol=protocol)
        at = next((i for i, known in enumerate(self.ports) if known.port == port), None)
        if at is None:
            self.ports.append(opened)
        else:
            self.ports[at] = opened
        return opened

    async def close_port(self, port: int) -> None:
        """Close the port ``port`` in it, as ``Client.close_sandbox_port`` does, and drop it from
        ``ports``."""
        await self._held().close_sandbox_port(self.id, port)
        self.ports[:] = [known for known in self.ports if known.port != port]

    async def __aenter__(self) -> Self:
        return self

    async def __aexit__(self, *exc: object) -> None:
        """Delete it, as ``Client.remove_sandbox`` does; one deleted already is left so."""
        try:
            await self._held().remove_sandbox(self.id)
        except VirtxError as error:
            if not str(error).startswith(_NOT_FOUND):
                raise

    @classmethod
    def from_dict(cls, value: Mapping[str, Any], client: Client | None = None) -> Sandbox:
        """``value`` as the wire has it, sent over ``client`` if given."""
        sandbox = cls(
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
        object.__setattr__(sandbox, "_client", client)
        return sandbox


_NOT_FOUND = "the server answered 404"
"""How a refusal with ``404`` reads, as ``libvirtx_core`` words it."""


class SandboxJobKind(str, Enum):
    """What a sandbox job does."""

    EXEC = "exec"


@dataclass(frozen=True)
class SandboxJob:
    """A process run in a sandbox: what ``exec_sandbox`` answers. Its output is
    ``sandbox_job_log``'s."""

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
    def from_dict(cls, value: Mapping[str, Any]) -> SandboxJob:
        return cls(
            value["id"],
            SandboxJobKind(value["kind"]),
            value["pid"],
            list(value["cmd"]),
            value["running"],
            value.get("code"),
        )


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
