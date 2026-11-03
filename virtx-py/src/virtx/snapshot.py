"""Snapshots: an upper layer the server keeps under a name, which a sandbox stands on over its
image and writes to. Its files are reached as a ``files.Snapshot``."""

from __future__ import annotations

from collections.abc import Mapping
from dataclasses import dataclass
from typing import Any


@dataclass(frozen=True)
class SnapshotListOutputItem:
    """One snapshot the server holds, as ``list_snapshot``, ``get_snapshot`` and
    ``create_snapshot`` answer it."""

    name: str

    size: int
    """The bytes its upper layer takes on the server."""

    in_use: bool
    """Whether a sandbox holds it now."""

    @classmethod
    def from_dict(cls, value: Mapping[str, Any]) -> SnapshotListOutputItem:
        return cls(value["name"], value["size"], value["in_use"])
