"""Runner-side gate for the Recovery-guest wheel smoke: exact waivers, no silent skips."""

from __future__ import annotations

import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "ci"))
import recovery_wheel_result as gate  # noqa: E402

LOG_ALL = """[wheel-smoke] inspect wheel archive
[wheel-smoke] create isolated virtual environment
[wheel-smoke] install wheel
[wheel-smoke] import installed extension
[wheel-smoke] verify installed CLI version
[wheel-smoke] verify offline doctor
[wheel-smoke] start daemon
[wheel-smoke] wait for daemon readiness
[wheel-smoke] verify online doctor
[wheel-smoke] stop daemon
[wheel-smoke] complete
"""

LOG_DAEMON_DIED = """[wheel-smoke] inspect wheel archive
[wheel-smoke] create isolated virtual environment
[wheel-smoke] install wheel
[wheel-smoke] import installed extension
[wheel-smoke] verify installed CLI version
[wheel-smoke] verify offline doctor
[wheel-smoke] start daemon
[wheel-smoke] wait for daemon readiness
Traceback (most recent call last):
AssertionError: installed daemon did not become ready; daemon=exited=1
"""


def test_full_pass_with_no_waiver_is_green() -> None:
    verdict = gate.evaluate(rc=0, log=LOG_ALL, waived=frozenset())
    assert verdict.ok
    assert verdict.last_phase == "complete"
    assert verdict.reached == 11


def test_failure_names_the_phase_it_died_in() -> None:
    verdict = gate.evaluate(rc=1, log=LOG_DAEMON_DIED, waived=frozenset())
    assert not verdict.ok
    assert verdict.last_phase == "wait for daemon readiness"
    assert "wait for daemon readiness" in verdict.reason
    assert "AssertionError" in verdict.reason


def test_failure_in_a_waived_phase_is_green_and_says_so() -> None:
    waived = frozenset({"wait for daemon readiness"})
    verdict = gate.evaluate(rc=1, log=LOG_DAEMON_DIED, waived=waived)
    assert verdict.ok
    assert "waived" in verdict.reason
    assert gate.WAIVED_PHASES == frozenset(), (
        "the committed waiver set must stay empty until proven"
    )


def test_stale_waiver_is_red_when_the_phase_now_passes() -> None:
    verdict = gate.evaluate(rc=0, log=LOG_ALL, waived=frozenset({"wait for daemon readiness"}))
    assert not verdict.ok
    assert "stale" in verdict.reason


def test_failure_outside_the_waiver_is_still_red() -> None:
    verdict = gate.evaluate(rc=1, log=LOG_DAEMON_DIED, waived=frozenset({"verify online doctor"}))
    assert not verdict.ok


def test_missing_interpreter_sentinel_is_named() -> None:
    verdict = gate.evaluate(rc=gate.PYTHON_MISSING_RC, log="", waived=frozenset())
    assert not verdict.ok
    assert "interpreter" in verdict.reason


def test_cli_reads_collected_directory_and_writes_summary(tmp_path: Path) -> None:
    collected = tmp_path / "collected"
    collected.mkdir()
    (collected / "wheel.rc").write_text("0\n")
    (collected / "wheel.log").write_text(LOG_ALL)
    (collected / "python.txt").write_text("Python 3.10.18 (main, Jun 12 2025) [Clang 20.1.4]\n")
    summary = tmp_path / "summary.md"
    assert gate.main(["--collected", str(collected), "--summary-file", str(summary)]) == 0
    text = summary.read_text()
    assert "Python 3.10.18" in text and "complete" in text and "11" in text


def test_cli_fails_when_the_guest_never_reported(tmp_path: Path) -> None:
    collected = tmp_path / "collected"
    collected.mkdir()
    with pytest.raises(SystemExit) as raised:
        gate.main(["--collected", str(collected)])
    assert raised.value.code != 0
