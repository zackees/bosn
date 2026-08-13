"""Phase 1: `bosn doctor` against a real engine. Docker-marked: Linux CI only."""

from __future__ import annotations

import pytest

from bosn import cli
from bosn.engine import Engine


@pytest.mark.docker
def test_engine_info_reports_versions(engine: Engine) -> None:
    info = engine.info()
    assert info.reachable
    assert info.client_version
    assert info.server_version


@pytest.mark.docker
def test_doctor_exits_zero_against_a_live_engine(capsys) -> None:
    assert cli.main(["doctor"]) == 0
    assert "reachable:      yes" in capsys.readouterr().out


@pytest.mark.docker
def test_engine_run_surfaces_failures(engine: Engine) -> None:
    result = engine.run(["not-a-docker-subcommand"])
    assert not result.ok
