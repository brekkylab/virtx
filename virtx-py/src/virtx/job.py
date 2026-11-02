"""What every job's log shares: each entry numbered, and the mark of a log cut short.

A job, an image's build, pull or push or a process in a sandbox, runs on the server apart from
the request that started it. Its log is read with the client's ``*_job_log``, from its start or
after an entry's ``id``, until its end entry.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Generic, TypeVar

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
