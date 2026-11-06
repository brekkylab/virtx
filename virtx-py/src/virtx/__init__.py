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
yields the job's log, as ``image_job_log`` or ``sandbox_job_log`` does, each a ``LogEntry``
numbered for ``after`` to resume from, until its end entry. A download or upload of an image yields its
``Progress``, each with a line of its log as its ``content``, then its ``Output``.

A sandbox, a volume, a snapshot and a sandbox job are answered as a ``Sandbox``, a ``Volume``, a
``Snapshot`` and a ``SandboxJob``, each carrying the client its requests go through: it is all a
caller hands on to act on one. The three with files read and write them as ``Files``:
``await volume.read_file("a.txt")``. The client's files requests name the root instead, a
``VolumeFiles``, a ``SnapshotFiles`` or a ``SandboxFiles``:
``client.read_file(VolumeFiles("v"), "a.txt")``.

A sandbox's port is reached through a ``PortForwarder``, opened over a client with the ``url``
``create_sandbox`` answered for it: an address on this machine's loopback.
"""

from ._library import LIBRARY_ENV, VirtxError, library_path, load
from .client import Client, Event, Output, Progress, ensure_virtx
from .files import DirEntry, FileRoot, Files, Metadata, SandboxFiles, SnapshotFiles, VolumeFiles
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
    "BearerAuth",
    "Client",
    "DirEntry",
    "Event",
    "FileRoot",
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
    "SandboxFiles",
    "SandboxJob",
    "SandboxJobEnd",
    "SandboxJobEntry",
    "SandboxJobKind",
    "SandboxPort",
    "SandboxVolume",
    "Snapshot",
    "SnapshotFiles",
    "StartedJob",
    "Stderr",
    "Stdout",
    "Truncated",
    "VirtxError",
    "Volume",
    "VolumeFiles",
    "VolumeMountListOutputItem",
    "image_name",
    "library_path",
    "load",
]
