"""Projects: what everything a server keeps belongs to, and the members of each, in a role.

A client's requests are in the project ``Client.set_project`` names, or the user's own project
without one.
"""

from __future__ import annotations

from collections.abc import Mapping
from dataclasses import dataclass, field
from enum import Enum
from typing import TYPE_CHECKING, Any

from ._library import args

if TYPE_CHECKING:
    from .client import Client


class Role(str, Enum):
    """What a member of a project may do there."""

    MEMBER = "member"
    """Reaches everything in the project, reads it and its members, and leaves it."""

    OWNER = "owner"
    """Also adds, removes and changes members, and deletes the project."""


@dataclass(frozen=True)
class Project:
    """One project the client has a role in, as ``list_project``, ``get_project`` and
    ``create_project`` answer it, and the client that reaches it: its members, and its removal.
    What is in it, a client reaches with the project set (``Client.set_project``). Dropping it
    leaves the project; ``remove`` removes it."""

    client: Client = field(repr=False, compare=False)
    """The client its requests go through."""

    id: str
    """What names it on any server, given it once."""

    name: str
    """What its owner names it, among their projects."""

    owner: str | None
    """The user whose project it is: ``None`` from a server with no users, such as over stdio."""

    role: Role
    """The client's role in it, as the server answered it."""

    @classmethod
    def from_dict(cls, client: Client, value: Mapping[str, Any]) -> Project:
        return cls(client, value["id"], value["name"], value.get("owner"), Role(value["role"]))

    async def members(self) -> list[ProjectMemberListOutputItem]:
        """Its members, with their roles."""
        items = await self.client._call("virtx_client_list_project_member", args(project=self.id))
        return [ProjectMemberListOutputItem.from_dict(item) for item in items]

    async def set_member(self, user: str, role: Role) -> ProjectMemberListOutputItem:
        """Give ``user`` ``role`` in it, adding them if they are no member."""
        output = await self.client._call(
            "virtx_client_set_project_member",
            args(project=self.id, user=user, role=role),
        )
        return ProjectMemberListOutputItem.from_dict(output)

    async def remove_member(self, user: str) -> None:
        """Take ``user``'s role in it away: theirs to do, leaving it, or an owner's."""
        await self.client._call(
            "virtx_client_remove_project_member", args(project=self.id, user=user)
        )

    async def remove(self) -> None:
        """Delete it and everything in it."""
        await self.client._call("virtx_client_remove_project", args(name=self.id))


@dataclass(frozen=True)
class ProjectMemberListOutputItem:
    """One member of a project, as ``Project.members`` and ``Project.set_member`` answer it."""

    user: str
    """The user as the server names them, such as an email address."""

    role: Role

    @classmethod
    def from_dict(cls, value: Mapping[str, Any]) -> ProjectMemberListOutputItem:
        return cls(value["user"], Role(value["role"]))
