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
from enum import Enum
from typing import Any

from .job import LogEntry, Truncated


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
class BasicAuth:
    """A username, and a password or an access token the registry takes in its place."""

    username: str
    password: str = field(repr=False)

    def to_dict(self) -> dict[str, Any]:
        """As the wire has it."""
        return {"type": "basic", "username": self.username, "password": self.password}


@dataclass(frozen=True)
class BearerAuth:
    """A token taken as it is, sent as ``Authorization: Bearer <token>``: a registry's, or the
    server's own, as :meth:`Client.connect` takes it."""

    token: str = field(repr=False)

    def to_dict(self) -> dict[str, Any]:
        """As the wire has it."""
        return {"type": "bearer", "token": self.token}


RegistryAuth = BasicAuth | BearerAuth
"""Credentials for a remote image registry."""


class ImageJobKind(str, Enum):
    """What an image job does."""

    PULL = "pull"
    PUSH = "push"
    BUILD = "build"


@dataclass(frozen=True)
class ImageJob:
    """A build, pull or push the server runs: what ``build_image``, ``pull_image`` and
    ``push_image`` answer. Its log is ``image_job_log``'s."""

    id: str
    """The job's id, which every request about it names it by."""

    kind: ImageJobKind

    reference: str
    """The reference it was started with."""

    running: bool
    """Whether it runs still."""

    digest: str | None = None
    """Once it has succeeded, as ``algorithm:hex``: the digest of the image a pull or build
    left here, or of the manifest a push put on the registry."""

    error: str | None = None
    """Once it has failed, why, for a person."""

    @classmethod
    def from_dict(cls, value: Mapping[str, Any]) -> ImageJob:
        return cls(
            value["id"],
            ImageJobKind(value["kind"]),
            value["reference"],
            value["running"],
            value.get("digest"),
            value.get("error"),
        )


@dataclass(frozen=True)
class Log:
    """A line of an image job's log."""

    line: str


@dataclass(frozen=True)
class ImageJobEnd:
    """How an image job ended, last in its log: with ``digest`` if it succeeded, ``error`` if
    not."""

    digest: str | None = None
    error: str | None = None


ImageJobEntry = Log | Truncated | ImageJobEnd
"""An entry of an image job's log."""


def image_job_entry(item: Mapping[str, Any]) -> LogEntry[ImageJobEntry]:
    """An item of ``image_job_log``'s stream, as ``{"id": n, "log": "..."}`` and the like."""
    entry: ImageJobEntry
    if "log" in item:
        entry = Log(item["log"])
    elif "truncated" in item:
        entry = Truncated()
    else:
        end = item["end"]
        entry = ImageJobEnd(end.get("digest"), end.get("error"))
    return LogEntry(item["id"], entry)


@dataclass(frozen=True)
class ImageListOutputItem:
    """One tag on the server's images."""

    reference: str
    """The tag, as ``name:tag``."""

    digest: str
    """The digest of the manifest it points at, as ``algorithm:hex``."""

    @classmethod
    def from_dict(cls, value: Mapping[str, Any]) -> ImageListOutputItem:
        return cls(value["reference"], value["digest"])


@dataclass(frozen=True)
class ImageDownloadOutput:
    """What ``download_image`` wrote."""

    digest: str
    """The digest of the manifest written, as ``algorithm:hex``."""

    @classmethod
    def from_dict(cls, value: Mapping[str, Any]) -> ImageDownloadOutput:
        return cls(value["digest"])


@dataclass(frozen=True)
class ImageUploadOutput:
    """What ``upload_image`` uploaded."""

    digest: str
    """The digest of the manifest uploaded, as ``algorithm:hex``."""

    @classmethod
    def from_dict(cls, value: Mapping[str, Any]) -> ImageUploadOutput:
        return cls(value["digest"])


@dataclass(frozen=True)
class ImagePruneOutput:
    """What ``prune_image`` deleted."""

    images: list[str]
    """The digest of each image deleted, as ``algorithm:hex``."""

    reclaimed: int
    """How many bytes the server freed."""

    @classmethod
    def from_dict(cls, value: Mapping[str, Any]) -> ImagePruneOutput:
        return cls(list(value["images"]), value["reclaimed"])
