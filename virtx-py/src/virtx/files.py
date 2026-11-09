"""The files in a volume, a snapshot or a sandbox, reached with WebDAV at
``/volumes/{name}/files/``, ``/snapshots/{name}/files/`` or ``/sandboxes/{id}/files/``: the
requests on them, and what they answer.

A path is relative to the root, ``/``-separated, as ``dir/file.txt``; the empty path is the
root. The server refuses one that leads outside.
"""

from __future__ import annotations

from collections.abc import Mapping
from dataclasses import dataclass
from typing import TYPE_CHECKING, Any

from ._library import args

if TYPE_CHECKING:
    from .client import Client


class Files:
    """The files of a ``Volume``, a ``Snapshot`` or a ``Sandbox``. A path is relative to its
    root, ``/``-separated, as ``dir/file.txt``; the empty path is the root."""

    if TYPE_CHECKING:
        client: Client

    @property
    def _root(self) -> dict[str, str]:
        """Whose files these are, as the wire names them."""
        raise NotImplementedError

    async def _files_call(self, name: str, **fields: Any) -> Any:
        return await self.client._call(name, args(**self._root, **fields))

    async def stat_file(self, path: str) -> Metadata:
        """What is at ``path``."""
        return Metadata.from_dict(await self._files_call("virtx_client_stat_file", path=path))

    async def exists(self, path: str) -> bool:
        """Whether anything is at ``path``."""
        return await self._files_call("virtx_client_file_exists", path=path)

    async def is_file(self, path: str) -> bool:
        """Whether a file is at ``path``."""
        return await self._files_call("virtx_client_is_file", path=path)

    async def is_dir(self, path: str) -> bool:
        """Whether a directory is at ``path``."""
        return await self._files_call("virtx_client_is_dir", path=path)

    async def read_dir(self, path: str) -> list[DirEntry]:
        """The entries of the directory at ``path``."""
        items = await self._files_call("virtx_client_read_dir", path=path)
        return [DirEntry.from_dict(item) for item in items]

    async def read_file(self, path: str, offset: int = 0, length: int | None = None) -> bytes:
        """The file at ``path``, from ``offset``, ``length`` bytes of it or all that is left."""
        return await self.client._call(
            "virtx_client_read_file",
            args(**self._root, path=path, offset=offset, length=length),
            raw=True,
        )

    async def read_text(self, path: str) -> str:
        """The whole file at ``path`` as text, which it has to be: UTF-8."""
        return await self._files_call("virtx_client_read_text", path=path)

    async def write_file(
        self, path: str, content: bytes | str, if_match: str | None = None
    ) -> None:
        """Write ``content``, a ``str`` as UTF-8, as the whole file at ``path``, creating it if
        it does not exist. Its parent directory must exist. With ``if_match``, the ``etag`` a
        :class:`Metadata` of the file gave, the write goes only while the file still has it,
        and is refused with a ``412`` otherwise."""
        client = self.client
        request = args(**self._root, path=path, if_match=if_match)
        content = content.encode() if isinstance(content, str) else bytes(content)
        handle = client._lib.virtx_client_write_file(
            client._held(), request, len(request), content, len(content)
        )
        await client._lib.answer(handle)

    async def create_dir(self, path: str) -> None:
        """Create the directory at ``path``, whose parent must exist."""
        await self._files_call("virtx_client_create_dir", path=path)

    async def create_dir_all(self, path: str) -> None:
        """Have the directory at ``path``, as ``mkdir -p``: its parents are created as needed,
        and one there already is left as it is. Refused with a ``405`` when a file is at
        ``path``."""
        await self._files_call("virtx_client_create_dir_all", path=path)

    async def remove_file(self, path: str, missing_ok: bool = False) -> None:
        """Remove what is at ``path``: a directory with everything in it. With ``missing_ok``,
        nothing being there is fine, as ``rm -f``; without it, a ``404``."""
        await self._files_call("virtx_client_remove_file", path=path, missing_ok=missing_ok or None)

    async def rename_file(self, from_path: str, to_path: str, overwrite: bool = False) -> None:
        """Move what is at ``from_path`` to ``to_path``, replacing what is at ``to_path`` if
        ``overwrite``, and refused if not and something is there."""
        await self._files_call(
            "virtx_client_rename_file", **{"from": from_path}, to=to_path, overwrite=overwrite
        )

    async def copy_file(self, from_path: str, to_path: str, overwrite: bool = False) -> None:
        """Copy what is at ``from_path`` to ``to_path``, a directory with everything in it, as
        ``rename_file`` replaces what is at ``to_path``."""
        await self._files_call(
            "virtx_client_copy_file", **{"from": from_path}, to=to_path, overwrite=overwrite
        )


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
