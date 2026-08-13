"""Phase 1: verb dispatch is complete and unimplemented verbs fail loudly."""

from __future__ import annotations

import pytest

from bosn import cli

UNIMPLEMENTED = sorted(v for v, (_, phase) in cli.VERBS.items() if phase != "implemented")


@pytest.mark.parametrize("verb", UNIMPLEMENTED)
def test_unimplemented_verbs_fail_with_a_specific_error(verb: str, capsys) -> None:
    code = cli.main([verb])
    assert code == cli.NOT_IMPLEMENTED_EXIT, f"{verb} must not silently succeed"
    err = capsys.readouterr().err
    assert verb in err
    assert "not implemented" in err


def test_every_designed_verb_is_registered() -> None:
    designed = {"run", "shell", "tasks", "jobs", "attach", "status", "gc", "done", "doctor"}
    assert designed <= set(cli.VERBS)


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
