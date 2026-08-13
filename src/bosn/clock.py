"""Time source.

Retention is a function of time, so time is injectable: TTL tests advance a fake clock
instead of sleeping out real durations.
"""

from __future__ import annotations

import time
from typing import Protocol


class Clock(Protocol):
    def now(self) -> float:
        """Seconds since the epoch."""
        ...


class SystemClock:
    def now(self) -> float:
        return time.time()


class FakeClock:
    """A manually advanced clock for retention tests."""

    def __init__(self, start: float = 1_000_000.0) -> None:
        self._now = float(start)

    def now(self) -> float:
        return self._now

    def advance(self, seconds: float) -> float:
        self._now += float(seconds)
        return self._now
