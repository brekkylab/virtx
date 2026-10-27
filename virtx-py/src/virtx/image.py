"""The images the server keeps, and the names it keeps them under.

- Digest: the name of an image, the hash of its manifest, as ``algorithm:hex`` (such as
  ``sha256:0123abcd...``). It identifies one image's content and never changes.
- Reference: a name a person gives an image, as ``name:tag`` (such as ``alpine:3.20``). It points
  at one image, and can be moved to another. Any number of references may point at one image.
"""

from __future__ import annotations

import re
from collections.abc import Iterable, Mapping
from dataclasses import dataclass
from typing import Any


@dataclass(frozen=True)
class ImageTag:
    """An image by a tag on it, as ``name:tag``."""

    tag: str

    def to_dict(self) -> dict[str, Any]:
        """As the wire has it."""
        return {"type": "tag", "tag": self.tag}


@dataclass(frozen=True)
class ImageDigest:
    """An image by the digest of its manifest, as ``algorithm:hex``."""

    digest: str

    def to_dict(self) -> dict[str, Any]:
        """As the wire has it."""
        return {"type": "digest", "digest": self.digest}


ImageName = ImageTag | ImageDigest
"""An image, by a tag on it or by its digest."""

_DIGEST = re.compile(r"(sha256:[0-9a-f]{64}|sha512:[0-9a-f]{128})")


def image_name(image: str | ImageName) -> ImageName:
    """``image`` as the wire names one: a full ``sha256:`` or ``sha512:`` digest is a digest, any
    other string a tag. An ``ImageName`` is taken as it is."""
    if not isinstance(image, str):
        return image
    if _DIGEST.fullmatch(image):
        return ImageDigest(image)
    return ImageTag(image)


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
class ImageBuildOutput:
    """What ``build_image`` built."""

    digest: str
    """The digest of the image's manifest, as ``algorithm:hex``."""

    @classmethod
    def from_dict(cls, value: Mapping[str, Any]) -> ImageBuildOutput:
        return cls(value["digest"])
