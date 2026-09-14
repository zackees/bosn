"""Shared test isolation for the native Bosn boundary."""

from __future__ import annotations

import pytest


@pytest.fixture(autouse=True)
def isolated_state_dir(
    tmp_path_factory: pytest.TempPathFactory, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Never let a client test select the developer's Bosn state directory."""

    state_dir = tmp_path_factory.mktemp("bosn-state")
    monkeypatch.setenv("BOSN_STATE_DIR", str(state_dir))
    monkeypatch.delenv("BOSN_PORT", raising=False)
