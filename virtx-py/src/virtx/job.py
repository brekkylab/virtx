"""What every job's log shares: each entry numbered, and the mark of a log cut short.

A job, an image's build, pull or push or a process in a sandbox, runs on the server apart from
the request that started it. Its log is read with the client's ``*_job_log``, from its start or
after an entry's ``id``, until its end entry. A request that starts one answers a
``StartedJob``: awaited, the job; iterated, its log; ``finished()``, the job once it has ended.
"""

from __future__ import annotations

import asyncio
from collections.abc import AsyncGenerator, AsyncIterator, Awaitable, Callable, Generator
from dataclasses import dataclass
from typing import Any, Generic, TypeVar

from ._library import VirtxError

J = TypeVar("J")
E = TypeVar("E")


@dataclass(frozen=True)
class LogEntry(Generic[E]):
    """One entry of a job's log, and its number in it, ``1`` for the first: what ``after``
    resumes after."""

    id: int
    entry: E


@dataclass(frozen=True)
class Truncated:
    """The log reached the server's limit: nothing after it is kept or sent, but the end."""


class StartedJob(Generic[J, E]):
    """A request that starts a job, sent the first time this is awaited, iterated or finished,
    and only then: awaited, it answers the job; iterated, it yields the job's log, each a
    ``LogEntry``, until its end entry; ``finished()``, the job once it has ended. Each way reads
    the one job it started. Closing the iteration leaves the job running; cancelling it is the
    client's ``remove_*_job``."""

    def __init__(
        self,
        start: Callable[[], Awaitable[J]],
        log: Callable[[J], AsyncIterator[LogEntry[E]]],
        ended: Callable[[J, E], J | None],
    ) -> None:
        self._start = start
        self._log = log
        self._ended = ended
        self._job: asyncio.Future[J] | None = None

    def __await__(self) -> Generator[Any, None, J]:
        if self._job is None:
            self._job = asyncio.ensure_future(self._start())
        return self._job.__await__()

    async def __aiter__(self) -> AsyncIterator[LogEntry[E]]:
        log = self._log(await self)
        try:
            async for entry in log:
                yield entry
        finally:
            if isinstance(log, AsyncGenerator):
                await log.aclose()

    async def finished(self) -> J:
        """The job once it has ended, its log read to its end entry: an image job with its
        ``digest``, or, for one that failed, its ``error`` raised as a ``VirtxError``; a sandbox
        job with its exit ``code``, which is not an error whatever it is. A job removed while it
        is waited on raises a ``VirtxError`` too: its log ends with no end entry. Its log is still
        read with the job's ``log``."""
        job = await self
        log = self._log(job)
        try:
            async for entry in log:
                ended = self._ended(job, entry.entry)
                if ended is not None:
                    return ended
        finally:
            if isinstance(log, AsyncGenerator):
                await log.aclose()
        raise VirtxError(
            "the job's log ended without its end entry: the job was removed, or the server closed its log"
        )
