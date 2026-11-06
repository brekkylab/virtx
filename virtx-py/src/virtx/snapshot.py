"""Snapshots: an upper layer the server keeps under a name, which a sandbox stands on over its
image and writes to."""

from __future__ import annotations

from collections.abc import Mapping
from dataclasses import dataclass, field
from typing import TYPE_CHECKING, Any

from .files import Files, SnapshotFiles

if TYPE_CHECKING:
    from .client import Client


@dataclass(frozen=True)
class Snapshot(Files):
    """One snapshot the server holds, as ``list_snapshot``, ``get_snapshot`` and
    ``create_snapshot`` answer it, and the client that reaches it: its files, refused while a
    sandbox holds it. Dropping it leaves the snapshot; ``remove`` removes it."""

    client: Client = field(repr=False, compare=False)
    """The client its requests go through."""

    name: str

    size: int
    """The bytes its upper layer takes on the server."""

    in_use: bool
    """Whether a sandbox held it when it was answered."""

    @classmethod
    def from_dict(cls, client: Client, value: Mapping[str, Any]) -> Snapshot:
        return cls(client, value["name"], value["size"], value["in_use"])

    @property
    def root(self) -> SnapshotFiles:
        """Its files, for the client's files requests."""
        return SnapshotFiles(self.name)

    async def remove(self) -> None:
        """Delete it and its upper layer; refused with a ``409`` while a sandbox holds it."""
        await self.client.remove_snapshot(self.name)
