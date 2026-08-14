"""Phase 1: verb dispatch is complete and unimplemented verbs fail loudly."""

from __future__ import annotations

import json

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


def test_one_word_manifest_task_dispatches_as_run(monkeypatch) -> None:
    seen = {}

    def run_task(opts) -> int:
        seen["task"] = opts.task
        return 17

    monkeypatch.setattr(cli, "cmd_run", run_task)
    assert cli.main(["unit"]) == 17
    assert seen == {"task": "unit"}


def test_one_word_task_keeps_leading_global_options(monkeypatch, tmp_path) -> None:
    seen = {}

    def run_task(opts) -> int:
        seen["task"] = opts.task
        seen["manifest"] = opts.manifest
        return 0

    manifest = tmp_path / "bosn.toml"
    monkeypatch.setattr(cli, "cmd_run", run_task)
    assert cli.main(["--manifest", str(manifest), "unit"]) == 0
    assert seen == {"task": "unit", "manifest": manifest}


def test_builtin_verb_name_is_reserved_from_task_dispatch(monkeypatch) -> None:
    seen = []
    monkeypatch.setattr(cli, "cmd_run", lambda _opts: seen.append("run") or 0)
    monkeypatch.setattr(cli, "cmd_status", lambda _opts: seen.append("status") or 0)

    assert cli.main(["status"]) == 0
    assert seen == ["status"]


def test_tasks_json_reports_unregistered_readiness(tmp_path, capsys) -> None:
    (tmp_path / "Dockerfile").write_text("FROM scratch\n", encoding="utf-8")
    manifest = tmp_path / "bosn.toml"
    manifest.write_text(
        "[stack.dev]\ndockerfile = 'Dockerfile'\ndefault = true\n[task.unit]\ncmd = 'echo unit'\n",
        encoding="utf-8",
    )

    assert (
        cli.main(
            ["--state-dir", str(tmp_path / "state"), "--manifest", str(manifest), "tasks", "--json"]
        )
        == 0
    )
    payload = json.loads(capsys.readouterr().out)
    assert payload["stacks"]["dev"]["readiness"]["state"] == "unregistered"
    assert payload["tasks"]["unit"]["readiness"]["jobs"]["state"] == "unavailable"


def test_tasks_json_manifest_error_has_stable_remedy(tmp_path, capsys) -> None:
    assert cli.main(["--state-dir", str(tmp_path), "tasks", "--json"]) == 1
    payload = json.loads(capsys.readouterr().out)
    assert payload == {
        "ok": False,
        "code": "manifest.invalid",
        "message": payload["message"],
        "next": "create or select a valid bosn.toml with --manifest",
    }


def test_gc_json_daemon_error_has_stable_remedy(tmp_path, capsys, monkeypatch) -> None:
    from bosn import daemon

    def unavailable(*_args, **_kwargs):
        raise daemon.DaemonError("down")

    monkeypatch.setattr(daemon, "request", unavailable)
    assert cli.main(["--state-dir", str(tmp_path), "gc", "--json"]) == 1
    payload = json.loads(capsys.readouterr().out)
    assert payload["code"] == "daemon.unreachable"
    assert payload["next"] == "start or restart the daemon, then retry"


def test_done_json_error_uses_the_common_envelope(tmp_path, capsys) -> None:
    assert cli.main(["--state-dir", str(tmp_path), "done", "--json"]) == 1
    payload = json.loads(capsys.readouterr().out)
    assert payload["code"] == "command.failed"
    assert payload["next"] == "resolve the reported condition and retry the command"


def test_doctor_json_failure_emits_one_parseable_envelope(tmp_path, capsys) -> None:
    assert cli.main(["--json", "--engine", "definitely-not-an-engine", "doctor"]) == 1
    payload = json.loads(capsys.readouterr().out)
    assert payload["code"] == "command.failed"


def test_json_parse_error_has_the_stable_envelope(capsys) -> None:
    with pytest.raises(SystemExit) as exc:
        cli.main(["--json", "gc", "--apply", "--dry-run"])
    assert exc.value.code == 2
    assert json.loads(capsys.readouterr().out)["code"] == "parse.invalid"
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


def test_unknown_first_token_is_diagnosed_as_an_unknown_manifest_task(capsys) -> None:
    assert cli.main(["definitely-not-a-verb"]) == 1
    assert "no bosn.toml" in capsys.readouterr().err


def test_doctor_reports_unreachable_engine_and_scheduler_state_without_crashing(
    tmp_path, capsys
) -> None:
    code = cli.main(
        ["--state-dir", str(tmp_path), "--engine", "definitely-not-an-engine-binary", "doctor"]
    )
    assert code == 1
    captured = capsys.readouterr()
    assert "reachable:      no" in captured.out
    assert "scheduler manifest installed:" in captured.out
    assert "scheduler next deadline: -" in captured.out
    assert "not on PATH" in captured.err


def test_gc_reports_invalid_policy_without_a_traceback(monkeypatch, tmp_path, capsys) -> None:
    monkeypatch.setenv("BOSN_WARM_VOLUME_TTL", "not-a-number")
    assert cli.main(["--state-dir", str(tmp_path), "gc"]) == 1
    assert "warm_volume_ttl" in capsys.readouterr().err


def test_stopping_a_daemon_that_was_not_running_says_so(tmp_path, capsys) -> None:
    assert cli.main(["--state-dir", str(tmp_path), "__daemon", "--stop"]) == 0
    assert "no daemon was running" in capsys.readouterr().out


def test_a_daemon_still_draining_is_not_reported_as_absent(tmp_path, monkeypatch, capsys) -> None:
    """Stopping now waits for in-flight builds, so the wait can expire on a live daemon.

    Saying "no daemon was running" there would be a plainly wrong answer to the question
    the user asked, and it would send them looking in the wrong place.
    """
    from bosn import daemon as daemon_mod

    monkeypatch.setattr(daemon_mod, "is_serving", lambda *a, **k: True)
    monkeypatch.setattr(daemon_mod, "stop", lambda *a, **k: False)

    assert cli.main(["--state-dir", str(tmp_path), "__daemon", "--stop"]) == 1
    err = capsys.readouterr().err
    assert "still shutting down" in err
    assert "bosn jobs" in err
