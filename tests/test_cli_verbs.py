"""Phase 1: verb dispatch is complete and unimplemented verbs fail loudly."""

from __future__ import annotations

import pytest

from bosn import cli

UNIMPLEMENTED = sorted(v for v, (_, phase) in cli.VERBS.items() if phase != "implemented")


@pytest.mark.skipif(not UNIMPLEMENTED, reason="every designed verb is implemented")
@pytest.mark.parametrize("verb", UNIMPLEMENTED or [""])
def test_unimplemented_verbs_fail_with_a_specific_error(verb: str, capsys) -> None:
    code = cli.main([verb])
    assert code == cli.NOT_IMPLEMENTED_EXIT, f"{verb} must not silently succeed"
    err = capsys.readouterr().err
    assert verb in err
    assert "not implemented" in err


def test_the_unimplemented_path_still_fails_loudly() -> None:
    """Kept live even with nothing left to skip: a placeholder must never be a no-op."""
    error = cli.VerbNotImplementedError("someday", "a later phase")
    assert "someday" in str(error)
    assert "not implemented" in str(error)


def test_every_designed_verb_is_registered() -> None:
    designed = {
        "run",
        "shell",
        "tasks",
        "jobs",
        "attach",
        "cancel",
        "status",
        "gc",
        "done",
        "doctor",
    }
    assert designed <= set(cli.VERBS)


def test_attach_and_cancel_are_no_longer_placeholders() -> None:
    """Both landed with daemon-owned jobs; `attach`'s stale 'phase 6' label went with them."""
    assert cli.VERBS["attach"][1] == "implemented"
    assert cli.VERBS["cancel"][1] == "implemented"


def test_attach_without_a_job_id_explains_itself(capsys) -> None:
    assert cli.main(["attach"]) == 2
    assert "job id" in capsys.readouterr().err


def test_cancel_without_a_job_id_explains_itself(capsys) -> None:
    assert cli.main(["cancel"]) == 2
    assert "job id" in capsys.readouterr().err


def test_cancel_fails_closed_when_no_daemon_is_running(tmp_path, capsys) -> None:
    """No daemon means no jobs to cancel -- and never a fallback to raw Docker."""
    code = cli.main(["--state-dir", str(tmp_path), "cancel", "j1-abc"])
    assert code == 1
    assert "cannot reach the bosn daemon" in capsys.readouterr().err


def test_unknown_verb_is_rejected() -> None:
    with pytest.raises(SystemExit) as exc:
        cli.main(["definitely-not-a-verb"])
    assert exc.value.code != 0


def test_doctor_reports_unreachable_engine_without_crashing(capsys) -> None:
    code = cli.main(["--engine", "definitely-not-an-engine-binary", "doctor"])
    assert code == 1
    captured = capsys.readouterr()
    assert "reachable:      no" in captured.out
    assert "not on PATH" in captured.err
