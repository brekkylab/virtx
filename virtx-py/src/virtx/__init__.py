"""A virtx client for Python, over the C ABI of ``libvirtx_core``.

The client is not compiled into this package: ``libvirtx_core`` is opened at run time, from the
path given to ``load``, or ``$VIRTX_CORE_LIB``, or the copy a wheel carries beside these files,
or the system loader's search for its file name. So every module in a process can make its
requests through the one library, and hand a ``Client`` between them as the ``VirtxClient*`` it
is, with ``Client.as_raw``, ``Client.into_raw`` and ``Client.from_raw``.

Requests take and answer the types of ``virtx.image``, ``virtx.snapshot``, ``virtx.volume``,
``virtx.sandbox`` and ``virtx.files``, and plain values elsewhere: what the wire calls a map is a
``dict``.

A build, pull or push of an image, and a process in a sandbox, is a job the server runs apart from
the request that started it. The method that starts one answers a ``StartedJob``, sent when first
awaited or iterated: awaited, it is the job, an ``ImageJob`` or a ``SandboxJob``; iterated, it
yields the job's log, as ``ImageJob.log`` or ``SandboxJob.log`` does, each a ``LogEntry``
numbered for ``after`` to resume from, until its end entry. A download or upload of an image yields its
``Progress``, each with a line of its log as its ``content``, then its ``Output``.

A sandbox, a volume, a snapshot, a sandbox job and an image job are answered as a ``Sandbox``, a
``Volume``, a ``Snapshot``, a ``SandboxJob`` and an ``ImageJob``, each carrying the client its
requests go through: what acts on one is the handle, so it is all a caller hands on. The client
lists, makes and gets them; one known by its id or name is had with ``get_sandbox``,
``get_volume``, ``get_snapshot``, ``get_image_job`` or ``Sandbox.job``. The three with files read and write them as ``Files``:
``await volume.read_file("a.txt")``.

A sandbox's port is reached through a ``PortForwarder``, opened over a client with the ``url``
``create_sandbox`` answered for it: an address on this machine's loopback.
"""

from ._library import LIBRARY_ENV, VirtxError, library_path, load
from .client import Auth, Client, CommandAuth, Event, Output, Progress, ensure_virtx
from .files import DirEntry, Files, Metadata
from .image import (
    BasicAuth,
    BearerAuth,
    ImageDigest,
    ImageDownloadOutput,
    ImageJob,
    ImageJobEnd,
    ImageJobEntry,
    ImageJobKind,
    ImageListOutputItem,
    ImageName,
    ImagePruneOutput,
    ImageTag,
    ImageUploadOutput,
    Log,
    Recipe,
    RegistryAuth,
    image_name,
)
from .job import LogEntry, StartedJob, Truncated
from .portforward import PortForwarder
from .sandbox import (
    Sandbox,
    SandboxJob,
    SandboxJobEnd,
    SandboxJobEntry,
    SandboxJobKind,
    SandboxPort,
    SandboxVolume,
    Stderr,
    Stdout,
)
from .snapshot import Snapshot
from .volume import (
    GdriveSource,
    MountSource,
    MountType,
    NotionSource,
    OnedriveSource,
    Volume,
    VolumeMountListOutputItem,
)

__all__ = [
    "LIBRARY_ENV",
    "BasicAuth",
    "Auth",
    "BearerAuth",
    "Client",
    "CommandAuth",
    "DirEntry",
    "Event",
    "Files",
    "GdriveSource",
    "ImageDigest",
    "ImageDownloadOutput",
    "ImageJob",
    "ImageJobEnd",
    "ImageJobEntry",
    "ImageJobKind",
    "ImageListOutputItem",
    "ImageName",
    "ImagePruneOutput",
    "ImageTag",
    "ImageUploadOutput",
    "Log",
    "LogEntry",
    "Metadata",
    "MountSource",
    "MountType",
    "NotionSource",
    "OnedriveSource",
    "Output",
    "PortForwarder",
    "Progress",
    "Recipe",
    "RegistryAuth",
    "Sandbox",
    "SandboxJob",
    "SandboxJobEnd",
    "SandboxJobEntry",
    "SandboxJobKind",
    "SandboxPort",
    "SandboxVolume",
    "Snapshot",
    "StartedJob",
    "Stderr",
    "Stdout",
    "Truncated",
    "VirtxError",
    "Volume",
    "VolumeMountListOutputItem",
    "ensure_virtx",
    "image_name",
    "library_path",
    "load",
]
