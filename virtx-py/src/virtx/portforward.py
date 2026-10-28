"""A sandbox's port, forwarded to this machine's loopback: a ``VirtxPortForwarder*`` held."""

from __future__ import annotations

import ctypes
import json
from typing import TYPE_CHECKING

from ._library import Library, _Bytes

if TYPE_CHECKING:
    from typing_extensions import Self

    from .client import Client


class PortForwarder:
    """A sandbox's port at an address on this machine's loopback, until it is closed.

    It runs on its client's session, so the client's last close stops it. Over stdio the server
    is on this machine and its own address is the forwarder's; over HTTP each connection to the
    forwarder is carried to the port in a WebSocket.
    """

    def __init__(self, raw: int, lib: Library, address: str) -> None:
        self._raw: int | None = raw
        self._lib = lib
        self.address = address
        """Where the port is reached, as ``host:port``."""

    @classmethod
    def open(cls, client: Client, url: str) -> PortForwarder:
        """Forward the port a sandbox answered ``url`` for, as ``init_sandbox`` and
        ``list_sandbox`` answer one in ``ports``, over ``client``'s session. Blocks until it is
        open."""
        lib = client._lib
        args = json.dumps({"url": url}).encode()
        raw = ctypes.c_void_p()
        out = _Bytes()
        status = lib.virtx_port_forwarder_open(
            client._held(), args, len(args), ctypes.byref(raw), ctypes.byref(out)
        )
        output = lib.take(status, out)
        return cls(raw.value or 0, lib, output["address"])

    @property
    def host(self) -> str:
        """The address's host: a loopback IP."""
        return self.address.rsplit(":", 1)[0].strip("[]")

    @property
    def port(self) -> int:
        """The address's port."""
        return int(self.address.rsplit(":", 1)[1])

    def close(self) -> None:
        """Stop forwarding. Closing twice does nothing."""
        raw, self._raw = self._raw, None
        if raw is not None:
            self._lib.virtx_port_forwarder_free(raw)

    def __enter__(self) -> Self:
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()

    def __del__(self) -> None:
        if getattr(self, "_raw", None) is not None:
            self.close()
