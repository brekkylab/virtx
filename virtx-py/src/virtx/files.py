"""The files in a volume, a snapshot or a sandbox, reached with WebDAV at
``/volumes/{name}/files/``, ``/snapshots/{name}/files/`` or ``/sandboxes/{id}/files/``: what
each request names, and what it answers.

A path is relative to the root, ``/``-separated, as ``dir/file.txt``; the empty path is the
root. The server refuses one that leads outside.
"""

from __future__ import annotations

from collections.abc import Mapping
from dataclasses import dataclass
from typing import Any


@dataclass(frozen=True)
class Volume:
    """The files of the volume ``name``."""

    name: str

    def to_dict(self) -> dict[str, Any]:
        """As the wire has it."""
        return {"volume": self.name}


@dataclass(frozen=True)
class Snapshot:
    """The files of the snapshot ``name``. One in use, held by a sandbox, refuses every
    request."""

    name: str

    def to_dict(self) -> dict[str, Any]:
        """As the wire has it."""
        return {"snapshot": self.name}


@dataclass(frozen=True)
class SandboxFiles:
    """The files of the sandbox ``id``, as its processes see them, while its VM runs."""

    id: str

    def to_dict(self) -> dict[str, Any]:
        """As the wire has it."""
        return {"sandbox": self.id}


FileRoot = Volume | Snapshot | SandboxFiles
"""Whose files a request names."""


@dataclass(frozen=True)
class Metadata:
    """What a file or directory is."""

    is_dir: bool
    """Whether it is a directory."""

    size: int
    """Its size in bytes; ``0`` for a directory."""

    modified: int | None = None
    """When it was last modified, in seconds since the Unix epoch, if the server says."""

    etag: str | None = None
    """Its entity tag as an ``ETag`` header spells it, if the server gives one: what a write
    names as ``if_match`` to go only over the file it read."""

    @classmethod
    def from_dict(cls, value: Mapping[str, Any]) -> Metadata:
        return cls(value["is_dir"], value["size"], value.get("modified"), value.get("etag"))


@dataclass(frozen=True)
class DirEntry:
    """One entry of a directory."""

    name: str
    """Its name in the directory: one path segment."""

    metadata: Metadata

    @classmethod
    def from_dict(cls, value: Mapping[str, Any]) -> DirEntry:
        return cls(value["name"], Metadata.from_dict(value["metadata"]))
