"""What every job's log shares: each entry numbered, and the mark of a log cut short.

A job, an image's build, pull, push or commit or a process in a sandbox, runs on the server
apart from the request that started it. Its log is read with the job's ``log``, from its start
or after an entry's ``id``, until its end entry. A request that starts one answers a
``StartedJob``: awaited, the job; iterated, its log.
"""

from __future__ import annotations

import asyncio
from collections.abc import AsyncGenerator, AsyncIterator, Awaitable, Callable, Generator
from dataclasses import dataclass
from typing import Any, Generic, TypeVar

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
    """A request that starts a job, sent the first time this is awaited or iterated, and only
    then: awaited, it answers the job; iterated, it yields the job's log, each a ``LogEntry``,
    until its end entry. Both ways read the one job it started. Closing the iteration leaves
    the job running; cancelling it is the client's ``remove_*_job``.

    Given ``run``, one iterated before it is awaited starts the job and reads its log as one:
    ``run`` yields the job first, then the log's entries."""

    def __init__(
        self,
        start: Callable[[], Awaitable[J]],
        log: Callable[[J], AsyncIterator[LogEntry[E]]],
        run: Callable[[], AsyncIterator[J | LogEntry[E]]] | None = None,
    ) -> None:
        self._start = start
        self._log = log
        self._run = run
        self._job: asyncio.Future[J] | None = None

    def __await__(self) -> Generator[Any, None, J]:
        if self._job is None:
            self._job = asyncio.ensure_future(self._start())
        return self._job.__await__()

    async def __aiter__(self) -> AsyncIterator[LogEntry[E]]:
        if self._job is None and self._run is not None:
            job: asyncio.Future[J] = asyncio.get_running_loop().create_future()
            self._job = job
            items = self._run()
            try:
                async for item in items:
                    if isinstance(item, LogEntry):
                        yield item
                    elif not job.done():
                        job.set_result(item)
            except BaseException as e:
                if not job.done():
                    job.set_exception(e)
                    # The loop's to raise, here: the job's own future, failed with it, is
                    # not left unheard for asyncio to log.
                    job.exception()
                raise
            finally:
                if not job.done():
                    job.set_exception(RuntimeError("the job's log ended before its job"))
                    job.exception()
                if isinstance(items, AsyncGenerator):
                    await items.aclose()
            return
        log = self._log(await self)
        try:
            async for entry in log:
                yield entry
        finally:
            if isinstance(log, AsyncGenerator):
                await log.aclose()
