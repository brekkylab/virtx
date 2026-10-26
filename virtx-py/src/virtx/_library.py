"""``libvirtx_core``, opened with ctypes, and its requests driven from asyncio.

One library per process: every client is made by the one first opened, which stays open until
the process ends. A client handed to another module is only good in the same library, so a
module that shares one opens the path ``library_path()`` answers.

A request is polled with a waker the library wakes from its own threads: the wake schedules
the asyncio future the poll is waiting on, on that future's loop, and the next poll takes what
woke it.
"""

from __future__ import annotations

import asyncio
import ctypes
import itertools
import json
import os
import sys
import threading
from collections.abc import AsyncIterator, Callable
from pathlib import Path
from typing import Any

OK, ERROR, PENDING, END = 0, -1, 1, 2

#: The variable naming the library to open when ``load`` was not called first.
LIBRARY_ENV = "VIRTX_CORE_LIB"

if sys.platform == "win32":
    FILE_NAME = "virtx_core.dll"
elif sys.platform == "darwin":
    FILE_NAME = "libvirtx_core.dylib"
else:
    FILE_NAME = "libvirtx_core.so"


class VirtxError(Exception):
    """A request failed: the server's error, the transport's, or the library's."""


class _Bytes(ctypes.Structure):
    _fields_ = [("data", ctypes.c_void_p), ("len", ctypes.c_size_t)]


_CALLBACK = ctypes.CFUNCTYPE(None, ctypes.c_void_p)


class _Waker(ctypes.Structure):
    _fields_ = [("data", ctypes.c_void_p), ("wake", _CALLBACK), ("release", _CALLBACK)]


# The pending polls, by the key their waker carries as its `data`: the loop and the future to
# settle on it.
_pending: dict[int, tuple[asyncio.AbstractEventLoop, asyncio.Future[None]]] = {}
_keys = itertools.count(1)


def _settle(future: asyncio.Future[None]) -> None:
    if not future.done():
        future.set_result(None)


@_CALLBACK
def _wake(key: int | None) -> None:
    entry = _pending.get(key or 0)
    if entry is None:
        return
    loop, future = entry
    try:
        loop.call_soon_threadsafe(_settle, future)
    except RuntimeError:
        # The loop has closed; nothing is waiting on it.
        pass


@_CALLBACK
def _release(key: int | None) -> None:
    _pending.pop(key or 0, None)


_HANDLE = ctypes.c_void_p
_OUT = ctypes.POINTER(_Bytes)

# Each function's argument and result types, as `virtx.h` declares them. A request takes the
# client and, unless it is a list, its arguments as `(bytes, len)`.
_PLAIN = ([_HANDLE], _HANDLE)
_WITH_ARGS = ([_HANDLE, ctypes.c_char_p, ctypes.c_size_t], _HANDLE)
_SIGNATURES: dict[str, tuple[list[Any], Any]] = {
    "virtx_client_spawn": (
        [
            ctypes.c_char_p,
            ctypes.POINTER(ctypes.c_char_p),
            ctypes.c_size_t,
            ctypes.POINTER(_HANDLE),
            _OUT,
        ],
        ctypes.c_int,
    ),
    "virtx_client_connect": ([ctypes.c_char_p, ctypes.POINTER(_HANDLE), _OUT], ctypes.c_int),
    "virtx_client_retain": ([_HANDLE], None),
    "virtx_client_release": ([_HANDLE], None),
    "virtx_client_build_image": _WITH_ARGS,
    "virtx_client_list_snapshot": _PLAIN,
    "virtx_client_create_snapshot": _WITH_ARGS,
    "virtx_client_remove_snapshot": _WITH_ARGS,
    "virtx_client_list_volume": _PLAIN,
    "virtx_client_create_volume": _WITH_ARGS,
    "virtx_client_remove_volume": _WITH_ARGS,
    "virtx_client_list_volume_mount": _WITH_ARGS,
    "virtx_client_create_volume_mount": _WITH_ARGS,
    "virtx_client_remove_volume_mount": _WITH_ARGS,
    "virtx_client_list_sandbox": _PLAIN,
    "virtx_client_init_sandbox": _WITH_ARGS,
    "virtx_client_exec_sandbox": _WITH_ARGS,
    "virtx_client_close_sandbox": _WITH_ARGS,
    "virtx_bytes_free": ([_Bytes], None),
    "virtx_future_poll": ([_HANDLE, _Waker, _OUT], ctypes.c_int),
    "virtx_future_free": ([_HANDLE], None),
    "virtx_stream_poll_next": ([_HANDLE, _Waker, _OUT], ctypes.c_int),
    "virtx_stream_free": ([_HANDLE], None),
}


class Library:
    """The functions of ``virtx.h``, found in the library at ``path``."""

    def __init__(self, path: str) -> None:
        self.path = path
        # `CDLL` lets go of the GIL for each call, so a call that blocks (opening a client, or
        # releasing the last hold) leaves the waker's callbacks free to take it.
        self._dll = ctypes.CDLL(path)
        for name, (argtypes, restype) in _SIGNATURES.items():
            function = getattr(self._dll, name)
            function.argtypes = argtypes
            function.restype = restype
            setattr(self, name, function)

    def take(self, status: int, out: _Bytes) -> Any:
        """What the library wrote for ``status``, read and freed: the output, or raised."""
        data = ctypes.string_at(out.data, out.len) if out.data else b""
        if out.data:
            self.virtx_bytes_free(out)
        if status == OK:
            return json.loads(data)
        if status == ERROR:
            raise VirtxError(json.loads(data) if data else "the call failed")
        raise VirtxError(f"unknown status {status}")

    def _poll(
        self, poll: Callable[..., int], handle: int
    ) -> tuple[int, _Bytes, asyncio.Future[None]]:
        loop = asyncio.get_running_loop()
        future = loop.create_future()
        key = next(_keys)
        _pending[key] = (loop, future)
        out = _Bytes()
        status = poll(handle, _Waker(key, _wake, _release), ctypes.byref(out))
        return status, out, future

    async def answer(self, handle: int) -> Any:
        """Await the future ``handle``, freeing it after, or on cancelling, which cancels it."""
        try:
            while True:
                status, out, woken = self._poll(self.virtx_future_poll, handle)
                if status != PENDING:
                    return self.take(status, out)
                await woken
        finally:
            self.virtx_future_free(handle)

    async def items(self, handle: int) -> AsyncIterator[Any]:
        """Each item of the stream ``handle``, freeing it after, or on closing, which cancels
        it."""
        try:
            while True:
                status, out, woken = self._poll(self.virtx_stream_poll_next, handle)
                if status == END:
                    return
                if status != PENDING:
                    yield self.take(status, out)
                    continue
                await woken
        finally:
            self.virtx_stream_free(handle)


_library: Library | None = None
_lock = threading.Lock()


def load(path: str | os.PathLike[str]) -> None:
    """Open the library at ``path`` for this process's clients. Opening the one already open does
    nothing; another is refused, since a process has one."""
    global _library
    path = os.fspath(path)
    with _lock:
        if _library is None:
            try:
                _library = Library(path)
            except OSError as e:
                raise VirtxError(f"opening {path}: {e}") from e
        elif _library.path != path:
            raise VirtxError(f"{_library.path} is open already, so {path} is not opened")


def library_path() -> str | None:
    """The path the open library was opened by; ``None`` before the first client or ``load``."""
    return _library.path if _library is not None else None


def library() -> Library:
    """The open library, opening it first if none is: ``$VIRTX_CORE_LIB``, else the one in this
    package, else the platform's file name for the system's loader to look up."""
    if _library is None:
        bundled = Path(__file__).with_name(FILE_NAME)
        path = os.environ.get(LIBRARY_ENV) or (str(bundled) if bundled.exists() else FILE_NAME)
        load(path)
    assert _library is not None
    return _library
