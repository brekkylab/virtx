"""Sandboxes: a VM booted from an image, with volumes mounted into it and a disk of its own that
outlives the VM -- it stops when unused or told to, and starts again as it was left, under the
same id, until it is deleted; and the processes run in one, each a job whose log is its output."""

from __future__ import annotations

from collections.abc import Mapping
from dataclasses import dataclass, field
from enum import Enum
from typing import Any

from .job import LogEntry, Truncated


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


class SandboxState(str, Enum):
    """Whether a sandbox's VM is up."""

    RUNNING = "running"
    STOPPED = "stopped"
    """Its VM is down and its disk kept: a job started in it, or ``start_sandbox``, starts it."""


@dataclass(frozen=True)
class Sandbox:
    """One sandbox the server holds, running or stopped."""

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
    """Its opened ports: answered by ``create_sandbox`` and ``start_sandbox`` only."""

    @property
    def running(self) -> bool:
        """Whether its VM is up."""
        return self.state == SandboxState.RUNNING

    @classmethod
    def from_dict(cls, value: Mapping[str, Any]) -> Sandbox:
        return cls(
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
