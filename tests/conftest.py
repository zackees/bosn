"""Shared fixtures. Docker-marked tests skip themselves when no engine is reachable."""

from __future__ import annotations

import pytest

from bosn.engine import Engine, engine_reachable

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
