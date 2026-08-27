"""Phase 1: verb dispatch is complete and unimplemented verbs fail loudly."""

from __future__ import annotations

import json

import pytest

from bosn import cli, ipc

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


def test_daemon_stop_is_a_public_help_visible_verb() -> None:
    help_text = cli.build_parser().format_help()

    assert "daemon-stop" in cli.VERBS
    assert "daemon-stop" in help_text


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


def test_status_surfaces_a_foreground_session_without_waiting_for_engine_scan(
    tmp_path, monkeypatch, capsys
) -> None:
    from bosn import daemon

    state = tmp_path / "state"
    state.mkdir()
    (state / "registry.sqlite3").touch()
    monkeypatch.setattr(
        daemon,
        "request",
        lambda *_args, **_kwargs: {
            "registry_id": "registry",
            "execution_sessions": [
                {"id": "session", "client_alive": False, "blocking_reason": "awaiting reap"}
            ],
        },
    )
    monkeypatch.setattr(cli, "Engine", lambda *_args: (_ for _ in ()).throw(AssertionError()))

    assert cli.main(["--state-dir", str(state), "status", "--json"]) == 0
    report = json.loads(capsys.readouterr().out)
    assert report["execution_sessions"][0]["id"] == "session"


def test_status_uses_persisted_session_proof_when_daemon_control_stream_is_lost(
    tmp_path, monkeypatch, capsys
) -> None:
    """#119: terminal-only jobs must not hide a disconnected foreground client.

    This deliberately uses the durable session record rather than a daemon fake response:
    it is the path available when the control RPC itself has timed out.  Constructing an
    Engine would prove a regression to slow inventory fallback.
    """
    from bosn import daemon, resources
    from bosn.registry import ExecutionSession, Registry

    state = tmp_path / "state"
    with Registry(state / "registry.sqlite3") as registry:
        registry.save_execution_session(
            ExecutionSession("orphan", "immutable-id", "podman", 4242, 9.0, ("lease",))
        )
        registry.log_event(
            "execution.orphan_reap.error",
            "session=orphan container=immutable-id RuntimeError: busy",
        )
    calls = []

    def lost_stream(*_args, **kwargs):
        calls.append(kwargs)
        raise ipc.TransportTimeout("timed out waiting for the daemon")

    monkeypatch.setattr(daemon, "request", lost_stream)
    monkeypatch.setattr(resources, "process_alive", lambda *_args: False)
    monkeypatch.setattr(cli, "Engine", lambda *_args: (_ for _ in ()).throw(AssertionError()))

    assert cli.main(["--state-dir", str(state), "status", "--json"]) == 0
    report = json.loads(capsys.readouterr().out)
    assert calls == [{"autostart": False, "request_timeout": cli.STATUS_DAEMON_TIMEOUT_SECONDS}]
    assert report["mode"] == "degraded"
    assert report["execution_sessions"][0]["id"] == "orphan"
    assert report["execution_sessions"][0]["last_orphan_reap_error"]["detail"].endswith("busy")
    assert "control channel is unavailable" in report["execution_sessions"][0]["recovery"]
    assert "do not run `bosn daemon-stop` yet" in report["next"]


def test_status_is_bounded_when_a_daemon_stream_is_lost_without_a_session(
    tmp_path, monkeypatch, capsys
) -> None:
    """#119: a completed foreground command must not send status into engine inventory."""
    from bosn import daemon
    from bosn.registry import Registry

    state = tmp_path / "state"
    with Registry(state / "registry.sqlite3"):
        pass

    monkeypatch.setattr(
        daemon,
        "request",
        lambda *_args, **_kwargs: (_ for _ in ()).throw(
            ipc.TransportTimeout("timed out waiting for the daemon")
        ),
    )
    monkeypatch.setattr(cli, "Engine", lambda *_args: (_ for _ in ()).throw(AssertionError()))

    assert cli.main(["--state-dir", str(state), "status", "--json"]) == 0
    report = json.loads(capsys.readouterr().out)
    assert report["mode"] == "degraded"
    assert report["execution_sessions"] == []
    assert "Restart the daemon" in report["next"]


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


def test_tasks_json_manifest_error_has_stable_remedy(tmp_path, monkeypatch, capsys) -> None:
    monkeypatch.chdir(tmp_path)
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


def test_gc_timeout_is_not_reported_as_an_unreachable_daemon(tmp_path, capsys, monkeypatch) -> None:
    """A busy daemon and an absent one have opposite remedies (#110).

    `ipc.TransportTimeout` subclasses `TransportError`, so before this distinction existed
    a `gc` that simply took longer than its budget fell into the clause above and told the
    user to "start or restart the daemon" -- the one action that would interrupt the
    collection being waited on, which continues regardless of this client giving up. I hit
    this for real running `bosn gc` on a host with ~280 Docker objects.
    """
    from bosn import daemon

    def slow(*_args, **_kwargs):
        raise ipc.TransportTimeout("timed out waiting for the daemon")

    monkeypatch.setattr(daemon, "request", slow)
    assert cli.main(["--state-dir", str(tmp_path), "gc", "--json"]) == 1
    payload = json.loads(capsys.readouterr().out)
    assert payload["code"] == "gc.timeout"
    assert "do not restart it" in payload["next"]
    assert "restart the daemon, then retry" not in payload["next"]


def test_gc_asks_the_daemon_for_more_than_the_shared_default_budget(tmp_path, monkeypatch) -> None:
    """`gc`'s daemon-side cost is unrelated to what every other verb needs (#110).

    Measured on a ~280-object host, `docker system df -v` alone is 5.2s and the scan adds
    7-24s more, so the shared 10s default could not cover even the first step. Asserting on
    the request rather than on the constant, so the wiring is what is pinned -- defining
    the constant and forgetting to pass it would leave the bug in place.
    """
    from bosn import daemon

    seen: dict[str, object] = {}

    def capture(verb, *_args, **kwargs):
        seen["verb"] = verb
        seen["request_timeout"] = kwargs.get("request_timeout")
        return {"ok": True, "result": {"collected": [], "kept": []}}

    monkeypatch.setattr(daemon, "request", capture)
    cli.main(["--state-dir", str(tmp_path), "gc", "--dry-run", "--json"])
    assert seen["verb"] == "gc"
    assert seen["request_timeout"] == cli.GC_REQUEST_TIMEOUT_SECONDS
    assert cli.GC_REQUEST_TIMEOUT_SECONDS > ipc.DEFAULT_TIMEOUT


def test_gc_json_reports_images_deferred_by_container_dependencies(
    tmp_path, capsys, monkeypatch
) -> None:
    from bosn import daemon

    def deferred(*_args, **_kwargs):
        image_decisions = [
            {
                "name": "sha256:current",
                "action": "kept",
                "eligible": False,
                "reason": "current-image",
            },
            {
                "name": "sha256:eager",
                "action": "would-remove",
                "eligible": True,
                "reason": "superseded-image",
            },
            {
                "name": "sha256:referenced",
                "action": "deferred",
                "eligible": False,
                "reason": "image-referenced",
                "candidate_reason": "superseded-image",
                "referenced_by": ["stopped-container"],
            },
            {
                "name": "sha256:unknown",
                "action": "deferred",
                "eligible": False,
                "reason": "image-dependency-unknown",
                "candidate_reason": "superseded-image",
            },
        ]
        return {
            "ok": True,
            "result": {"removed": 0, "image_dependency_deferred": 2},
            "image_dependency_deferred": ["sha256:referenced", "sha256:unknown"],
            "image_decisions": image_decisions,
        }

    monkeypatch.setattr(daemon, "request", deferred)

    assert cli.main(["--state-dir", str(tmp_path), "gc", "--json"]) == 0
    payload = json.loads(capsys.readouterr().out)
    assert {decision["reason"] for decision in payload["image_decisions"]} == {
        "current-image",
        "superseded-image",
        "image-referenced",
        "image-dependency-unknown",
    }
    referenced = next(
        decision
        for decision in payload["image_decisions"]
        if decision["reason"] == "image-referenced"
    )
    assert referenced["referenced_by"] == ["stopped-container"]


def test_adopt_asks_the_daemon_for_a_transfer_sized_budget(tmp_path, monkeypatch) -> None:
    from bosn import daemon, resources

    seen: dict[str, object] = {}

    def capture(verb, *_args, **kwargs):
        seen["verb"] = verb
        seen["request_timeout"] = kwargs.get("request_timeout")
        return {"ok": True, "transferred": ["cache"], "registry_id": "ours"}

    monkeypatch.setattr(daemon, "request", capture)
    assert (
        cli.main(
            [
                "--state-dir",
                str(tmp_path),
                "adopt",
                "--transfer",
                "volume:cache",
                "--transfer",
                "volume:second-cache",
                "--json",
            ]
        )
        == 0
    )
    assert seen["verb"] == "adopt"
    expected = cli.ADOPT_SCAN_REQUEST_TIMEOUT_SECONDS + (
        4 * resources.VOLUME_TRANSFER_COPY_TIMEOUT_SECONDS
    )
    assert seen["request_timeout"] == expected
    assert seen["request_timeout"] == cli.adopt_request_timeout_seconds(2)
    assert expected > ipc.DEFAULT_TIMEOUT


def test_adopt_timeout_does_not_misreport_the_daemon_as_unreachable(
    tmp_path, monkeypatch, capsys
) -> None:
    from bosn import daemon

    def slow(*_args, **_kwargs):
        raise ipc.TransportTimeout("timed out waiting for the daemon")

    monkeypatch.setattr(daemon, "request", slow)
    assert cli.main(["--state-dir", str(tmp_path), "adopt", "--json"]) == 1
    payload = json.loads(capsys.readouterr().out)
    assert payload["code"] == "adopt.timeout"
    assert "do not restart" in payload["next"]
    assert "retry" not in payload["next"]


def test_done_json_error_uses_the_common_envelope(tmp_path, monkeypatch, capsys) -> None:
    monkeypatch.chdir(tmp_path)
    assert cli.main(["--state-dir", str(tmp_path), "done", "--json"]) == 1
    payload = json.loads(capsys.readouterr().out)
    assert payload["code"] == "command.failed"
    assert payload["next"] == "resolve the reported condition and retry the command"


def test_doctor_json_failure_emits_one_parseable_envelope(tmp_path, capsys) -> None:
    assert cli.main(["--json", "--engine", "definitely-not-an-engine", "doctor"]) == 1
    payload = json.loads(capsys.readouterr().out)
    assert payload["code"] == "command.failed"


def test_run_json_failure_reserves_stdout_for_one_envelope(tmp_path, monkeypatch, capsys) -> None:
    from bosn import daemon
    from bosn.converge import ConvergeResult
    from bosn.engine import EngineResult

    (tmp_path / "Dockerfile").write_text("FROM scratch\n", encoding="utf-8")
    manifest = tmp_path / "bosn.toml"
    manifest.write_text(
        "[stack.dev]\ndockerfile = 'Dockerfile'\ndefault = true\n",
        encoding="utf-8",
    )
    converged = ConvergeResult("dev", "sha256:g", "reused", "image")
    monkeypatch.setattr(cli, "_converge_via_daemon", lambda *_args: converged)

    def request(verb, *_args, **_kwargs):
        if verb == "execution-acquire":
            assert isinstance(_kwargs["pid"], int)
            assert "proc_start" in _kwargs
            return {"ok": True, "container": "sha256:container", "session": "session"}
        if verb == "execution-release":
            return {"ok": True}
        raise AssertionError(f"unexpected daemon request: {verb}")

    monkeypatch.setattr(daemon, "request", request)

    class FailingEngine:
        def __init__(self, _binary="docker") -> None:
            pass

        def run(self, *_args, **_kwargs):
            return EngineResult(7, "partial stdout", "partial stderr")

        def execute(self, *_args, **_kwargs):
            raise AssertionError("JSON mode must reserve stdout instead of inheriting it")

    monkeypatch.setattr(cli, "Engine", FailingEngine)

    code = cli.main(
        [
            "--state-dir",
            str(tmp_path / "state"),
            "--manifest",
            str(manifest),
            "--json",
            "run",
            "--",
            "false",
        ]
    )

    assert code == 1
    payload = json.loads(capsys.readouterr().out)
    assert payload["code"] == "command.failed"
    assert "partial stdout" in payload["message"]
    assert "partial stderr" in payload["message"]


@pytest.mark.parametrize(("verb", "engine_method"), [("run", "execute"), ("shell", "interactive")])
def test_successful_foreground_command_fails_visibly_when_release_fails(
    verb, engine_method, tmp_path, monkeypatch, capsys
) -> None:
    from bosn import daemon, resources
    from bosn.converge import ConvergeResult

    (tmp_path / "Dockerfile").write_text("FROM scratch\n", encoding="utf-8")
    manifest = tmp_path / "bosn.toml"
    manifest.write_text(
        "[stack.dev]\ndockerfile = 'Dockerfile'\ndefault = true\n",
        encoding="utf-8",
    )
    monkeypatch.setattr(
        cli,
        "_converge_via_daemon",
        lambda *_args: ConvergeResult("dev", "sha256:g", "reused", "image"),
    )
    monkeypatch.setattr(resources, "process_start_time", lambda _pid: 10.0)

    def request(requested_verb, *_args, **_kwargs):
        if requested_verb == "execution-acquire":
            return {"ok": True, "container": "sha256:container", "session": "stuck-session"}
        if requested_verb == "execution-release":
            return {"ok": False, "error": "database is locked"}
        raise AssertionError(f"unexpected daemon request: {requested_verb}")

    monkeypatch.setattr(daemon, "request", request)

    class SuccessfulEngine:
        def __init__(self, _binary="docker") -> None:
            pass

        def execute(self, *_args, **_kwargs) -> int:
            assert engine_method == "execute"
            return 0

        def interactive(self, *_args, **_kwargs) -> int:
            assert engine_method == "interactive"
            return 0

    monkeypatch.setattr(cli, "Engine", SuccessfulEngine)
    args = ["--manifest", str(manifest), verb]
    if verb == "run":
        args.extend(["--", "true"])

    assert cli.main(args) == 1
    error = capsys.readouterr().err
    assert "execution cleanup failed for session stuck-session" in error
    assert "database is locked" in error


def test_failed_command_keeps_its_exit_code_when_release_also_fails(
    tmp_path, monkeypatch, capsys
) -> None:
    from bosn import daemon, resources
    from bosn.converge import ConvergeResult

    (tmp_path / "Dockerfile").write_text("FROM scratch\n", encoding="utf-8")
    manifest = tmp_path / "bosn.toml"
    manifest.write_text(
        "[stack.dev]\ndockerfile = 'Dockerfile'\ndefault = true\n",
        encoding="utf-8",
    )
    monkeypatch.setattr(
        cli,
        "_converge_via_daemon",
        lambda *_args: ConvergeResult("dev", "sha256:g", "reused", "image"),
    )
    monkeypatch.setattr(resources, "process_start_time", lambda _pid: 10.0)
    monkeypatch.setattr(
        daemon,
        "request",
        lambda verb, *_args, **_kwargs: (
            {"ok": True, "container": "sha256:container", "session": "stuck-session"}
            if verb == "execution-acquire"
            else {"ok": False, "error": "database is locked"}
        ),
    )

    class FailedEngine:
        def __init__(self, _binary="docker") -> None:
            pass

        def execute(self, *_args, **_kwargs) -> int:
            return 7

    monkeypatch.setattr(cli, "Engine", FailedEngine)

    assert cli.main(["--manifest", str(manifest), "run", "--", "false"]) == 7
    assert "execution cleanup failed" in capsys.readouterr().err


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


def test_unknown_first_token_is_diagnosed_as_an_unknown_manifest_task(
    tmp_path, monkeypatch, capsys
) -> None:
    monkeypatch.chdir(tmp_path)
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


def test_doctor_fails_with_actionable_warning_when_engine_clock_is_skewed(
    tmp_path, monkeypatch, capsys
) -> None:
    from bosn.engine import EngineInfo

    class SkewedEngine:
        def __init__(self, binary: str = "docker") -> None:
            self.binary = binary

        def info(self):
            return EngineInfo(
                binary=self.binary,
                reachable=True,
                client_version="28.5.1",
                server_version="28.5.1",
                clock_skew_seconds=2.25,
            )

    monkeypatch.setattr(cli, "Engine", SkewedEngine)
    _stub_scan(monkeypatch, {})

    code = cli.main(["--state-dir", str(tmp_path), "doctor"])

    assert code == 1
    captured = capsys.readouterr()
    assert "clock skew:     +2.250s" in captured.out
    assert "incremental builds" in captured.err
    assert "synchronize" in captured.err


def _stub_reachable_engine(monkeypatch) -> None:
    """Doctor only reaches the foreign-registry report once the engine is reachable."""
    from bosn.engine import EngineInfo

    class FakeEngine:
        def __init__(self, binary: str = "docker") -> None:
            self.binary = binary

        def info(self):
            return EngineInfo(binary=self.binary, reachable=True, client_version="1.0")

    monkeypatch.setattr(cli, "Engine", FakeEngine)


def _stub_scan(monkeypatch, foreign_by_registry: dict[str, int]) -> None:
    """Fake a scan whose foreign bucket holds ``count`` resources per registry id."""
    import bosn.resources
    from bosn import labels
    from bosn.resources import DiscoveredResource, ScanResult

    def _raw(registry: str) -> dict[str, str]:
        return labels.ResourceLabels(
            registry=registry,
            kind="volume",
            stack="dev",
            generation="digest",
            scope="spec",
            workspace="workspace",
            created="2026-01-01T00:00:00Z",
        ).to_dict()

    foreign = [
        DiscoveredResource("volume", f"{registry_id}-{i}", _raw(registry_id))
        for registry_id, count in foreign_by_registry.items()
        for i in range(count)
    ]

    class Scanner:
        def __init__(self, _engine) -> None:
            pass

        def scan(self, _registry_id, **_kwargs):
            return ScanResult(foreign=foreign)

    monkeypatch.setattr(bosn.resources, "ResourceScanner", Scanner)


def test_doctor_reports_few_foreign_registries_as_exact_adopt_commands(
    tmp_path, monkeypatch, capsys
) -> None:
    """The case doctor's foreign-registry report was built for: one lost identity."""
    _stub_reachable_engine(monkeypatch)
    _stub_scan(monkeypatch, {"lost-registry-1": 2, "lost-registry-2": 1})

    code = cli.main(["--state-dir", str(tmp_path), "doctor"])
    assert code == 0
    err = capsys.readouterr().err
    assert f"bosn --state-dir {tmp_path} adopt --from-registry lost-registry-1" in err
    assert f"bosn --state-dir {tmp_path} adopt --from-registry lost-registry-2" in err


def test_doctor_aggregates_many_foreign_registries_instead_of_listing_each(
    tmp_path, monkeypatch, capsys
) -> None:
    """150 foreign ids must not become 150 semicolon-joined commands on one line."""
    _stub_reachable_engine(monkeypatch)
    foreign_by_registry = {f"leaked-registry-{i}": 1 for i in range(150)}
    _stub_scan(monkeypatch, foreign_by_registry)

    code = cli.main(["--state-dir", str(tmp_path), "doctor"])
    assert code == 0
    err = capsys.readouterr().err

    # The aggregate counts are reported...
    assert "150" in err
    assert "150 registry ids" in err or "registry ids" in err
    # ...but no per-id adopt command is emitted for the bulk of them.
    assert err.count("adopt --from-registry leaked-registry-") == 0
    assert "adopt --from-registry <id>" in err


def test_doctor_reports_total_resource_count_not_just_id_count(
    tmp_path, monkeypatch, capsys
) -> None:
    """150 ids holding 151 resources and a few ids holding thousands read very differently."""
    _stub_reachable_engine(monkeypatch)
    heavy = {"heavy-registry-a": 3000, "heavy-registry-b": 2000}
    padding = {f"leaked-registry-{i}": 1 for i in range(10)}
    _stub_scan(monkeypatch, {**heavy, **padding})

    code = cli.main(["--state-dir", str(tmp_path), "doctor"])
    assert code == 0
    err = capsys.readouterr().err
    assert "5010" in err  # total resource count, not just the 12 distinct ids
    assert "heavy-registry-a" in err
    assert "heavy-registry-b" in err


def test_doctor_foreign_registry_wording_never_claims_dead_or_safe_to_delete(
    tmp_path, monkeypatch, capsys
) -> None:
    """bosn cannot distinguish a stale registry from one owned by a live user elsewhere."""
    _stub_reachable_engine(monkeypatch)
    foreign_by_registry = {f"leaked-registry-{i}": 1 for i in range(150)}
    _stub_scan(monkeypatch, foreign_by_registry)

    code = cli.main(["--state-dir", str(tmp_path), "doctor"])
    assert code == 0
    err = capsys.readouterr().err.lower()

    for forbidden in ("orphan", "dead", "stale", "safe to delete", "safe to remove"):
        assert forbidden not in err, f"doctor must not claim foreign resources are {forbidden!r}"


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


def test_daemon_stop_surfaces_an_active_session_refusal(tmp_path, monkeypatch, capsys) -> None:
    from bosn import daemon as daemon_mod

    monkeypatch.setattr(daemon_mod, "is_serving", lambda *a, **k: True)
    monkeypatch.setattr(
        daemon_mod,
        "stop",
        lambda *a, **k: (_ for _ in ()).throw(
            daemon_mod.DaemonError(
                "daemon has active execution session(s); wait for run or shell to exit"
            )
        ),
    )

    assert cli.main(["--state-dir", str(tmp_path), "daemon-stop"]) == 1
    err = capsys.readouterr().err
    assert "active execution session" in err
    assert "still shutting down" not in err


# -- init: Compose migration moved under `bosn` (#46) -------------------------------------


def _write_compose(tmp_path, image: str = "alpine:3.20"):
    compose = tmp_path / "compose.yaml"
    compose.write_text(f"services:\n  app:\n    image: {image}\n", encoding="utf-8")
    return compose


def test_init_writes_a_manifest_from_a_compose_file(tmp_path, capsys) -> None:
    compose = _write_compose(tmp_path)
    output = tmp_path / "bosn.toml"

    code = cli.main(["init", "--compose", str(compose), "--output", str(output)])

    assert code == 0
    assert output.exists()
    assert 'image = "alpine:3.20"' in output.read_text(encoding="utf-8")
    assert str(output) in capsys.readouterr().out


def test_init_refuses_to_overwrite_an_existing_output(tmp_path, capsys) -> None:
    compose = _write_compose(tmp_path)
    output = tmp_path / "bosn.toml"
    output.write_text("# pre-existing\n", encoding="utf-8")

    code = cli.main(["init", "--compose", str(compose), "--output", str(output)])

    assert code == 1
    err = capsys.readouterr().err
    assert "refusing to overwrite" in err
    assert str(output) in err


def test_init_refuses_to_overwrite_with_the_json_envelope(tmp_path, capsys) -> None:
    compose = _write_compose(tmp_path)
    output = tmp_path / "bosn.toml"
    output.write_text("# pre-existing\n", encoding="utf-8")

    code = cli.main(["init", "--compose", str(compose), "--output", str(output), "--json"])

    assert code == 1
    payload = json.loads(capsys.readouterr().out)
    assert payload["ok"] is False
    assert payload["code"] == "init.failed"
    assert "refusing to overwrite" in payload["message"]
    assert payload["next"]


def test_init_malformed_compose_produces_the_envelope_not_a_traceback(tmp_path, capsys) -> None:
    compose = tmp_path / "compose.yaml"
    compose.write_text("services:\n  app:\n    build: {}\n", encoding="utf-8")
    output = tmp_path / "bosn.toml"

    code = cli.main(["init", "--compose", str(compose), "--output", str(output), "--json"])

    assert code == 1
    payload = json.loads(capsys.readouterr().out)
    assert payload["ok"] is False
    assert payload["code"] == "init.failed"
    assert payload["next"]
    assert not output.exists()


def test_bosn_docker_init_alias_still_works(tmp_path, capsys) -> None:
    from bosn import docker_cli

    compose = _write_compose(tmp_path)
    output = tmp_path / "bosn.toml"

    code = docker_cli.main(["init", "--compose", str(compose), "--output", str(output)])

    assert code == 0
    assert output.exists()
    assert 'image = "alpine:3.20"' in output.read_text(encoding="utf-8")


def test_init_help_lists_the_compose_and_output_flags(capsys) -> None:
    with pytest.raises(SystemExit) as exc:
        cli.main(["init", "--help"])
    assert exc.value.code == 0
    out = capsys.readouterr().out
    assert "--compose" in out
    assert "--output" in out
