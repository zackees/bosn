"""Shared test isolation for the native Bosn boundary."""

from __future__ import annotations

import os

import pytest

# zackees/ci.yml GATE-005 (bosn#361): bosn's tests start bosn daemons and touch
# bosn state roots, so the suite runs only in CI (CI=true: GitHub runners and
# act) or in the isolated bosn container (docker/test.Dockerfile sets
# BOSN_TEST_ISOLATED). The same rule guards the Rust suite (ci/test_guard.sh).
ISOLATION_MARKER = "BOSN_TEST_ISOLATED"


def pytest_configure(config: pytest.Config) -> None:
    if os.environ.get("CI") != "true" and not os.environ.get(ISOLATION_MARKER):
        pytest.exit(
            "bosn tests never run on the developer host: run `bosn run --task test` "
            "(or the whole local gate: ci-lint local-gate run). See local-gate.toml.",
            returncode=2,
        )


@pytest.fixture(autouse=True)
def isolated_state_dir(
    tmp_path_factory: pytest.TempPathFactory, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Never let a client test select the developer's Bosn state directory."""

    state_dir = tmp_path_factory.mktemp("bosn-state")
    monkeypatch.setenv("BOSN_STATE_DIR", str(state_dir))
    monkeypatch.delenv("BOSN_PORT", raising=False)
