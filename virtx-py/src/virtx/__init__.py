"""A virtx client for Python, over the C ABI of ``libvirtx_core``.

The client is not compiled into this package: ``libvirtx_core`` is opened at run time, from the
path given to ``load``, or ``$VIRTX_CORE_LIB``, or the copy a wheel carries beside these files,
or the system loader's search for its file name. So every module in a process can make its
requests through the one library, and hand a ``Client`` between them as the ``VirtxClient*`` it
is, with ``Client.as_raw``, ``Client.into_raw`` and ``Client.from_raw``.

Requests take and answer the types of ``virtx.image``, ``virtx.volume`` and ``virtx.files``,
and plain values elsewhere: what the wire calls a map is a ``dict``. A request that streams
yields its ``Progress``, each with a line of its log as its ``content``, then its ``Output``.

The files of a volume or a snapshot are read and written through the client too, each request
naming its root as a ``Volume`` or a ``Snapshot``: ``client.read_file(Volume("v"), "a.txt")``.

A sandbox's port is reached through a ``PortForwarder``, opened over a client with the ``url``
the sandbox answered for it: an address on this machine's loopback.
"""

from ._library import LIBRARY_ENV, VirtxError, library_path, load
from .client import Client, Event, Output, Progress
from .files import DirEntry, FileRoot, Metadata, Snapshot, Volume
from .image import (
    BasicAuth,
    BearerAuth,
    ImageBuildOutput,
    ImageDigest,
    ImageDownloadOutput,
    ImageListOutputItem,
    ImageName,
    ImagePruneOutput,
    ImagePullOutput,
    ImagePushOutput,
    ImageTag,
    ImageUploadOutput,
    Recipe,
    RegistryAuth,
    image_name,
)
from .portforward import PortForwarder
from .volume import (
    GdriveSource,
    MountSource,
    MountType,
    NotionSource,
    OnedriveSource,
    VolumeCreateOutput,
    VolumeListOutputItem,
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
    "GdriveSource",
    "ImageBuildOutput",
    "ImageDigest",
    "ImageDownloadOutput",
    "ImageListOutputItem",
    "ImageName",
    "ImagePruneOutput",
    "ImagePullOutput",
    "ImagePushOutput",
    "ImageTag",
    "ImageUploadOutput",
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
    "Snapshot",
    "VirtxError",
    "Volume",
    "VolumeCreateOutput",
    "VolumeListOutputItem",
    "VolumeMountListOutputItem",
    "image_name",
    "library_path",
    "load",
]
