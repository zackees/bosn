"""Shared fixtures. Docker-marked tests skip themselves when no engine is reachable."""

from __future__ import annotations

import pytest

from bosn.engine import Engine, engine_reachable


@pytest.fixture(autouse=True)
def isolated_state_dir(tmp_path_factory, monkeypatch) -> None:
    """Never let a test touch the developer's real registry or spawn a real daemon there."""
    state_dir = tmp_path_factory.mktemp("bosn-state")
    monkeypatch.setenv("BOSN_STATE_DIR", str(state_dir))
    monkeypatch.delenv("BOSN_PORT", raising=False)


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
