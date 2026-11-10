"""A sandbox's port, forwarded to this machine's loopback: a ``VirtxPortForwarder*`` held."""

from __future__ import annotations

import ctypes
import json
from typing import TYPE_CHECKING

from ._library import Library, VirtxError, _Bytes
from .sandbox import SandboxPort

if TYPE_CHECKING:
    from typing_extensions import Self

    from .client import Client


class PortForwarder:
    """A sandbox's port at an address on this machine's loopback, until it is closed.

    It runs on its client's session, so the client's last close stops it. Over stdio the server
    is on this machine and its own address is the forwarder's; over HTTP each connection to the
    forwarder is carried to the port in a WebSocket. A connection that reaches nothing in the
    sandbox -- its WebSocket refused, or closed before the port sent a byte, as it is when
    nothing listens there -- is closed, and why is ``last_error``, and logged as a warning by
    the ``virtx`` logger.
    """

    def __init__(self, raw: int, lib: Library, address: str) -> None:
        self._raw: int | None = raw
        self._lib = lib
        self.address = address
        """Where the port is reached, as ``host:port``."""

    @classmethod
    def open(cls, client: Client, port: SandboxPort | str) -> PortForwarder:
        """Forward ``port``, as ``create_sandbox`` answers one in ``ports``, or the ``url`` it
        answered for it, over ``client``'s session. Given the port, what a connection that
        fails says names its number. Returns as soon as its address on this machine's loopback
        is open, which takes no request of the server: each connection to it is carried as it
        comes, so it holds up no event loop it is called on."""
        lib = client._lib
        if isinstance(port, SandboxPort):
            args = json.dumps({"url": port.url, "port": port.port}).encode()
        else:
            args = json.dumps({"url": port}).encode()
        raw = ctypes.c_void_p()
        out = _Bytes()
        status = lib.virtx_port_forwarder_open(
            client._held(), args, len(args), ctypes.byref(raw), ctypes.byref(out)
        )
        output = lib.take(status, out)
        return cls(raw.value or 0, lib, output["address"])

    @property
    def last_error(self) -> VirtxError | None:
        """Why the last connection to the forwarder that reached nothing in the sandbox failed,
        its message naming the port; ``None`` while none has. ``status`` and ``code`` are the
        server's, where it refused the connection's WebSocket. A server on this machine is
        reached at its own address, not through the forwarder, so it has none."""
        if self._raw is None:
            raise VirtxError("the forwarder is closed")
        out = _Bytes()
        status = self._lib.virtx_port_forwarder_last_error(self._raw, ctypes.byref(out))
        try:
            self._lib.take(status, out)
        except VirtxError as error:
            return error
        return None

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
