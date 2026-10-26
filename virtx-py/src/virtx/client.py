"""The client: a ``VirtxClient*`` held, and each request of ``virtx.h`` as a method."""

from __future__ import annotations

import asyncio
import atexit
import ctypes
import json
import weakref
from collections.abc import AsyncIterator, Callable, Iterable, Mapping
from dataclasses import dataclass
from typing import TYPE_CHECKING, Any, Generic, TypeVar

from ._library import OK, Library, VirtxError, _Bytes, library
from .image import ImageBuildOutput, ImageName, Recipe, image_name
from .volume import (
    MountSource,
    VolumeCreateOutput,
    VolumeListOutputItem,
    VolumeMountListOutputItem,
)

if TYPE_CHECKING:
    from typing_extensions import Self

T = TypeVar("T")
C = TypeVar("C")


@dataclass(frozen=True)
class Progress(Generic[C]):
    """A notification about a request: how far it has got, what that will reach when known, and
    its ``content``, if it sent one, such as a line of its log."""

    progress: int
    total: int | None = None
    content: C | None = None


@dataclass(frozen=True)
class Output(Generic[T]):
    """What a request answered, after its progress."""

    value: T


Event = Output[T] | Progress[C]


def _args(**args: Any) -> bytes:
    """``args`` without the ones left out, as JSON."""
    return json.dumps({key: value for key, value in args.items() if value is not None}).encode()


def _event(item: Mapping[str, Any], output: Callable[[Any], T]) -> Event[T, Any]:
    if "progress" in item:
        progress = item["progress"]
        return Progress(progress["progress"], progress.get("total"), progress.get("content"))
    return Output(output(item["output"]))


# Every client not yet closed, closed at exit: a hold left to the interpreter's teardown would
# end its session after the waker's callbacks can still run.
_open: weakref.WeakSet[Client] = weakref.WeakSet()


@atexit.register
def _close_all() -> None:
    for client in list(_open):
        client.close()


class Client:
    """A client of one server, over stdio or Streamable HTTP, made by ``libvirtx_core``.

    Each method is a coroutine, or an async iterator for a request that streams its progress.
    Holds of one session are counted: ``clone`` takes another, ``close`` lets this one go, and
    the session ends with the last, which answers every request still waiting with an error.
    """

    def __init__(self, raw: int, lib: Library) -> None:
        self._raw: int | None = raw
        self._lib = lib
        _open.add(self)

    @classmethod
    def spawn(cls, program: str, args: Iterable[str] = ()) -> Client:
        """Start ``program`` with ``args`` as the server, and talk to it over its stdin and
        stdout. Blocks until it has started."""
        lib = library()
        argv = [arg.encode() for arg in args]
        array = (ctypes.c_char_p * len(argv))(*argv)
        raw = ctypes.c_void_p()
        error = _Bytes()
        status = lib.virtx_client_spawn(
            program.encode(), array, len(argv), ctypes.byref(raw), ctypes.byref(error)
        )
        return cls._opened(lib, status, raw, error)

    @classmethod
    def connect(cls, url: str) -> Client:
        """Reach the server at ``url``, its base ``http`` or ``https`` URL (``/rpc`` is under
        it)."""
        lib = library()
        raw = ctypes.c_void_p()
        error = _Bytes()
        status = lib.virtx_client_connect(url.encode(), ctypes.byref(raw), ctypes.byref(error))
        return cls._opened(lib, status, raw, error)

    @classmethod
    def _opened(cls, lib: Library, status: int, raw: ctypes.c_void_p, error: _Bytes) -> Client:
        if status == OK and raw.value:
            return cls(raw.value, lib)
        lib.take(status, error)
        raise VirtxError(f"unknown status {status}")

    @classmethod
    def from_raw(cls, raw: int) -> Client:
        """Take over one hold of the ``VirtxClient*`` ``raw``, as ``into_raw`` or C's
        ``virtx_client_retain`` gave it, made by the library this process opened."""
        return cls(raw, library())

    def as_raw(self) -> int:
        """The ``VirtxClient*`` this holds, still held by it."""
        return self._held()

    def into_raw(self) -> int:
        """Give up this hold of the ``VirtxClient*``, for whoever takes the pointer to release.
        This client is closed after."""
        raw = self._held()
        self._raw = None
        _open.discard(self)
        return raw

    def clone(self) -> Client:
        """Another hold of the same session."""
        raw = self._held()
        self._lib.virtx_client_retain(raw)
        return Client(raw, self._lib)

    def close(self) -> None:
        """Let go of this hold; the last ends the session, blocking until it has. Closing twice
        does nothing."""
        raw, self._raw = self._raw, None
        _open.discard(self)
        if raw is not None:
            self._lib.virtx_client_release(raw)

    async def aclose(self) -> None:
        """``close``, off the event loop's thread."""
        await asyncio.to_thread(self.close)

    def __enter__(self) -> Self:
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()

    async def __aenter__(self) -> Self:
        return self

    async def __aexit__(self, *exc: object) -> None:
        await self.aclose()

    def __del__(self) -> None:
        if getattr(self, "_raw", None) is not None:
            self.close()

    def _held(self) -> int:
        if self._raw is None:
            raise VirtxError("the client is closed")
        return self._raw

    async def _call(self, name: str, args: bytes | None = None) -> Any:
        start = getattr(self._lib, name)
        raw = self._held()
        handle = start(raw) if args is None else start(raw, args, len(args))
        return await self._lib.answer(handle)

    async def _events(
        self, name: str, args: bytes, output: Callable[[Any], T]
    ) -> AsyncIterator[Event[T, Any]]:
        handle = getattr(self._lib, name)(self._held(), args, len(args))
        async for item in self._lib.items(handle):
            yield _event(item, output)

    def build_image(
        self, recipe: Recipe | Mapping[str, Any], reference: str
    ) -> AsyncIterator[Event[ImageBuildOutput, str]]:
        """Build ``recipe``, and point ``reference`` at it. Its events are the build's log, then
        what it built."""
        recipe = recipe.to_dict() if isinstance(recipe, Recipe) else recipe
        return self._events(
            "virtx_client_build_image",
            _args(recipe=recipe, reference=reference),
            ImageBuildOutput.from_dict,
        )

    async def list_snapshot(self) -> list[dict[str, Any]]:
        """Every snapshot the server holds, as ``{"name", "size", "in_use"}``, ``in_use`` whether
        a sandbox holds it now."""
        return await self._call("virtx_client_list_snapshot")

    async def create_snapshot(
        self, name: str | None = None, disk_gib: int | None = None
    ) -> dict[str, Any]:
        """Keep a new, empty upper layer under ``name``, holding at most ``disk_gib`` GiB;
        without either the server picks it. Answers ``{"name"}``."""
        return await self._call("virtx_client_create_snapshot", _args(name=name, disk_gib=disk_gib))

    async def remove_snapshot(self, name: str) -> None:
        """Delete the snapshot ``name`` and its upper layer."""
        await self._call("virtx_client_remove_snapshot", _args(name=name))

    async def list_volume(self) -> list[VolumeListOutputItem]:
        """Every volume the server holds."""
        items = await self._call("virtx_client_list_volume")
        return [VolumeListOutputItem.from_dict(item) for item in items]

    async def create_volume(self, name: str | None = None) -> VolumeCreateOutput:
        """Keep a new, empty volume under ``name``; without one the server picks it."""
        output = await self._call("virtx_client_create_volume", _args(name=name))
        return VolumeCreateOutput.from_dict(output)

    async def remove_volume(self, name: str) -> None:
        """Delete the volume ``name`` and everything in it."""
        await self._call("virtx_client_remove_volume", _args(name=name))

    async def list_volume_mount(self, volume: str) -> list[VolumeMountListOutputItem]:
        """The mounts in ``volume``, without their credentials."""
        items = await self._call("virtx_client_list_volume_mount", _args(volume=volume))
        return [VolumeMountListOutputItem.from_dict(item) for item in items]

    async def create_volume_mount(self, volume: str, path: str, source: MountSource) -> None:
        """Mount ``source`` at ``path`` in ``volume``, a directory the mount creates."""
        await self._call(
            "virtx_client_create_volume_mount",
            _args(volume=volume, path=path, source=source.to_dict()),
        )

    async def remove_volume_mount(self, volume: str, path: str) -> None:
        """Take away the mount at ``path`` in ``volume``, leaving its source as it is."""
        await self._call("virtx_client_remove_volume_mount", _args(volume=volume, path=path))

    async def list_sandbox(self) -> list[dict[str, Any]]:
        """Every sandbox the server holds, as ``{"id", "image", "snapshot", "running", "daemon",
        "ports"}``, ``snapshot`` only for one that has one and ``daemon`` and ``ports`` only for a
        daemon."""
        return await self._call("virtx_client_list_sandbox")

    async def init_sandbox(
        self,
        image: str | ImageName,
        *,
        snapshot: str | None = None,
        network: bool = True,
        daemon: bool = False,
        ports: Iterable[int] = (),
        vcpus: int | None = None,
        memory_mib: int | None = None,
        gpu: bool | None = None,
        gpu_memory_mib: int | None = None,
        volumes: Iterable[Mapping[str, Any]] = (),
    ) -> dict[str, Any]:
        """Make a sandbox of ``image``. ``ports`` are TCP ports in it to open, only for a
        ``daemon``; ``volumes`` are ``{"volume", "path", "readonly"}``. Whatever is left out the
        server picks. Answers ``{"id", "ports"}``, ``ports``, each ``{"port", "url"}``,
        only if it was given some."""
        return await self._call(
            "virtx_client_init_sandbox",
            _args(
                image=image_name(image).to_dict(),
                snapshot=snapshot,
                network=network,
                daemon=daemon,
                ports=list(ports),
                vcpus=vcpus,
                memory_mib=memory_mib,
                gpu=gpu,
                gpu_memory_mib=gpu_memory_mib,
                volumes=list(volumes),
            ),
        )

    def exec_sandbox(
        self, sandbox: str, cmd: Iterable[str], timeout_ms: int | None = None
    ) -> AsyncIterator[Event[dict[str, Any], str]]:
        """Run ``cmd`` in ``sandbox`` to its end, killed after ``timeout_ms`` if one is given. Its
        events are its output, a line as it is written, then how it ended, ``{"code"}``."""
        return self._events(
            "virtx_client_exec_sandbox",
            _args(sandbox=sandbox, cmd=list(cmd), timeout_ms=timeout_ms),
            dict,
        )

    async def close_sandbox(self, sandbox: str) -> None:
        """Stop ``sandbox`` and forget its id, keeping its snapshot and volumes."""
        await self._call("virtx_client_close_sandbox", _args(sandbox=sandbox))
