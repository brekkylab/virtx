"""A virtx client for Python, over the C ABI of ``libvirtx_core``.

The client is not compiled into this package: ``libvirtx_core`` is opened at run time, from the
path given to ``load``, or ``$VIRTX_CORE_LIB``, or the copy a wheel carries beside these files,
or the system loader's search for its file name. So every module in a process can make its
requests through the one library, and hand a ``Client`` between them as the ``VirtxClient*`` it
is, with ``Client.as_raw``, ``Client.into_raw`` and ``Client.from_raw``.

Requests take and answer plain values: what the wire calls a map is a ``dict``, a byte string
``bytes``. A request that streams yields ``Progress`` lines, then its ``Output``.
"""

from ._library import LIBRARY_ENV, VirtxError, library_path, load
from .client import Client, Event, Output, Progress, Recipe, image_name

__all__ = [
    "LIBRARY_ENV",
    "Client",
    "Event",
    "Output",
    "Progress",
    "Recipe",
    "VirtxError",
    "image_name",
    "library_path",
    "load",
]
