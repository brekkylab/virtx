"""The client: a ``VirtxClient*`` held, and each request of ``virtx.h`` as a method."""

from __future__ import annotations

import asyncio
import atexit
import ctypes
import re
import weakref
from collections.abc import AsyncIterator, Iterable, Mapping
from dataclasses import dataclass
from typing import Any, Generic, TypeVar

import cbor2

from ._library import OK, Library, VirtxError, _Bytes, library

T = TypeVar("T")


@dataclass(frozen=True)
class Progress:
    """A line of a request's log: a build's, or a command's output."""

    message: str


@dataclass(frozen=True)
class Output(Generic[T]):
    """What a request answered, after its progress."""

    value: T


Event = Progress | Output[T]


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


_DIGEST = re.compile(r"(sha256:[0-9a-f]{64}|sha512:[0-9a-f]{128})")


def image_name(image: str | Mapping[str, Any]) -> Mapping[str, Any]:
    """``image`` as the wire names one: a full ``sha256:`` or ``sha512:`` digest is a digest, any
    other string a reference. A mapping is taken as already named."""
    if not isinstance(image, str):
        return image
    if _DIGEST.fullmatch(image):
        return {"type": "digest", "digest": image}
    return {"type": "reference", "reference": image}


def _args(**args: Any) -> bytes:
    """``args`` without the ones left out, as CBOR."""
    return cbor2.dumps({key: value for key, value in args.items() if value is not None})


def _event(item: Mapping[str, Any]) -> Event[Any]:
    if "progress" in item:
        return Progress(item["progress"])
    return Output(item["output"])


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
        """Reach the server at ``url``, an ``http`` or ``https`` URL."""
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

    def __enter__(self) -> Client:
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()

    async def __aenter__(self) -> Client:
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

    async def _events(self, name: str, args: bytes) -> AsyncIterator[Event[Any]]:
        handle = getattr(self._lib, name)(self._held(), args, len(args))
        async for item in self._lib.items(handle):
            yield _event(item)

    async def list_image(self) -> list[dict[str, Any]]:
        """Every reference the server holds, and the image each points at, as
        ``{"reference", "digest", "size"}``."""
        return await self._call("virtx_client_list_image")

    def pull_image(self, reference: str) -> AsyncIterator[Event[dict[str, Any]]]:
        """Pull ``reference`` from its registry, and point it at what it pulls. Its events are
        the pull's log, then what it pulled, ``{"reference", "digest"}``."""
        return self._events("virtx_client_pull_image", _args(reference=reference))

    def build_image(
        self, recipe: Recipe | Mapping[str, Any], reference: str | None = None
    ) -> AsyncIterator[Event[dict[str, Any]]]:
        """Build ``recipe``, and point ``reference`` at it; without one the server picks it. Its
        events are the build's log, then what it built, ``{"reference", "digest"}``."""
        recipe = recipe.to_dict() if isinstance(recipe, Recipe) else recipe
        return self._events("virtx_client_build_image", _args(recipe=recipe, reference=reference))

    async def remove_image(self, image: str | Mapping[str, Any]) -> None:
        """Forget ``image``, a reference or a digest."""
        await self._call("virtx_client_remove_image", _args(image=image_name(image)))

    async def list_snapshot(self) -> list[dict[str, Any]]:
        """Every snapshot the server holds, as ``{"name", "size"}``."""
        return await self._call("virtx_client_list_snapshot")

    async def create_snapshot(
        self, name: str | None = None, disk_gib: int | None = None
    ) -> dict[str, Any]:
        """Keep a new, empty upper layer under ``name``, holding at most ``disk_gib`` GiB;
        without either the server picks it. Answers ``{"name"}``."""
        return await self._call(
            "virtx_client_create_snapshot", _args(name=name, disk_gib=disk_gib)
        )

    async def remove_snapshot(self, name: str) -> None:
        """Delete the snapshot ``name`` and its upper layer."""
        await self._call("virtx_client_remove_snapshot", _args(name=name))

    async def read_snapshot_dir(self, snapshot: str, path: str) -> list[dict[str, Any]]:
        """The entries of the directory ``path`` in ``snapshot``'s upper layer, in no set order,
        as ``{"name", "metadata"}``; the image under it is not looked at."""
        return await self._call(
            "virtx_client_read_snapshot_dir", _args(snapshot=snapshot, path=path)
        )

    async def read_snapshot_file(
        self, snapshot: str, path: str, offset: int = 0, length: int | None = None
    ) -> bytes:
        """Up to ``length`` bytes of the file ``path`` in ``snapshot``'s upper layer, not the
        image under it, from ``offset``, or to the end without a ``length``. Fewer may come back before the end; none at or past it."""
        return await self._call(
            "virtx_client_read_snapshot_file",
            _args(snapshot=snapshot, path=path, offset=offset, length=length),
        )

    async def list_volume(self) -> list[dict[str, Any]]:
        """Every volume the server holds, as ``{"name", "size"}``."""
        return await self._call("virtx_client_list_volume")

    async def create_volume(self, name: str | None = None) -> dict[str, Any]:
        """Keep a new, empty volume under ``name``; without one the server picks it. Answers
        ``{"name"}``."""
        return await self._call("virtx_client_create_volume", _args(name=name))

    async def remove_volume(self, name: str) -> None:
        """Delete the volume ``name`` and everything in it."""
        await self._call("virtx_client_remove_volume", _args(name=name))

    async def stat_volume_file(self, volume: str, path: str) -> dict[str, Any]:
        """The metadata of ``path`` in ``volume``: ``{"type": "file" or "dir", "size",
        "modified"}``, ``modified`` in nanoseconds since the Unix epoch."""
        return await self._call("virtx_client_stat_volume_file", _args(volume=volume, path=path))

    async def read_volume_dir(self, volume: str, path: str) -> list[dict[str, Any]]:
        """The entries of the directory ``path`` in ``volume``, in no set order, as
        ``{"name", "metadata"}``."""
        return await self._call("virtx_client_read_volume_dir", _args(volume=volume, path=path))

    async def read_volume_file(
        self, volume: str, path: str, offset: int = 0, length: int | None = None
    ) -> bytes:
        """Up to ``length`` bytes of the file ``path`` in ``volume`` from ``offset``, or to the
        end without a ``length``. Fewer may come back before the end; none at or past it."""
        return await self._call(
            "virtx_client_read_volume_file",
            _args(volume=volume, path=path, offset=offset, length=length),
        )

    async def write_volume_file(
        self, volume: str, path: str, data: bytes, offset: int | None = None
    ) -> None:
        """Write ``data`` to the file ``path`` in ``volume`` at ``offset``, or make the file
        ``data`` and nothing else without one. The file is created if there is none."""
        await self._call(
            "virtx_client_write_volume_file",
            _args(volume=volume, path=path, offset=offset, data=bytes(data)),
        )

    async def create_volume_dir(self, volume: str, path: str, parents: bool = False) -> None:
        """Create the directory ``path`` in ``volume``, and with ``parents`` the ones above it
        too."""
        await self._call(
            "virtx_client_create_volume_dir", _args(volume=volume, path=path, parents=parents)
        )

    async def remove_volume_file(self, volume: str, path: str, recursive: bool = False) -> None:
        """Delete the file or directory ``path`` in ``volume``; a directory with anything in it
        only when ``recursive``."""
        await self._call(
            "virtx_client_remove_volume_file",
            _args(volume=volume, path=path, recursive=recursive),
        )

    async def rename_volume_file(
        self, volume: str, from_: str, to: str, overwrite: bool = False
    ) -> None:
        """Move ``from_`` to ``to`` in ``volume``, replacing what is at ``to`` only when
        ``overwrite``."""
        await self._call(
            "virtx_client_rename_volume_file",
            cbor2.dumps({"volume": volume, "from": from_, "to": to, "overwrite": overwrite}),
        )

    async def copy_volume_file(
        self, volume: str, from_: str, to: str, overwrite: bool = False
    ) -> None:
        """Copy ``from_``, a file or a whole directory, to ``to`` in ``volume``, deleting what is
        at ``to`` first only when ``overwrite``."""
        await self._call(
            "virtx_client_copy_volume_file",
            cbor2.dumps({"volume": volume, "from": from_, "to": to, "overwrite": overwrite}),
        )

    async def list_volume_mount(self, volume: str) -> list[dict[str, Any]]:
        """The mounts in ``volume``, without their credentials, as ``{"path", "type"}``."""
        return await self._call("virtx_client_list_volume_mount", _args(volume=volume))

    async def create_volume_mount(
        self, volume: str, path: str, source: Mapping[str, Any]
    ) -> None:
        """Mount ``source`` at ``path`` in ``volume``, a directory the mount creates. ``source``
        is ``{"type": "notion", "api_key"}``, ``{"type": "gdrive", "client_id",
        "client_secret", "refresh_token"}`` or ``{"type": "onedrive", "client_id",
        "refresh_token"}`` (and ``"client_secret"`` if it has one)."""
        await self._call(
            "virtx_client_create_volume_mount", _args(volume=volume, path=path, source=source)
        )

    async def remove_volume_mount(self, volume: str, path: str) -> None:
        """Take away the mount at ``path`` in ``volume``, leaving its source as it is."""
        await self._call("virtx_client_remove_volume_mount", _args(volume=volume, path=path))

    async def list_sandbox(self) -> list[dict[str, Any]]:
        """Every sandbox the server holds, as ``{"id", "image", "snapshot"}``, ``snapshot`` only
        for one that has one."""
        return await self._call("virtx_client_list_sandbox")

    async def init_sandbox(
        self,
        image: str | Mapping[str, Any],
        *,
        snapshot: str | None = None,
        network: bool = True,
        ports: Iterable[tuple[int, int] | Mapping[str, int]] = (),
        vcpus: int | None = None,
        memory_mib: int | None = None,
        gpu: bool | None = None,
        gpu_memory_mib: int | None = None,
        volumes: Iterable[Mapping[str, Any]] = (),
    ) -> dict[str, Any]:
        """Make a sandbox of ``image``. ``ports`` are ``(host, guest)``; ``volumes`` are
        ``{"volume", "path", "readonly"}``. Whatever is left out the server picks. Answers
        ``{"id", "cwd"}``, ``cwd`` only if the server says."""
        ports = [
            port if isinstance(port, Mapping) else {"host": port[0], "guest": port[1]}
            for port in ports
        ]
        return await self._call(
            "virtx_client_init_sandbox",
            _args(
                image=image_name(image),
                snapshot=snapshot,
                network=network,
                ports=ports,
                vcpus=vcpus,
                memory_mib=memory_mib,
                gpu=gpu,
                gpu_memory_mib=gpu_memory_mib,
                volumes=list(volumes),
            ),
        )

    def exec_sandbox(
        self, sandbox: str, cmd: Iterable[str], timeout_ms: int | None = None
    ) -> AsyncIterator[Event[dict[str, Any]]]:
        """Run ``cmd`` in ``sandbox`` to its end, killed after ``timeout_ms`` if one is given. Its
        events are its output, a line as it is written, then how it ended, ``{"code"}``."""
        return self._events(
            "virtx_client_exec_sandbox",
            _args(sandbox=sandbox, cmd=list(cmd), timeout_ms=timeout_ms),
        )

    async def close_sandbox(self, sandbox: str) -> None:
        """Stop ``sandbox`` and forget its id, keeping its snapshot and volumes."""
        await self._call("virtx_client_close_sandbox", _args(sandbox=sandbox))
