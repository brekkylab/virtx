"""Presets: a sandbox kept under a name, made as many times as it is asked for, by the project's
members and by whom its access lets in; and ``AppClient``, what an app's user reaches the one
sandbox they made of one with, by its grant alone."""

from __future__ import annotations

from collections.abc import Mapping
from dataclasses import dataclass, field
from typing import TYPE_CHECKING, Any, Self

from ._library import args
from .image import BearerAuth
from .sandbox import Sandbox

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


PresetAccess = PublicAccess | JwtAccess
"""A way in to make a sandbox of a preset, for one who is no member of its project."""


def _access(value: Mapping[str, Any]) -> PresetAccess:
    if value["type"] == "public":
        return PublicAccess()
    return JwtAccess(value["issuer"], value["audience"], value.get("jwks"), value.get("claims", {}))


@dataclass(frozen=True)
class PresetLimits:
    """How many sandboxes made of a preset by those its access lets in may live at once: all of
    theirs, and each subject's."""

    sandboxes: int | None = None
    per_subject: int | None = None

    def to_dict(self) -> dict[str, Any]:
        """As the wire has it."""
        value = {"sandboxes": self.sandboxes, "per_subject": self.per_subject}
        return {key: n for key, n in value.items() if n is not None}


@dataclass(frozen=True)
class Preset:
    """One preset, as ``list_preset``, ``get_preset`` and ``put_preset`` answer it, and the client
    that reaches it. Dropping it leaves the preset; ``remove`` removes it."""

    client: Client = field(repr=False, compare=False)
    """The client its requests go through."""

    id: str
    """The id the server gave it, which names its project with it: what an app names it by."""

    name: str

    sandbox: dict[str, Any]
    """What each sandbox made of it is made with, as the wire has it."""

    max: dict[str, int] | None
    access: list[PresetAccess]
    origins: list[str]
    limits: PresetLimits | None

    @classmethod
    def from_dict(cls, client: Client, value: Mapping[str, Any]) -> Preset:
        limits = value.get("limits")
        return cls(
            client,
            value["id"],
            value["name"],
            dict(value["sandbox"]),
            value.get("max"),
            [_access(way) for way in value.get("access", ())],
            list(value.get("origins", ())),
            PresetLimits(**limits) if limits is not None else None,
        )

    async def create_sandbox(self, *, name: str | None = None) -> PresetSandbox:
        """Make a sandbox of it, named ``name`` where one is given."""
        return await self.client.create_preset_sandbox(self.id, name=name)

    async def remove(self) -> None:
        """Remove it. The sandboxes made of it stay, each with its grant."""
        await self.client._call("virtx_client_remove_preset", args(preset=self.id))


@dataclass(frozen=True)
class PresetSandbox:
    """A sandbox made of a preset, and its grant: the token that reaches it alone, given out as
    it is made and never again, which an ``AppClient`` reaches it with. A server over stdio gives
    none."""

    sandbox: Sandbox
    grant: str | None = field(repr=False)

    @classmethod
    def from_dict(cls, client: Client, value: Mapping[str, Any]) -> PresetSandbox:
        return cls(Sandbox.from_dict(client, value["sandbox"]), value.get("grant"))


class AppClient:
    """One sandbox, made of a preset for whom its access lets in and reached with its grant
    alone: what an app's user runs processes in, reads and writes the files of, and forwards the
    ports of, as ``sandbox``, and deletes with ``close``. The grant reaches nothing else, so
    neither does this."""

    def __init__(self, sandbox: Sandbox) -> None:
        self.sandbox = sandbox

    @classmethod
    async def connect(cls, url: str, preset: str, auth: Auth | None = None) -> AppClient:
        """Make a sandbox of the preset ``preset``, by its id, on the server at ``url``, as
        ``auth`` lets in -- none where the preset is public, or a ``BearerAuth`` of a token of
        the issuer its access names -- and reach it with its grant from then on. Refused as the
        server refuses making it: ``401`` or ``403`` for one the preset does not let in, and
        ``429`` past its limits. One client does both."""
        from .client import Client

        client = Client.connect(url, auth)
        try:
            made = await client.create_preset_sandbox(preset)
            if made.grant is None:
                raise ValueError("the server gave the sandbox no grant to reach it with")
            # From here on, the grant alone: what made it reaches nothing more through this client.
            client.set_auth(BearerAuth(made.grant))
        except BaseException:
            await client.aclose()
            raise
        return cls(made.sandbox)

    async def close(self) -> None:
        """Delete its sandbox, and with it its grant, and close the client."""
        try:
            await self.sandbox.remove()
        finally:
            await self.sandbox.client.aclose()

    async def __aenter__(self) -> Self:
        return self

    async def __aexit__(self, *exc: object) -> None:
        await self.close()
