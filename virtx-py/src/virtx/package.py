"""Packages: a sandbox kept under a name, made as many times as it is asked for, by the project's
members and by whom its access lets in; and ``AppClient``, what an app's user reaches the one
sandbox they made of one with, by its grant alone."""

from __future__ import annotations

from collections.abc import Iterable, Mapping
from dataclasses import dataclass, field
from typing import TYPE_CHECKING, Any, Self

from ._library import args
from .sandbox import PortSpec, Sandbox

if TYPE_CHECKING:
    from .client import Auth, Client


@dataclass(frozen=True)
class PublicAccess:
    """Anyone, with no credentials: their subject is the address they came from."""

    def to_dict(self) -> dict[str, Any]:
        """As the wire has it."""
        return {"type": "public"}


@dataclass(frozen=True)
class JwtAccess:
    """Whoever sends a JSON Web Token ``issuer`` signed for ``audience``, carrying each of
    ``claims``: their subject is the token's ``sub``. Its keys are at ``jwks`` where it is
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


PackageAccess = PublicAccess | JwtAccess
"""A way in to make a sandbox of a package, for one who is no member of its project."""


def _access(value: Mapping[str, Any]) -> PackageAccess:
    if value["type"] == "public":
        return PublicAccess()
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
    ) -> PackageSandbox:
        """Make a sandbox of it, named ``name`` where one is given, with ``ports`` on local."""
        return await self.client.create_package_sandbox(self.id, name=name, ports=ports)

    async def list_sandbox(self) -> list[Sandbox]:
        """The sandboxes made of it that the client's credentials made."""
        return await self.client.list_package_sandbox(self.id)

    async def grant_sandbox(self, sandbox: str) -> PackageSandbox:
        """A new grant to the sandbox ``sandbox``, one ``list_sandbox`` finds."""
        return await self.client.grant_package_sandbox(self.id, sandbox)

    async def remove(self) -> None:
        """Remove it. The sandboxes made of it stay, each with its grant."""
        await self.client._call("virtx_client_remove_package", args(package=self.id))


@dataclass(frozen=True)
class PackageSandbox:
    """A sandbox made of a package, and a grant to it: the token that reaches it alone, given out
    in this answer and no other, which an ``AppClient`` reaches it with. A server over stdio gives
    none."""

    sandbox: Sandbox
    grant: str | None = field(repr=False)

    @classmethod
    def from_dict(cls, client: Client, value: Mapping[str, Any]) -> PackageSandbox:
        return cls(Sandbox.from_dict(client, value["sandbox"]), value.get("grant"))


class AppClient:
    """One sandbox, made of a package for whom its access lets in and reached with its grant
    alone: what an app's user runs processes in, reads and writes the files of, and forwards the
    ports of, as ``sandbox``. The grant reaches nothing else, so neither does this.

    The sandbox lives until it is deleted, with ``remove``: closed, or left by ``async with``,
    this leaves it, to be come back to with ``resume`` and its ``grant``, or with ``resume_as``
    as the one who made it."""

    def __init__(self, sandbox: Sandbox, grant: str) -> None:
        self.sandbox = sandbox
        self.grant = grant
        """The token that reaches its sandbox, to keep and ``resume`` with: as a password to it,
        kept where the app's user's other secrets are."""

    def __repr__(self) -> str:
        return f"AppClient({self.sandbox!r})"

    @classmethod
    async def connect(
        cls, url: str, package: str, auth: Auth | None = None, *, ports: Iterable[PortSpec] = ()
    ) -> AppClient:
        """Make a sandbox of the package ``package``, by its id, on the server at ``url``, as
        ``auth`` lets in -- none where the package is public, or a ``BearerAuth`` of a token of
        the issuer its access names -- with ``ports`` on local, each ``on="local"``, beside
        those the package gives it in the sandbox, and reach it with its grant from then on.
        Refused as the server refuses making it: ``401`` or ``403`` for one the package does not
        let in, and ``429`` past its limits. One client does both."""
        request = args(package=package, ports=[p.to_dict() for p in ports] or None)
        return await cls._open(url, auth, "virtx_client_app_create", request)

    @classmethod
    async def resume(cls, url: str, grant: str) -> AppClient:
        """Reach again the sandbox ``grant`` reaches, on the server at ``url``: refused ``401``
        once the sandbox is deleted."""
        return await cls._open(url, None, "virtx_client_app_resume", args(grant=grant))

    @classmethod
    async def resume_as(cls, url: str, package: str, auth: Auth, sandbox: str) -> AppClient:
        """Reach again the sandbox ``sandbox``, one made of the package ``package`` with
        ``auth``, as ``Client.list_package_sandbox`` finds it, with a new grant: for one who kept
        no grant, let in by who they are. Refused ``401`` with no credentials, and ``404`` for
        a sandbox they did not make."""
        request = args(package=package, sandbox=sandbox)
        return await cls._open(url, auth, "virtx_client_app_resume_as", request)

    @classmethod
    async def _open(cls, url: str, auth: Auth | None, request: str, arguments: bytes) -> AppClient:
        """A client of the server at ``url`` with ``auth``, made one of a sandbox's by the
        library's ``request``, which answers the sandbox and its grant: closed where it fails."""
        from .client import Client

        client = Client.connect(url, auth)
        try:
            made = await client._call(request, arguments)
        except BaseException:
            await client.aclose()
            raise
        return cls(Sandbox.from_dict(client, made["sandbox"]), made["grant"])

    async def close(self) -> None:
        """Close the client, leaving its sandbox as it is."""
        await self.sandbox.client.aclose()

    async def remove(self) -> None:
        """Delete its sandbox, and with it every grant to it, and close the client."""
        try:
            await self.sandbox.remove()
        finally:
            await self.sandbox.client.aclose()

    async def __aenter__(self) -> Self:
        return self

    async def __aexit__(self, *exc: object) -> None:
        await self.close()
