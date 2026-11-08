"""Volumes: storage the server keeps under a name, apart from any VM, and the mounts in one.

A path in a volume is relative to its root, ``/``-separated, as ``dir/file.txt``; the empty path
is the root. The server refuses one that leads outside the volume.
"""

from __future__ import annotations

from collections.abc import Mapping
from dataclasses import dataclass, field
from enum import Enum
from typing import TYPE_CHECKING, Any, ClassVar

from ._library import args
from .files import Files

if TYPE_CHECKING:
    from .client import Client


class MountType(str, Enum):
    """A mount source's type, without its credentials."""

    NOTION = "notion"
    GDRIVE = "gdrive"
    ONEDRIVE = "onedrive"


@dataclass(frozen=True)
class NotionSource:
    """The pages a Notion integration can reach."""

    mount_type: ClassVar[MountType] = MountType.NOTION

    api_key: str = field(repr=False)

    def to_dict(self) -> dict[str, Any]:
        """As the wire has it."""
        return {"type": "notion", "api_key": self.api_key}


@dataclass(frozen=True)
class GdriveSource:
    """A Google Drive."""

    mount_type: ClassVar[MountType] = MountType.GDRIVE

    client_id: str
    client_secret: str = field(repr=False)
    refresh_token: str = field(repr=False)

    def to_dict(self) -> dict[str, Any]:
        """As the wire has it."""
        return {
            "type": "gdrive",
            "client_id": self.client_id,
            "client_secret": self.client_secret,
            "refresh_token": self.refresh_token,
        }


@dataclass(frozen=True)
class OnedriveSource:
    """A OneDrive. ``client_secret`` is for a confidential app; a public one, as a personal
    account's usually is, has none."""

    mount_type: ClassVar[MountType] = MountType.ONEDRIVE

    client_id: str
    refresh_token: str = field(repr=False)
    client_secret: str | None = field(default=None, repr=False)

    def to_dict(self) -> dict[str, Any]:
        """As the wire has it."""
        value = {
            "type": "onedrive",
            "client_id": self.client_id,
            "refresh_token": self.refresh_token,
        }
        if self.client_secret is not None:
            value["client_secret"] = self.client_secret
        return value


MountSource = NotionSource | GdriveSource | OnedriveSource
"""What a mount shows, and the credentials to reach it."""


@dataclass(frozen=True)
class Volume(Files):
    """One volume the server holds, as ``list_volume``, ``get_volume`` and ``create_volume``
    answer it, and the client that reaches it: its files, and the mounts in it. Dropping it
    leaves the volume; ``remove`` removes it."""

    client: Client = field(repr=False, compare=False)
    """The client its requests go through."""

    name: str

    size: int
    """The bytes the volume's files took on the server when it was answered."""

    @classmethod
    def from_dict(cls, client: Client, value: Mapping[str, Any]) -> Volume:
        return cls(client, value["name"], value["size"])

    @property
    def _root(self) -> dict[str, str]:
        return {"volume": self.name}

    async def mounts(self) -> list[VolumeMountListOutputItem]:
        """The mounts in it, without their credentials."""
        items = await self.client._call("virtx_client_list_volume_mount", args(volume=self.name))
        return [VolumeMountListOutputItem.from_dict(item) for item in items]

    async def mount(self, path: str, source: MountSource) -> VolumeMountListOutputItem:
        """Mount ``source`` at ``path`` in it, a directory the mount creates."""
        output = await self.client._call(
            "virtx_client_create_volume_mount",
            args(volume=self.name, path=path, source=source.to_dict()),
        )
        return VolumeMountListOutputItem.from_dict(output)

    async def unmount(self, path: str) -> None:
        """Take away the mount at ``path``, leaving its source as it is."""
        await self.client._call(
            "virtx_client_remove_volume_mount", args(volume=self.name, path=path)
        )

    async def remove(self) -> None:
        """Delete it and everything in it."""
        await self.client._call("virtx_client_remove_volume", args(name=self.name))


@dataclass(frozen=True)
class VolumeMountListOutputItem:
    """One mount in a volume, as ``Volume.mounts`` and ``Volume.mount`` answer it. Its
    credentials are not answered."""

    path: str
    """Where it is mounted."""

    mount_type: MountType

    @classmethod
    def from_dict(cls, value: Mapping[str, Any]) -> VolumeMountListOutputItem:
        return cls(value["path"], MountType(value["mount_type"]))
