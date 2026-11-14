"""The client: a ``VirtxClient*`` held, and each request of ``virtx.h`` as a method."""

from __future__ import annotations

import asyncio
import atexit
import ctypes
import json
import os
import weakref
from collections.abc import AsyncIterator, Callable, Iterable, Mapping
from dataclasses import dataclass
from typing import TYPE_CHECKING, Any, Generic, Literal, TypeVar

from ._library import OK, Library, VirtxError, _Bytes, library
from ._library import args as _args
from .image import (
    BearerAuth,
    ImageDownloadOutput,
    ImageJob,
    ImageJobEntry,
    ImageListOutputItem,
    ImageName,
    ImagePruneOutput,
    ImageUploadOutput,
    Recipe,
    RegistryAuth,
    image_name,
)
from .job import StartedJob
from .sandbox import (
    InboundRule,
    PortInbound,
    PortProtocol,
    PortSpec,
    Sandbox,
    SandboxPort,
    SandboxVolume,
    inbound_rules,
)
from .volume import Volume

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


def _event(item: Mapping[str, Any], output: Callable[[Any], T]) -> Event[T, Any]:
    if "progress" in item:
        progress = item["progress"]
        return Progress(progress["progress"], progress.get("total"), progress.get("content"))
    return Output(output(item["output"]))


# Every client not yet closed, closed at exit: a hold left to the interpreter's teardown would
# end its session after the waker's callbacks can still run.
def _header_array(headers: Mapping[str, str] | None) -> ctypes.Array[ctypes.c_char_p]:
    """``headers`` as ``virtx_client_connect`` takes them: each name then its value."""
    strings = [s.encode() for name, value in (headers or {}).items() for s in (name, value)]
    return (ctypes.c_char_p * len(strings))(*strings)


@dataclass(frozen=True)
class CommandAuth:
    """The token ``program`` with ``args`` prints, sent as ``Authorization: Bearer <token>``:
    a token alone, or ``{"token": ..., "expires_in": <seconds>}``. It is run for the first
    request, and again when its token is a minute from running out or the server refuses it:
    a person's short-lived token, as ``virtx token --json`` prints it."""

    program: str
    args: tuple[str, ...] = ()

    def to_dict(self) -> dict[str, Any]:
        """As the wire has it."""
        return {"type": "command", "program": self.program, "args": list(self.args)}


Auth = BearerAuth | CommandAuth


def _auth_bytes(auth: Auth | None) -> bytes:
    """``auth`` as ``virtx_client_connect`` takes it: JSON, or nothing for none."""
    return b"" if auth is None else json.dumps(auth.to_dict()).encode()


_open: weakref.WeakSet[Client] = weakref.WeakSet()


@atexit.register
def _close_all() -> None:
    for client in list(_open):
        client.close()


async def ensure_virtx() -> str:
    """Fetch ``virtx-stdio``, the server of the library's version, into this host's cache if it
    is not there, and give its path: what ``Client.spawn()`` without a program starts. The
    fetch runs in a thread, off the event loop.

    The server is kept one directory per version, so a library that is upgraded fetches its
    own. ``$VIRTX_STDIO`` names a server to use as it is; ``$VIRTX_STDIO_VERSION`` another
    version; ``$VIRTX_DIST_URL`` where releases are fetched from; ``$VIRTX_HOME`` the cache.
    """
    lib = library()

    def fetch() -> str:
        out = _Bytes()
        status = lib.virtx_ensure_virtx(ctypes.byref(out))
        return lib.take(status, out)

    return await asyncio.to_thread(fetch)


class Client:
    """A client of one server, over stdio or HTTP, made by ``libvirtx_core``.

    Each method is a coroutine, or an async iterator for a request that streams: a job's log,
    or the progress of a download or upload. One that starts a job answers a ``StartedJob``,
    both.
    Holds of one session are counted: ``clone`` takes another, ``close`` lets this one go, and
    the session ends with the last, which answers every request still waiting with an error.
    """

    def __init__(self, raw: int, lib: Library) -> None:
        self._raw: int | None = raw
        self._lib = lib
        _open.add(self)

    @classmethod
    def spawn(cls, program: str | None = None, args: Iterable[str] = ()) -> Client:
        """Start ``program`` with ``args`` as the server, and talk to it over its stdin and
        stdout. Blocks until it has started. Without ``program``, the server is ``virtx-stdio``
        of this library's version, fetched first if this host has none -- blocking for the
        fetch; ``ensure_virtx`` does that fetch alone, and off the event loop."""
        lib = library()
        argv = [arg.encode() for arg in args]
        array = (ctypes.c_char_p * len(argv))(*argv)
        raw = ctypes.c_void_p()
        error = _Bytes()
        status = lib.virtx_client_spawn(
            None if program is None else program.encode(),
            array,
            len(argv),
            ctypes.byref(raw),
            ctypes.byref(error),
        )
        return cls._opened(lib, status, raw, error)

    @classmethod
    def connect(
        cls,
        url: str,
        auth: Auth | None = None,
        headers: Mapping[str, str] | None = None,
    ) -> Client:
        """Reach the server at ``url``, its base ``http`` or ``https`` URL, sending ``auth``, the credentials it wants, and ``headers``, whatever else it asks
        of each request, with every request. ``Authorization`` is ``auth``'s, over one given as
        a header. Refused when a token would go in the clear: over ``http`` to any host but
        this machine."""
        lib = library()
        auth_json = _auth_bytes(auth)
        array = _header_array(headers)
        raw = ctypes.c_void_p()
        error = _Bytes()
        status = lib.virtx_client_connect(
            url.encode(),
            auth_json,
            len(auth_json),
            array,
            len(array) // 2,
            ctypes.byref(raw),
            ctypes.byref(error),
        )
        return cls._opened(lib, status, raw, error)

    def set_auth(self, auth: Auth | None) -> None:
        """Send ``auth`` with every request from now on, in place of what was given at
        ``connect``: a new token, when the server's are short-lived. A forwarder already open
        sends it with its next connection. Over stdio, where none is sent, nothing changes.
        Refused as at ``connect``, the auth unchanged."""
        auth_json = _auth_bytes(auth)
        error = _Bytes()
        status = self._lib.virtx_client_set_auth(
            self._held(), auth_json, len(auth_json), ctypes.byref(error)
        )
        if status != OK:
            self._lib.take(status, error)

    def set_headers(self, headers: Mapping[str, str]) -> None:
        """Send ``headers`` with every request from now on, in place of those given at
        ``connect``. A forwarder already open sends them with its next connection. Over stdio,
        where none are sent, nothing changes. A header that cannot be sent raises, the headers
        unchanged."""
        array = _header_array(headers)
        error = _Bytes()
        status = self._lib.virtx_client_set_headers(
            self._held(), array, len(array) // 2, ctypes.byref(error)
        )
        if status != OK:
            self._lib.take(status, error)

    @classmethod
    def from_env(cls) -> Client:
        """Reach the server ``VIRTX_URL`` names, with ``VIRTX_API_KEY`` and ``VIRTX_ORG`` if set:
        with ``VIRTX_ORG`` and no key, as the person signed in with ``virtx login``, whose token
        ``virtx token --json`` prints, again as it runs out.

        Without ``VIRTX_URL``, virtx cloud under ``VIRTX_DOMAIN`` (``DOMAIN`` by default): the
        cell ``VIRTX_REGION`` names (``apne2``), or else ``run.<domain>``, every cell's name,
        which DNS answers with the cell nearest this machine. A sandbox of another cell is
        reached through it as well."""
        key = os.environ.get("VIRTX_API_KEY") or None
        org = os.environ.get("VIRTX_ORG") or None
        domain = os.environ.get("VIRTX_DOMAIN") or None
        region = os.environ.get("VIRTX_REGION") or None
        url = os.environ.get("VIRTX_URL") or None
        if not url and region:
            url = f"https://{region}.api.{domain or DOMAIN}"
        if not url and (key or org):
            url = f"https://run.{domain or DOMAIN}"
        if not url:
            raise VirtxError("VIRTX_URL is not set, and no cell of virtx cloud was found")
        auth: Auth | None = None
        if key:
            auth = BearerAuth(key)
        elif org:
            auth = CommandAuth(*SIGNED_IN)
        return cls.connect(url, auth, {ORG_HEADER: org} if org else None)

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

    async def _call(self, name: str, args: bytes | None = None, *, raw: bool = False) -> Any:
        start = getattr(self._lib, name)
        held = self._held()
        handle = start(held) if args is None else start(held, args, len(args))
        return await self._lib.answer(handle, raw)

    def _items(self, name: str, args: bytes) -> AsyncIterator[Any]:
        handle = getattr(self._lib, name)(self._held(), args, len(args))
        return self._lib.items(handle)

    async def _events(
        self, name: str, args: bytes, output: Callable[[Any], T]
    ) -> AsyncIterator[Event[T, Any]]:
        async for item in self._items(name, args):
            yield _event(item, output)

    def _image_job(self, name: str, args: bytes) -> StartedJob[ImageJob, ImageJobEntry]:
        async def start() -> ImageJob:
            return ImageJob.from_dict(self, await self._call(name, args))

        return StartedJob(start, lambda job: job.log())

    def build_image(
        self, recipe: Recipe | Mapping[str, Any], reference: str
    ) -> StartedJob[ImageJob, ImageJobEntry]:
        """Build ``recipe``, to point ``reference`` at what it builds: awaited, the job;
        iterated, its log, as ``ImageJob.log``'s. Sent when first awaited or iterated."""
        recipe = recipe.to_dict() if isinstance(recipe, Recipe) else recipe
        return self._image_job(
            "virtx_client_build_image", _args(recipe=recipe, reference=reference)
        )

    def commit_image(
        self, sandbox: str | Sandbox, reference: str
    ) -> StartedJob[ImageJob, ImageJobEntry]:
        """Make an image of ``sandbox``, a ``Sandbox`` or its id -- its image, with what it has
        written as one more layer -- to point ``reference`` at: how a sandbox is kept to start
        others from. Of a running one, as it is when the job starts; of a stopped one, as it was
        left. Awaited, the job; iterated, its log, as ``ImageJob.log``'s. Sent when first
        awaited or iterated."""
        sandbox = sandbox.id if isinstance(sandbox, Sandbox) else sandbox
        return self._image_job(
            "virtx_client_commit_image", _args(sandbox=sandbox, reference=reference)
        )

    def pull_image(
        self, reference: str, auth: RegistryAuth | None = None
    ) -> StartedJob[ImageJob, ImageJobEntry]:
        """Pull ``reference`` from its registry, to point it at what it pulls; without ``auth``
        the server pulls anonymously. Awaited, the job; iterated, its log, as
        ``ImageJob.log``'s. Sent when first awaited or iterated."""
        return self._image_job(
            "virtx_client_pull_image", _args(reference=reference, auth=auth and auth.to_dict())
        )

    def push_image(
        self, reference: str, auth: RegistryAuth | None = None
    ) -> StartedJob[ImageJob, ImageJobEntry]:
        """Push the image ``reference`` points at to the registry ``reference`` names; without
        ``auth`` the server pushes anonymously. Awaited, the job; iterated, its log, as
        ``ImageJob.log``'s. Sent when first awaited or iterated."""
        return self._image_job(
            "virtx_client_push_image", _args(reference=reference, auth=auth and auth.to_dict())
        )

    async def list_image_job(self) -> list[ImageJob]:
        """Every image job the server holds."""
        items = await self._call("virtx_client_list_image_job")
        return [ImageJob.from_dict(self, item) for item in items]

    async def get_image_job(self, id: str) -> ImageJob:
        """The image job ``id``."""
        output = await self._call("virtx_client_get_image_job", _args(id=id))
        return ImageJob.from_dict(self, output)

    async def list_image(self) -> list[ImageListOutputItem]:
        """Every tag on the server's images, as ``name:tag``, with the digest it points at."""
        items = await self._call("virtx_client_list_image")
        return [ImageListOutputItem.from_dict(item) for item in items]

    async def tag_image(self, source: str, target: str) -> None:
        """Point the tag ``target`` (``name:tag``) at the image ``source`` names (``name:tag`` or
        ``name@algorithm:hex``), moving it if it is in use."""
        await self._call("virtx_client_tag_image", _args(source=source, target=target))

    async def untag_image(self, reference: str) -> None:
        """Remove the tag ``reference`` (``name:tag``), leaving the image it points at."""
        await self._call("virtx_client_untag_image", _args(reference=reference))

    async def remove_image(self, reference: str) -> None:
        """Remove the image ``reference`` names (``name:tag`` or ``name@algorithm:hex``), and
        every tag on it in that name."""
        await self._call("virtx_client_remove_image", _args(reference=reference))

    def download_image(
        self, reference: str, path: str | os.PathLike[str]
    ) -> AsyncIterator[Event[ImageDownloadOutput, str]]:
        """Write the image ``reference`` names to ``path``, as an OCI image layout in a tar
        archive, replacing what is there once all of it is written. Its events are a line of log
        a blob, then what it wrote."""
        return self._events(
            "virtx_client_download_image",
            _args(reference=reference, path=os.fspath(path)),
            ImageDownloadOutput.from_dict,
        )

    def upload_image(
        self, path: str | os.PathLike[str], reference: str
    ) -> AsyncIterator[Event[ImageUploadOutput, str]]:
        """Upload the image in the OCI image layout archive at ``path``, and point the tag
        ``reference`` (``name:tag``) at it: the archive's only image, or the one it names by that
        tag. Its events are a line of log a blob, then what it uploaded."""
        return self._events(
            "virtx_client_upload_image",
            _args(path=os.fspath(path), reference=reference),
            ImageUploadOutput.from_dict,
        )

    async def prune_image(self) -> ImagePruneOutput:
        """Delete each image no tag points at and no sandbox uses, and what only such images
        held."""
        return ImagePruneOutput.from_dict(await self._call("virtx_client_prune_image"))

    async def list_volume(self) -> list[Volume]:
        """Every volume the server holds."""
        items = await self._call("virtx_client_list_volume")
        return [Volume.from_dict(self, item) for item in items]

    async def create_volume(self, name: str | None = None) -> Volume:
        """Keep a new, empty volume under ``name``; without one the server picks it."""
        output = await self._call("virtx_client_create_volume", _args(name=name))
        return Volume.from_dict(self, output)

    async def get_volume(self, name: str) -> Volume:
        """The volume ``name``."""
        output = await self._call("virtx_client_get_volume", _args(name=name))
        return Volume.from_dict(self, output)

    async def list_sandbox(self) -> list[Sandbox]:
        """Every sandbox the server holds, without its ports."""
        items = await self._call("virtx_client_list_sandbox")
        return [Sandbox.from_dict(self, item) for item in items]

    async def create_sandbox(
        self,
        image: str | ImageName,
        *,
        name: str | None = None,
        ephemeral: bool = False,
        auto_stop_minutes: int | None = None,
        auto_delete_days: int | Literal["never"] | None = None,
        disk_gib: int | None = None,
        network: bool = True,
        ports: Iterable[int | PortSpec | Mapping[str, Any]] = (),
        vcpus: int | None = None,
        memory_mib: int | None = None,
        gpu: bool | None = None,
        gpu_memory_mib: int | None = None,
        volumes: Iterable[SandboxVolume | Mapping[str, Any]] = (),
    ) -> Sandbox:
        """Make a sandbox of ``image``, running, with ``volumes`` mounted into it, each a
        ``SandboxVolume``. It is kept when it stops -- after ``auto_stop_minutes`` unused, or at
        ``Sandbox.stop`` -- and starts again as it was left, until it is deleted: at
        ``Sandbox.remove``, when it stops if ``ephemeral``, or after ``auto_delete_days`` stopped
        and unused, ``"never"`` keeping it until then. ``name`` finds it again with
        ``find_sandbox``. ``ports`` are ports in it to open -- a number for TCP, or a
        ``PortSpec`` (or its ``{port, label?, protocol?, inbound?}``) to say what it is for,
        whether it is HTTP and whom it lets in -- each answered here and by ``Sandbox.start``
        with the ``url`` it is reached by.
        Whatever is left out the server picks.

        ``image`` has to be on the server already, pulled or built: this does not pull it, and a
        tag the server has no image by is refused with ``404``."""
        args = json.loads(
            _args(
                image=image_name(image).to_dict(),
                name=name,
                ephemeral=ephemeral or None,
                auto_stop_minutes=auto_stop_minutes,
                auto_delete_days=None if auto_delete_days == "never" else auto_delete_days,
                disk_gib=disk_gib,
                network=network,
                ports=[p.to_dict() if isinstance(p, PortSpec) else p for p in ports],
                vcpus=vcpus,
                memory_mib=memory_mib,
                gpu=gpu,
                gpu_memory_mib=gpu_memory_mib,
                volumes=[v.to_dict() if isinstance(v, SandboxVolume) else dict(v) for v in volumes],
            )
        )
        if auto_delete_days == "never":
            args["auto_delete_days"] = None
        return Sandbox.from_dict(
            self, await self._call("virtx_client_create_sandbox", json.dumps(args).encode())
        )

    async def get_sandbox(self, id: str) -> Sandbox:
        """The sandbox ``id``, running or stopped, without its ports."""
        return Sandbox.from_dict(self, await self._call("virtx_client_get_sandbox", _args(id=id)))

    async def find_sandbox(self, name: str) -> Sandbox | None:
        """The sandbox named ``name``, without its ports, if one is."""
        found = await self._call("virtx_client_find_sandbox", _args(name=name))
        return None if found is None else Sandbox.from_dict(self, found)

    async def start_sandbox(self, id: str) -> Sandbox:
        """Start the sandbox ``id`` if it is stopped -- its VM booted over its disk as it was
        left -- and answer it once it runs, with where its ports are now. A job started in a
        stopped sandbox starts it too."""
        return Sandbox.from_dict(self, await self._call("virtx_client_start_sandbox", _args(id=id)))

    async def stop_sandbox(self, id: str) -> Sandbox:
        """Stop the sandbox ``id`` if it runs: its jobs end, and its disk is kept -- or, for an
        ephemeral one, deleted with it. Answered once it has stopped."""
        return Sandbox.from_dict(self, await self._call("virtx_client_stop_sandbox", _args(id=id)))

    async def open_sandbox_port(
        self,
        id: str,
        port: int,
        *,
        label: str | None = None,
        protocol: PortProtocol | str | None = None,
        inbound: Iterable[InboundRule | Mapping[str, Any]] | None = None,
    ) -> SandboxPort:
        """Open the port ``port`` in the sandbox ``id``, as those it was made with are, and
        answer where it is reached from: TCP unless ``protocol`` says HTTP, ``label`` saying
        what it is for, ``inbound`` its inbound rules, virtx cloud's. Opened already, it is
        answered as it is, ``label`` renaming it. A program in it listening on ``port`` already
        is reached once it listens again."""
        args = _args(
            id=id,
            port=port,
            label=label,
            protocol=None if protocol is None else PortProtocol(protocol).value,
            inbound=None if inbound is None else inbound_rules(inbound),
        )
        return SandboxPort.from_dict(await self._call("virtx_client_open_sandbox_port", args))

    async def get_sandbox_port_inbound(self, id: str, port: int) -> PortInbound:
        """The inbound rules of the port ``port`` in the sandbox ``id``, and its public address
        if it has one: virtx cloud's, which a self-hosted server does not answer. Its desktop's
        port is refused ``409``, ``not_public``; one not open, ``404``."""
        args = _args(id=id, port=port)
        return PortInbound.from_dict(
            await self._call("virtx_client_get_sandbox_port_inbound", args)
        )

    async def set_sandbox_port_inbound(
        self, id: str, port: int, rules: Iterable[InboundRule | Mapping[str, Any]]
    ) -> PortInbound:
        """Make the inbound rules of the port ``port`` in the sandbox ``id`` ``rules``, all of
        them -- ``[]`` lets in no one -- and answer them as the server keeps them. A source that
        is not an address or a CIDR is refused ``400``, ``invalid``."""
        args = _args(id=id, port=port, rules=inbound_rules(rules))
        return PortInbound.from_dict(
            await self._call("virtx_client_set_sandbox_port_inbound", args)
        )

    async def close_sandbox_port(self, id: str, port: int) -> None:
        """Close the port ``port`` in the sandbox ``id``: its ``url`` reaches it no more, and it
        is not opened again as the sandbox starts. One not open is refused ``404``."""
        await self._call("virtx_client_close_sandbox_port", _args(id=id, port=port))


DOMAIN = "virtx.sh"
"""Where virtx cloud is, by default: ``run.<DOMAIN>``, every cell's name, and
``<cell>.api.<DOMAIN>`` for one."""

ORG_HEADER = "virtx-org"
"""The header a request names its organization in, on a server that keeps organizations apart
(PROTOCOL.md, Authentication)."""

SIGNED_IN = ("virtx", ("token", "--json"))
"""The command that prints the token of the person signed in with ``virtx login``, and its
arguments."""
