"""Volumes: storage the server keeps under a name, apart from any VM, and the files in one.

A path in a volume is relative to its root, ``/``-separated, as ``dir/file.txt``; the empty path
is the root. The server refuses one that leads outside the volume.
"""

from __future__ import annotations

from collections.abc import Mapping
from dataclasses import dataclass, field
from enum import Enum
from typing import Any, ClassVar


class FileType(str, Enum):
    """What a path in a volume is."""

    FILE = "file"
    DIR = "dir"


@dataclass(frozen=True)
class Metadata:
    """A path's type, size and modification time."""

    file_type: FileType

    size: int
    """The file's length in bytes; for a directory, what the server's filesystem reports."""

    modified: int
    """When the contents last changed, in nanoseconds since the Unix epoch."""

    @classmethod
    def from_dict(cls, value: Mapping[str, Any]) -> Metadata:
        return cls(FileType(value["type"]), value["size"], value["modified"])


@dataclass(frozen=True)
class DirEntry:
    """One entry of a directory: its name within the directory, and its metadata."""

    name: str
    metadata: Metadata

    @classmethod
    def from_dict(cls, value: Mapping[str, Any]) -> DirEntry:
        return cls(value["name"], Metadata.from_dict(value["metadata"]))


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
class VolumeListOutputItem:
    """One volume the server holds."""

    name: str

    size: int
    """The bytes the volume's files take on the server."""

    @classmethod
    def from_dict(cls, value: Mapping[str, Any]) -> VolumeListOutputItem:
        return cls(value["name"], value["size"])


@dataclass(frozen=True)
class VolumeCreateOutput:
    """What ``create_volume`` answers."""

    name: str
    """The volume's name: the one asked for, or the one the server picked."""

    @classmethod
    def from_dict(cls, value: Mapping[str, Any]) -> VolumeCreateOutput:
        return cls(value["name"])


@dataclass(frozen=True)
class VolumeMountListOutputItem:
    """One mount in a volume. Its credentials are not answered."""

    path: str
    """Where it is mounted."""

    mount_type: MountType

    @classmethod
    def from_dict(cls, value: Mapping[str, Any]) -> VolumeMountListOutputItem:
        return cls(value["path"], MountType(value["mount_type"]))
