"""Shared fixtures. Docker-marked tests skip themselves when no engine is reachable."""

from __future__ import annotations

import os

import pytest

from bosn.engine import Engine, engine_reachable
from bosn.resources import process_start_time


@pytest.fixture(autouse=True)
def isolated_state_dir(tmp_path_factory, monkeypatch) -> None:
    """Never let a test touch the developer's real registry or spawn a real daemon there."""
    state_dir = tmp_path_factory.mktemp("bosn-state")
    monkeypatch.setenv("BOSN_STATE_DIR", str(state_dir))
    monkeypatch.delenv("BOSN_PORT", raising=False)


_LIVE_PROC_START: float | None = None
_LIVE_PROC_START_PROBED = False


def live_proc_start() -> float | None:
    """The real process creation time of the test process, for tests that hold a lease.

    A lease whose ``proc_start`` is a wall-clock guess no longer survives its TTL: liveness
    now proves holder identity, not just the PID. Tests that mean "this live process holds
    the lease" must therefore store the identity the probe will actually report. Probed once
    per session because the Windows probe spawns a PowerShell process.
    """
    global _LIVE_PROC_START, _LIVE_PROC_START_PROBED
    if not _LIVE_PROC_START_PROBED:
        _LIVE_PROC_START = process_start_time(os.getpid())
        _LIVE_PROC_START_PROBED = True
    return _LIVE_PROC_START


_ENGINE_REACHABLE: bool | None = None


def _reachable() -> bool:
    global _ENGINE_REACHABLE
    if _ENGINE_REACHABLE is None:
        _ENGINE_REACHABLE = engine_reachable()
    return bool(_ENGINE_REACHABLE)


@pytest.fixture(scope="session")
def engine() -> Engine:
    return Engine()


def pytest_runtest_setup(item: pytest.Item) -> None:
    if item.get_closest_marker("docker") and not _reachable():
        pytest.skip("no reachable Docker engine")
