"""A sandbox's port forwarded by this client, on either side of the sandbox: a
``VirtxPortForwarder*`` held."""

from __future__ import annotations

import ctypes
from typing import Self

from ._library import Library, VirtxError, _Bytes, args
from .sandbox import PortSide, Sandbox


class PortForwarder:
    """A sandbox's port, on either side of it, forwarded until it is closed.

    A port in the sandbox is reached at the forwarder's ``local`` end, an address on this
    machine. A port on local, one the sandbox was made with ``on="local"``, is reached by the
    sandbox's processes at ``10.0.2.2:{port}``, each of their connections carried to ``local``,
    its target on this machine, and refused when that is not reached.

    It runs on its client's session, so the client's last close stops it. A connection that fails
    on the way is closed, and what was seen is ``last_error``, and logged as a warning by the
    ``virtx`` logger.
    """

    def __init__(self, lib: Library, raw: int | None, port: int, on: PortSide) -> None:
        self._lib = lib
        self._raw = raw
        self.port = port
        """The port, on its side."""
        self.on: PortSide = on
        """Which side the port is on."""
        self.local = ""
        """Its local end, as ``host:port``: for a port in the sandbox the address it is reached
        at, for a port on local the target its connections are carried to."""

    @classmethod
    async def open(
        cls, sandbox: Sandbox, port: int, *, on: PortSide = "sandbox", local: str | None = None
    ) -> PortForwarder:
        """Forward the port ``port`` of ``sandbox``, one it was made with on the side ``on``, over
        its client's session: answered once it is open. ``local``, ``host:port``, is its local
        end: for a port in the sandbox the address it is reached at, a free port on
        ``127.0.0.1`` without one, or over stdio the address the server answered for the port;
        for a port on local the target its connections are carried to, ``127.0.0.1:{port}``
        without one. A port on local the sandbox was not made with is refused ``404``."""
        client = sandbox.client
        lib = client._lib
        url = next(
            (p.url for p in sandbox.ports if p.on == on and p.port == port),
            None,
        )
        request = args(sandbox=sandbox.id, port=port, on=on, url=url, local=local)
        raw = ctypes.c_void_p()
        opening = lib.virtx_port_forwarder_open(
            client._held(), request, len(request), ctypes.byref(raw)
        )
        forwarder = cls(lib, raw.value, port, on)
        try:
            forwarder.local = (await lib.answer(opening))["local"]
        except BaseException:
            forwarder.close()
            raise
        return forwarder

    @property
    def last_error(self) -> VirtxError | None:
        """Why the last connection through the forwarder that failed did, its message naming the
        port, with the server's reason for closing it if it gave one; ``None`` while none has,
        and again once a later connection reaches the port. ``status`` is the server's, where it
        refused the connection's WebSocket. A server on this machine reaches what it is itself,
        not through the forwarder, so for a port in the sandbox at its own address, and for a
        port on local over stdio, it has none."""
        if self._raw is None:
            raise VirtxError("the forwarder is closed")
        out = _Bytes()
        status = self._lib.virtx_port_forwarder_last_error(self._raw, ctypes.byref(out))
        try:
            self._lib.take(status, out)
        except VirtxError as error:
            return error
        return None

    async def wait_closed(self) -> VirtxError:
        """Wait until it has ended, and answer what ended it: for a port on local, ``404`` once
        its sandbox is gone. A port in the sandbox is forwarded until it is closed."""
        if self._raw is None:
            raise VirtxError("the forwarder is closed")
        try:
            await self._lib.answer(self._lib.virtx_port_forwarder_closed(self._raw))
        except VirtxError as error:
            return error
        return VirtxError(f"port {self.port} ended")

    def close(self) -> None:
        """Stop forwarding. Closing twice does nothing."""
        raw, self._raw = self._raw, None
        if raw is not None:
            self._lib.virtx_port_forwarder_free(raw)

    def __enter__(self) -> Self:
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()

    async def __aenter__(self) -> Self:
        return self

    async def __aexit__(self, *exc: object) -> None:
        self.close()

    def __del__(self) -> None:
        if getattr(self, "_raw", None) is not None:
            self.close()
