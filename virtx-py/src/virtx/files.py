"""The files in a volume, a snapshot or a sandbox, reached with WebDAV at
``/volumes/{name}/files/``, ``/snapshots/{name}/files/`` or ``/sandboxes/{id}/files/``: what
each request names, and what it answers.

A path is relative to the root, ``/``-separated, as ``dir/file.txt``; the empty path is the
root. The server refuses one that leads outside.
"""

from __future__ import annotations

from collections.abc import Mapping
from dataclasses import dataclass
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from .client import Client


@dataclass(frozen=True)
class VolumeFiles:
    """The files of the volume ``name``."""

    name: str

    def to_dict(self) -> dict[str, Any]:
        """As the wire has it."""
        return {"volume": self.name}


@dataclass(frozen=True)
class SnapshotFiles:
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


FileRoot = VolumeFiles | SnapshotFiles | SandboxFiles
"""Whose files a request names."""


class Files:
    """The files of a ``Volume``, a ``Snapshot`` or a ``Sandbox``: each request the client's of
    the same name, at its ``root``. A path is relative to the root, as ``dir/file.txt``."""

    if TYPE_CHECKING:
        client: Client

        @property
        def root(self) -> FileRoot: ...

    async def stat_file(self, path: str) -> Metadata:
        """What is at ``path``."""
        return await self.client.stat_file(self.root, path)

    async def exists(self, path: str) -> bool:
        """Whether anything is at ``path``."""
        return await self.client.exists(self.root, path)

    async def is_file(self, path: str) -> bool:
        """Whether a file is at ``path``."""
        return await self.client.is_file(self.root, path)

    async def is_dir(self, path: str) -> bool:
        """Whether a directory is at ``path``."""
        return await self.client.is_dir(self.root, path)

    async def read_dir(self, path: str) -> list[DirEntry]:
        """The entries of the directory at ``path``."""
        return await self.client.read_dir(self.root, path)

    async def read_file(self, path: str, offset: int = 0, length: int | None = None) -> bytes:
        """The file at ``path``, from ``offset``, ``length`` bytes of it or all that is left."""
        return await self.client.read_file(self.root, path, offset, length)

    async def read_text(self, path: str) -> str:
        """The whole file at ``path`` as text, which it has to be: UTF-8."""
        return await self.client.read_text(self.root, path)

    async def write_file(
        self, path: str, content: bytes | str, if_match: str | None = None
    ) -> None:
        """Write ``content`` as the whole file at ``path``, as ``Client.write_file`` does."""
        await self.client.write_file(self.root, path, content, if_match)

    async def create_dir(self, path: str) -> None:
        """Create the directory at ``path``, whose parent must exist."""
        await self.client.create_dir(self.root, path)

    async def create_dir_all(self, path: str) -> None:
        """Have the directory at ``path``, as ``mkdir -p``."""
        await self.client.create_dir_all(self.root, path)

    async def remove_file(self, path: str, missing_ok: bool = False) -> None:
        """Remove what is at ``path``, as ``Client.remove_file`` does."""
        await self.client.remove_file(self.root, path, missing_ok)

    async def rename_file(self, from_path: str, to_path: str, overwrite: bool = False) -> None:
        """Move what is at ``from_path`` to ``to_path``, as ``Client.rename_file`` does."""
        await self.client.rename_file(self.root, from_path, to_path, overwrite)

    async def copy_file(self, from_path: str, to_path: str, overwrite: bool = False) -> None:
        """Copy what is at ``from_path`` to ``to_path``, as ``Client.copy_file`` does."""
        await self.client.copy_file(self.root, from_path, to_path, overwrite)


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
