"""A sandbox's client port, taken by this client: a ``VirtxStream*`` of
``virtx_client_port_forwarder_open`` held."""

from __future__ import annotations

from typing import Self

from ._library import Library, VirtxError, args
from .sandbox import Sandbox


class ClientPortForwarder:
    """A client port of a sandbox's, one it was made with in its ``client_ports``, taken by this
    client until it is closed: each connection a process in the sandbox makes to
    ``10.0.2.2:{port}`` is carried to its target on this machine, and refused when the target is
    not reached. Closed, the port is let go of, and the connections it carries are closed. What
    fails on the way is logged as a warning by the ``virtx`` logger."""

    def __init__(self, lib: Library, handle: int, port: int) -> None:
        self._lib = lib
        self._handle: int | None = handle
        self.port = port
        """The port, as a process in the sandbox reaches it at ``10.0.2.2``."""

    @classmethod
    async def open(
        cls, sandbox: Sandbox, port: int, target: str | None = None
    ) -> ClientPortForwarder:
        """Take the client port ``port`` of ``sandbox``, one it was made with, its connections
        carried to ``target``, ``host:port``, or ``127.0.0.1:{port}`` without one, over its
        client's session: answered once it is taken. Over HTTP it is reached while this is
        held, beside any other client's; a server on this machine reaches ``127.0.0.1:{port}``
        itself, and refuses another ``target``. Refused ``404`` for a port the sandbox was not
        made with."""
        client = sandbox.client
        lib = client._lib
        request = args(sandbox=sandbox.id, port=port, target=target)
        taken = cls(
            lib, lib.virtx_client_port_forwarder_open(client._held(), request, len(request)), port
        )
        try:
            more, _ = await lib.next_item(taken._handle)
            if not more:
                raise VirtxError(f"client port {port} ended before it was open")
        except BaseException:
            taken.close()
            raise
        return taken

    async def wait_closed(self) -> VirtxError:
        """Wait until it is no longer taken, and answer why: ``404`` once the sandbox is gone, and
        what else ended it."""
        if self._handle is None:
            raise VirtxError("the client port is closed")
        try:
            while (await self._lib.next_item(self._handle))[0]:
                pass
        except VirtxError as error:
            return error
        return VirtxError(f"client port {self.port} ended")

    def close(self) -> None:
        """Let the port go. Closing twice does nothing."""
        handle, self._handle = self._handle, None
        if handle is not None:
            self._lib.virtx_stream_free(handle)

    def __enter__(self) -> Self:
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()

    async def __aenter__(self) -> Self:
        return self

    async def __aexit__(self, *exc: object) -> None:
        self.close()

    def __del__(self) -> None:
        if getattr(self, "_handle", None) is not None:
            self.close()
