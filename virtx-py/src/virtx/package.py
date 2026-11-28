"""Packages: a sandbox kept under a name, made as many times as it is asked for, by the project's
members and by whom its access lets in; and ``AppClient``, what an app's user reaches a sandbox
they made of one with, by the token that let them in."""

from __future__ import annotations

from collections.abc import Iterable, Mapping
from dataclasses import dataclass, field
from typing import TYPE_CHECKING, Any, Self

from ._library import args
from .sandbox import PortSpec, Sandbox

if TYPE_CHECKING:
    from .client import Auth, Client


@dataclass(frozen=True)
class JwtAccess:
    """Whoever sends a JSON Web Token ``issuer`` signed for ``audience``, carrying each of
    ``claims``: their subject is the token's ``sub``, of that issuer. Each request they send
    carries such a token, which reaches the sandboxes they made and nothing else. Its keys are at ``jwks`` where it is
    given, and otherwise where the issuer's OpenID Connect discovery document says."""

    issuer: str
    audience: str
    jwks: str | None = None
    claims: Mapping[str, Any] = field(default_factory=dict)

    def to_dict(self) -> dict[str, Any]:
        """As the wire has it."""
        value: dict[str, Any] = {"type": "jwt", "issuer": self.issuer, "audience": self.audience}
        if self.jwks is not None:
            value["jwks"] = self.jwks
        if self.claims:
            value["claims"] = dict(self.claims)
        return value


PackageAccess = JwtAccess
"""A way in to make a sandbox of a package, and reach it after, for one who is no member of its
project."""


def _access(value: Mapping[str, Any]) -> PackageAccess:
    return JwtAccess(value["issuer"], value["audience"], value.get("jwks"), value.get("claims", {}))


@dataclass(frozen=True)
class PackageLimits:
    """How many sandboxes made of a package by those its access lets in may live at once: all of
    theirs, and each subject's."""

    sandboxes: int | None = None
    per_subject: int | None = None

    def to_dict(self) -> dict[str, Any]:
        """As the wire has it."""
        value = {"sandboxes": self.sandboxes, "per_subject": self.per_subject}
        return {key: n for key, n in value.items() if n is not None}


@dataclass(frozen=True)
class Package:
    """One package, as ``list_package``, ``get_package`` and ``put_package`` answer it, and the client
    that reaches it. Dropping it leaves the package; ``remove`` removes it."""

    client: Client = field(repr=False, compare=False)
    """The client its requests go through."""

    id: str
    """The id the server gave it, which names its project with it: what an app names it by."""

    name: str

    sandbox: dict[str, Any]
    """What each sandbox made of it is made with, as the wire has it."""

    max: dict[str, int] | None
    access: list[PackageAccess]
    origins: list[str]
    limits: PackageLimits | None

    @classmethod
    def from_dict(cls, client: Client, value: Mapping[str, Any]) -> Package:
        limits = value.get("limits")
        return cls(
            client,
            value["id"],
            value["name"],
            dict(value["sandbox"]),
            value.get("max"),
            [_access(way) for way in value.get("access", ())],
            list(value.get("origins", ())),
            PackageLimits(**limits) if limits is not None else None,
        )

    async def create_sandbox(
        self, *, name: str | None = None, ports: Iterable[PortSpec] = ()
    ) -> Sandbox:
        """Make a sandbox of it, named ``name`` where one is given, with ``ports`` on local."""
        return await self.client.create_package_sandbox(self.id, name=name, ports=ports)

    async def list_sandbox(self) -> list[Sandbox]:
        """The sandboxes made of it that the client's credentials made."""
        return await self.client.list_package_sandbox(self.id)

    async def remove(self) -> None:
        """Remove it. The sandboxes made of it stay, each reached by who made it."""
        await self.client._call("virtx_client_remove_package", args(package=self.id))


class AppClient:
    """One sandbox, made of a package for whom its access lets in, and reached as they are: what
    an app's user runs processes in, reads and writes the files of, and forwards the ports of, as
    ``sandbox``. Every request carries the token it was made with, which reaches the sandboxes
    that user made of the package and nothing else.

    The sandbox lives until it is deleted, with ``remove``: closed, or left by ``async with``,
    this leaves it, to be come back to with ``open`` by its id."""

    def __init__(self, sandbox: Sandbox) -> None:
        self.sandbox = sandbox

    def __repr__(self) -> str:
        return f"AppClient({self.sandbox!r})"

    @classmethod
    async def create(
        cls,
        url: str,
        project: str,
        package: str,
        auth: Auth | None = None,
        *,
        ports: Iterable[PortSpec] = (),
    ) -> AppClient:
        """Make a sandbox of the package named ``package`` in the project ``project``, by the
        project's id, on the server at ``url``, as ``auth`` lets in -- a ``BearerAuth`` of a token
        of the issuer its access names, sent with every request from then on -- with ``ports`` on
        local, each ``on="local"``, beside those the package gives it in the sandbox. Refused as
        the server refuses making it: ``401`` or ``403`` for one the package does not let in, and
        ``429`` past its limits."""
        request = args(package=package, ports=[p.to_dict() for p in ports] or None)
        return await cls._open(url, project, auth, "virtx_client_create_package_sandbox", request)

    @classmethod
    async def open(cls, url: str, project: str, auth: Auth, sandbox: str) -> AppClient:
        """Reach again the sandbox ``sandbox`` in the project ``project``, each by its id, one
        made of a package as ``auth`` lets in, such as ``Client.list_package_sandbox`` finds:
        refused ``401`` or ``403`` for one they did not make, or one deleted."""
        return await cls._open(url, project, auth, "virtx_client_get_sandbox", args(id=sandbox))

    @classmethod
    async def _open(
        cls, url: str, project: str, auth: Auth | None, request: str, arguments: bytes
    ) -> AppClient:
        """A client of the project ``project`` on the server at ``url`` with ``auth``, made one of
        a sandbox's by the library's ``request``, which answers the sandbox: closed where it
        fails."""
        from .client import Client

        client = Client.connect(url, auth, project)
        try:
            made = await client._call(request, arguments)
        except BaseException:
            await client.aclose()
            raise
        return cls(Sandbox.from_dict(client, made))

    async def close(self) -> None:
        """Close the client, leaving its sandbox as it is."""
        await self.sandbox.client.aclose()

    async def remove(self) -> None:
        """Delete its sandbox, and close the client."""
        try:
            await self.sandbox.remove()
        finally:
            await self.sandbox.client.aclose()

    async def __aenter__(self) -> Self:
        return self

    async def __aexit__(self, *exc: object) -> None:
        await self.close()
