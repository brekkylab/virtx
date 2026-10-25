"""The images the server keeps, and the names it keeps them under.

- Digest: the name of an image, the hash of its manifest, as ``algorithm:hex`` (such as
  ``sha256:0123abcd...``). It identifies one image's content and never changes.
- Reference: a name a person gives an image, as ``name:tag`` (such as ``alpine:3.20``). It points
  at one image, and can be moved to another. Any number of references may point at one image.
"""

from __future__ import annotations

import re
from collections.abc import Iterable, Mapping
from dataclasses import dataclass, field
from typing import Any


@dataclass(frozen=True)
class ImageReference:
    """An image by a reference to it, as ``name:tag``."""

    reference: str

    def to_dict(self) -> dict[str, Any]:
        """As the wire has it."""
        return {"type": "reference", "reference": self.reference}


@dataclass(frozen=True)
class ImageDigest:
    """An image by the digest of its manifest, as ``algorithm:hex``."""

    digest: str

    def to_dict(self) -> dict[str, Any]:
        """As the wire has it."""
        return {"type": "digest", "digest": self.digest}


ImageName = ImageReference | ImageDigest
"""An image, by a reference to it or by its digest."""

_DIGEST = re.compile(r"(sha256:[0-9a-f]{64}|sha512:[0-9a-f]{128})")


def image_name(image: str | ImageName) -> ImageName:
    """``image`` as the wire names one: a full ``sha256:`` or ``sha512:`` digest is a digest, any
    other string a reference. An ``ImageName`` is taken as it is."""
    if not isinstance(image, str):
        return image
    if _DIGEST.fullmatch(image):
        return ImageDigest(image)
    return ImageReference(image)


@dataclass(frozen=True)
class BasicAuth:
    """A username, and a password or an access token the registry takes in its place."""

    username: str
    password: str = field(repr=False)

    def to_dict(self) -> dict[str, Any]:
        """As the wire has it."""
        return {"type": "basic", "username": self.username, "password": self.password}


@dataclass(frozen=True)
class BearerAuth:
    """A token the registry takes as is."""

    token: str = field(repr=False)

    def to_dict(self) -> dict[str, Any]:
        """As the wire has it."""
        return {"type": "bearer", "token": self.token}


RegistryAuth = BasicAuth | BearerAuth
"""Credentials for a remote image registry."""


class Recipe:
    """A base and the steps over it, which the server builds: ``Recipe("alpine:3.20").run("apk
    add jq")``. Each step returns the recipe, so they chain."""

    def __init__(self, base: str, steps: Iterable[Mapping[str, Any]] = ()) -> None:
        self.base = base
        self.steps: list[Mapping[str, Any]] = list(steps)

    def run(self, command: str) -> Recipe:
        """A command, run through ``sh -c`` with every earlier ``env``."""
        self.steps.append({"run": command})
        return self

    def env(self, key: str, value: str) -> Recipe:
        """A variable for every later ``run`` and the built image's config."""
        self.steps.append({"env": {"key": key, "value": value}})
        return self

    def workdir(self, directory: str) -> Recipe:
        """Where later steps run, and the built image's working directory."""
        self.steps.append({"workdir": directory})
        return self

    def to_dict(self) -> dict[str, Any]:
        """As the wire has it."""
        return {"v": 1, "base": self.base, "steps": list(self.steps)}


@dataclass(frozen=True)
class ImageListOutputItem:
    """One reference the server holds. Two references to one image are two items with the same
    digest."""

    reference: str
    """The reference, as ``name:tag``."""

    digest: str
    """The digest of the image's manifest, as ``algorithm:hex``."""

    size: int
    """The bytes the image's layers take on the server, counting a layer shared with another
    image in both."""

    @classmethod
    def from_dict(cls, value: Mapping[str, Any]) -> ImageListOutputItem:
        return cls(value["reference"], value["digest"], value["size"])


@dataclass(frozen=True)
class ImagePullOutput:
    """What ``pull_image`` pulled."""

    digest: str
    """The digest of the image's manifest, as ``algorithm:hex``."""

    @classmethod
    def from_dict(cls, value: Mapping[str, Any]) -> ImagePullOutput:
        return cls(value["digest"])


@dataclass(frozen=True)
class ImageBuildOutput:
    """What ``build_image`` built."""

    digest: str
    """The digest of the image's manifest, as ``algorithm:hex``."""

    @classmethod
    def from_dict(cls, value: Mapping[str, Any]) -> ImageBuildOutput:
        return cls(value["digest"])
