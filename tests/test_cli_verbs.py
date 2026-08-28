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


def test_reconcile_volume_derives_a_manifest_target_before_requesting_daemon(
    tmp_path, monkeypatch, capsys
) -> None:
    from bosn import daemon

    manifest = tmp_path / "bosn.toml"
    (tmp_path / "Dockerfile").write_text("FROM alpine\n", encoding="utf-8")
    manifest.write_text(
        "[stack.perf]\ndockerfile = 'Dockerfile'\n\n[stack.perf.volumes]\n"
        "target = { scope = 'stack' }\n",
        encoding="utf-8",
    )
    requests = []
    monkeypatch.setattr(
        daemon,
        "request",
        lambda verb, *_args, **kwargs: (
            requests.append((verb, kwargs))
            or {
                "ok": True,
                "applied": kwargs["apply"],
                "plan": {"name": "derived", "decision": {"action": "would-recreate"}},
            }
        ),
    )

    preview = [
        "--manifest",
        str(manifest),
        "--json",
        "reconcile-volume",
        "--stack",
        "perf",
        "--volume",
        "target",
    ]
    assert cli.main(preview) == 0
    assert cli.main([*preview, "--apply", "--yes"]) == 0
    assert [entry[1]["apply"] for entry in requests] == [False, True]
    assert all(entry[1]["volume"] == "target" for entry in requests)
    assert '"applied": true' in capsys.readouterr().out.lower()


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
            "resources": 3,
            "execution_sessions": [
                {"id": "session", "client_alive": False, "blocking_reason": "awaiting reap"}
            ],
        },
    )
    monkeypatch.setattr(cli, "Engine", lambda *_args: (_ for _ in ()).throw(AssertionError()))

    assert cli.main(["--state-dir", str(state), "status", "--json"]) == 0
    report = json.loads(capsys.readouterr().out)
    assert report["mode"] == "online"
    assert report["registered"] == 3
    assert report["execution_sessions"][0]["id"] == "session"


def test_jobs_is_a_bounded_diagnostic_and_never_autostarts(tmp_path, monkeypatch, capsys) -> None:
    from bosn import daemon

    calls = []

    def request(*args, **kwargs):
        calls.append((args, kwargs))
        raise ipc.TransportTimeout("timed out waiting for the daemon")

    monkeypatch.setattr(daemon, "request", request)
    assert cli.main(["--state-dir", str(tmp_path), "jobs"]) == 1
    assert calls[0][1] == {
        "autostart": False,
        "request_timeout": cli.JOBS_DAEMON_TIMEOUT_SECONDS,
        "diagnostic": True,
    }
    assert "timed out" in capsys.readouterr().err


def test_status_returns_a_healthy_empty_session_report_without_engine_inventory(
    tmp_path, monkeypatch, capsys
) -> None:
    """#119: an idle healthy daemon is a complete status answer, not a Docker scan."""
    from bosn import daemon

    state = tmp_path / "state"
    state.mkdir()
    (state / "registry.sqlite3").touch()
    monkeypatch.setattr(
        daemon,
        "request",
        lambda *_args, **_kwargs: {
            "ok": True,
            "registry_id": "registry",
            "resources": 0,
            "execution_sessions": [],
            "pid": 42,
            "version": "test",
        },
    )
    monkeypatch.setattr(cli, "Engine", lambda *_args: (_ for _ in ()).throw(AssertionError()))

    assert cli.main(["--state-dir", str(state), "status", "--json"]) == 0
    assert json.loads(capsys.readouterr().out) == {
        "mode": "online",
        "registry_id": "registry",
        "registered": 0,
        "execution_sessions": [],
        "daemon": {"reachable": True, "pid": 42, "version": "test"},
        "next": "no foreground execution session is blocking the daemon",
    }


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
    assert calls == [
        {
            "autostart": False,
            "request_timeout": cli.STATUS_DAEMON_TIMEOUT_SECONDS,
            "diagnostic": True,
        }
    ]
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
    assert report["registered"] == 0
    assert report["execution_sessions"] == []
    assert "Restore or restart Bosn" in report["next"]


@pytest.mark.parametrize("timeout_verb", ["ping", "status"])
def test_status_preserves_a_real_diagnostic_request_timeout_as_degraded(
    tmp_path, monkeypatch, capsys, timeout_verb
) -> None:
    """#119: request's liveness probe must not hide a contacted-but-stuck daemon.

    This deliberately invokes the real ``daemon.request`` and real ``is_serving`` rather
    than replacing either: the fake transport models the two wire positions where a timeout
    can occur. A dead port still raises ordinary ``TransportError`` and is reported offline.
    """
    from bosn.registry import Registry

    state = tmp_path / "state"
    with Registry(state / "registry.sqlite3"):
        pass
    calls = []

    def transport(_port, request, *, timeout, **_kwargs):
        calls.append((request["verb"], timeout))
        if request["verb"] == timeout_verb:
            raise ipc.TransportTimeout("timed out waiting for the daemon")
        return {"ok": True}

    monkeypatch.setattr(ipc, "send_request", transport)
    monkeypatch.setattr(cli, "Engine", lambda *_args: (_ for _ in ()).throw(AssertionError()))

    assert cli.main(["--state-dir", str(state), "status", "--json"]) == 0
    report = json.loads(capsys.readouterr().out)
    assert report["mode"] == "degraded"
    assert calls == [("ping", cli.STATUS_DAEMON_TIMEOUT_SECONDS)] + (
        [("status", cli.STATUS_DAEMON_TIMEOUT_SECONDS)] if timeout_verb == "status" else []
    )


def test_status_actual_diagnostic_request_reports_a_dead_port_offline(
    tmp_path, monkeypatch, capsys
) -> None:
    """A refused ping is absence, unlike a ping that reaches its timeout budget."""
    from bosn.registry import Registry

    state = tmp_path / "state"
    with Registry(state / "registry.sqlite3"):
        pass
    monkeypatch.setattr(
        ipc,
        "send_request",
        lambda *_args, **_kwargs: (_ for _ in ()).throw(ipc.TransportError("connection refused")),
    )
    monkeypatch.setattr(cli, "Engine", lambda *_args: (_ for _ in ()).throw(AssertionError()))

    assert cli.main(["--state-dir", str(state), "status", "--json"]) == 0
    report = json.loads(capsys.readouterr().out)
    assert report["mode"] == "offline"
    assert report["daemon"]["reachable"] is False


def test_status_returns_an_offline_empty_session_report_without_engine_inventory(
    tmp_path, monkeypatch, capsys
) -> None:
    """#119: no daemon is still a bounded registry report, never an engine scan."""
    from bosn import daemon
    from bosn.registry import Registry

    state = tmp_path / "state"
    with Registry(state / "registry.sqlite3"):
        pass
    monkeypatch.setattr(
        daemon,
        "request",
        lambda *_args, **_kwargs: (_ for _ in ()).throw(daemon.DaemonError("no daemon")),
    )
    monkeypatch.setattr(cli, "Engine", lambda *_args: (_ for _ in ()).throw(AssertionError()))

    assert cli.main(["--state-dir", str(state), "status", "--json"]) == 0
    report = json.loads(capsys.readouterr().out)
    assert report["mode"] == "offline"
    assert report["registered"] == 0
    assert report["execution_sessions"] == []
    assert report["daemon"]["reachable"] is False


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


def test_tasks_reports_the_selected_custom_manifest_path(tmp_path, capsys) -> None:
    (tmp_path / "Dockerfile").write_text("FROM scratch\n", encoding="utf-8")
    custom = tmp_path / "development.toml"
    custom.write_text("[stack.dev]\ndockerfile = 'Dockerfile'\n", encoding="utf-8")
    # A decoy makes this fail before #126: the loaded custom manifest was displayed as
    # `<root>/bosn.toml`, despite that file neither selecting nor describing this stack.
    (tmp_path / "bosn.toml").write_text("[stack.decoy]\nimage = 'busybox'\n", encoding="utf-8")

    assert cli.main(["--manifest", str(custom), "tasks", "--json"]) == 0

    assert json.loads(capsys.readouterr().out)["manifest"] == str(custom.resolve())


@pytest.mark.parametrize(
    "args",
    [
        lambda custom: ["--manifest", str(custom), "ensure"],
        lambda custom: ["ensure", "--manifest", str(custom)],
    ],
    ids=["global-manifest", "verb-local-manifest"],
)
def test_ensure_forwards_custom_manifest_source_for_global_and_local_flags(
    tmp_path, monkeypatch, args
) -> None:
    from bosn.converge import ConvergeResult

    (tmp_path / "Dockerfile").write_text("FROM scratch\n", encoding="utf-8")
    custom = tmp_path / "development.toml"
    custom.write_text("[stack.dev]\ndockerfile = 'Dockerfile'\n", encoding="utf-8")
    (tmp_path / "bosn.toml").write_text("[stack.decoy]\nimage = 'busybox'\n", encoding="utf-8")
    seen: dict[str, object] = {}

    def converge(_opts, manifest, _stack):
        seen["manifest"] = manifest.path
        return ConvergeResult("dev", "sha256:custom", "reused", "image")

    monkeypatch.setattr(cli, "_converge_via_daemon", converge)
    assert cli.main(args(custom)) == 0
    assert seen["manifest"] == custom.resolve()


@pytest.mark.parametrize("verb", ["run", "shell"])
def test_foreground_verbs_forward_the_selected_custom_source_to_execution_acquire(
    verb, tmp_path, monkeypatch
) -> None:
    from bosn import daemon, resources
    from bosn.converge import ConvergeResult

    (tmp_path / "Dockerfile").write_text("FROM scratch\n", encoding="utf-8")
    custom = tmp_path / "development.toml"
    custom.write_text("[stack.dev]\ndockerfile = 'Dockerfile'\ndefault = true\n", encoding="utf-8")
    (tmp_path / "bosn.toml").write_text("[stack.decoy]\nimage = 'busybox'\n", encoding="utf-8")
    monkeypatch.setattr(
        cli,
        "_converge_via_daemon",
        lambda *_args: ConvergeResult("dev", "sha256:custom", "reused", "image"),
    )
    monkeypatch.setattr(resources, "process_start_time", lambda _pid: 1.0)
    seen: dict[str, object] = {}

    def request(requested_verb, *_args, **kwargs):
        if requested_verb == "execution-acquire":
            seen["manifest"] = kwargs["manifest"]
            return {"ok": True, "container": "container", "session": "session"}
        assert requested_verb == "execution-release"
        return {"ok": True}

    class Engine:
        def __init__(self, _binary="docker") -> None:
            pass

        def execute(self, _args, **_kwargs) -> int:
            return 0

        def interactive(self, _args) -> int:
            return 0

    monkeypatch.setattr(daemon, "request", request)
    monkeypatch.setattr(cli, "Engine", Engine)
    command = ["--manifest", str(custom), verb]
    if verb == "run":
        command.extend(["--", "true"])
    assert cli.main(command) == 0
    assert seen["manifest"] == str(custom.resolve())


def test_one_word_task_forwards_the_selected_custom_manifest_to_converge(
    tmp_path, monkeypatch
) -> None:
    from bosn import daemon, resources
    from bosn.converge import ConvergeResult

    (tmp_path / "Dockerfile").write_text("FROM scratch\n", encoding="utf-8")
    custom = tmp_path / "development.toml"
    custom.write_text(
        "[stack.dev]\ndockerfile = 'Dockerfile'\ndefault = true\n[task.unit]\ncmd = 'true'\n",
        encoding="utf-8",
    )
    (tmp_path / "bosn.toml").write_text("[stack.decoy]\nimage = 'busybox'\n", encoding="utf-8")
    seen: dict[str, object] = {}

    def converge(_opts, manifest, _stack):
        seen["converge"] = manifest.path
        return ConvergeResult("dev", "sha256:custom", "reused", "image")

    def request(verb, *_args, **_kwargs):
        if verb == "execution-acquire":
            return {"ok": True, "container": "container", "session": "session"}
        return {"ok": True}

    class Engine:
        def __init__(self, _binary="docker") -> None:
            pass

        def execute(self, _args, **_kwargs) -> int:
            return 0

    monkeypatch.setattr(cli, "_converge_via_daemon", converge)
    monkeypatch.setattr(resources, "process_start_time", lambda _pid: 1.0)
    monkeypatch.setattr(daemon, "request", request)
    monkeypatch.setattr(cli, "Engine", Engine)
    assert cli.main(["--manifest", str(custom), "unit"]) == 0
    assert seen["converge"] == custom.resolve()


def test_reconcile_volume_forwards_selected_custom_manifest_source(tmp_path, monkeypatch) -> None:
    from bosn import daemon

    (tmp_path / "Dockerfile").write_text("FROM scratch\n", encoding="utf-8")
    custom = tmp_path / "development.toml"
    custom.write_text(
        (
            "[stack.dev]\ndockerfile = 'Dockerfile'\n[stack.dev.volumes]\n"
            "target = { scope = 'spec' }\n"
        ),
        encoding="utf-8",
    )
    (tmp_path / "bosn.toml").write_text("[stack.decoy]\nimage = 'busybox'\n", encoding="utf-8")
    seen: dict[str, object] = {}

    def request(_verb, *_args, **kwargs):
        seen["manifest"] = kwargs["manifest"]
        return {"ok": True, "plan": {}}

    monkeypatch.setattr(daemon, "request", request)
    assert (
        cli.main(
            [
                "reconcile-volume",
                "--manifest",
                str(custom),
                "--stack",
                "dev",
                "--volume",
                "target",
            ]
        )
        == 0
    )
    assert seen["manifest"] == str(custom.resolve())


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


def _stub_gc_stream(monkeypatch, final: dict, *, progress: list | None = None, capture=None):
    """Stand in for the daemon's streamed `gc`: some progress, then one final event (#110)."""
    from bosn import daemon

    def stream(verb, *_args, **kwargs):
        if capture is not None:
            capture["verb"] = verb
            capture.update(kwargs)
        yield from (progress or [])
        yield {**final, "final": True}

    monkeypatch.setattr(daemon, "stream", stream)
    # `gc` must not fall back to the budgeted single-request path it was moved off of.
    monkeypatch.setattr(
        daemon,
        "request",
        lambda *a, **k: (_ for _ in ()).throw(AssertionError("gc must stream, not request")),
    )


def test_gc_json_daemon_error_has_stable_remedy(tmp_path, capsys, monkeypatch) -> None:
    from bosn import daemon

    def unavailable(*_args, **_kwargs):
        raise daemon.DaemonError("down")

    monkeypatch.setattr(daemon, "stream", unavailable)
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

    Now that the verb streams, a silent gap this long means the daemon stopped talking
    rather than that the collection is slow -- but the remedy must still not be the one
    that kills a collection which may yet be running.
    """
    from bosn import daemon

    def silent(*_args, **_kwargs):
        raise ipc.TransportTimeout("timed out waiting for the daemon")

    monkeypatch.setattr(daemon, "stream", silent)
    assert cli.main(["--state-dir", str(tmp_path), "gc", "--json"]) == 1
    payload = json.loads(capsys.readouterr().out)
    assert payload["code"] == "gc.timeout"
    assert "before restarting anything" in payload["next"]
    assert "restart the daemon, then retry" not in payload["next"]


def test_gc_streams_instead_of_asking_for_a_fixed_budget(tmp_path, monkeypatch, capsys) -> None:
    """#110's second half: the budget was the bug, not its size.

    #111 gave `gc` a measured 120-second request budget and a field host still ran past it
    with nothing wrong, leaving no report of a collection that was proceeding normally.
    A runtime that is a function of how much there is to delete cannot be covered by any
    constant, so the client now waits on the daemon still reporting progress. Asserting on
    the wiring rather than on a constant: `daemon.request` is stubbed to fail loudly, so a
    regression back to the single-request path cannot pass this quietly.
    """
    seen: dict[str, object] = {}
    _stub_gc_stream(
        monkeypatch,
        {"ok": True, "result": {"collected": [], "kept": []}},
        progress=[{"ok": True, "phase": "scanning engine resources", "elapsed_seconds": 5.0}],
        capture=seen,
    )
    # The repository checkout has its own bosn.toml. This case is specifically global GC
    # with no discoverable manifest, so its client cwd must be the empty temporary root.
    monkeypatch.chdir(tmp_path)

    assert cli.main(["--state-dir", str(tmp_path), "gc", "--dry-run"]) == 0

    assert seen["verb"] == "gc"
    assert seen["manifest"] is None
    assert "request_timeout" not in seen, "a streamed gc must not carry a total deadline"
    assert not hasattr(cli, "GC_REQUEST_TIMEOUT_SECONDS"), "the guessed budget must be gone"
    # Progress is visible while it works, and never on stdout, which carries only the report.
    captured = capsys.readouterr()
    assert "scanning engine resources" in captured.err
    assert "scanning engine resources" not in captured.out


def test_gc_progress_repeats_a_long_phase_only_occasionally(tmp_path, monkeypatch, capsys):
    """The daemon heartbeats for the transport; a person reads what reaches the terminal.

    A five-minute collection heartbeats roughly sixty times. Echoing each one would scroll
    the report those lines are introducing off the screen, so an unchanged phase repeats on
    its own slower interval while every phase *change* is always shown.
    """
    heartbeats = [
        {"ok": True, "phase": "scanning engine resources", "elapsed_seconds": float(seconds)}
        for seconds in range(5, 65, 5)
    ] + [{"ok": True, "phase": "planning collection", "elapsed_seconds": 70.0}]
    _stub_gc_stream(monkeypatch, {"ok": True, "result": {}, "errors": []}, progress=heartbeats)
    monkeypatch.chdir(tmp_path)

    assert cli.main(["--state-dir", str(tmp_path), "gc", "--dry-run"]) == 0

    lines = [line for line in capsys.readouterr().err.splitlines() if line.startswith("collecting")]
    assert len(lines) < len(heartbeats), "every heartbeat must not become a line"
    assert lines[0] == "collecting: scanning engine resources (5s)"
    # A phase change is never throttled away, however soon after the last line it lands.
    assert lines[-1] == "collecting: planning collection (70s)"


def test_gc_progress_stays_off_stdout_in_json_mode(tmp_path, monkeypatch, capsys) -> None:
    """A `--json` consumer parses stdout; progress may not reach it in any form."""
    _stub_gc_stream(
        monkeypatch,
        {"ok": True, "result": {}, "errors": []},
        progress=[{"ok": True, "phase": "planning collection", "elapsed_seconds": 9.0}],
    )
    monkeypatch.chdir(tmp_path)

    assert cli.main(["--state-dir", str(tmp_path), "gc", "--json"]) == 0

    captured = capsys.readouterr()
    json.loads(captured.out)  # exactly one parseable report, no progress interleaved
    assert "planning collection" not in captured.out
    assert "planning collection" not in captured.err


def test_gc_passes_an_available_manifest_for_exact_collision_diagnostics(
    tmp_path, monkeypatch
) -> None:

    manifest = tmp_path / "bosn.toml"
    manifest.write_text("[stack.perf]\nimage = 'alpine:3.20'\n", encoding="utf-8")
    seen: dict[str, object] = {}

    _stub_gc_stream(monkeypatch, {"ok": True, "result": {}, "errors": []}, capture=seen)
    assert cli.main(["--manifest", str(manifest), "gc", "--json"]) == 0
    assert seen["verb"] == "gc"
    assert seen["manifest"] == str(manifest.resolve())


def test_gc_canonicalizes_a_relative_custom_manifest_before_daemon_ipc(
    tmp_path, monkeypatch
) -> None:

    custom = tmp_path / "development.toml"
    custom.write_text("[stack.perf]\nimage = 'alpine:3.20'\n", encoding="utf-8")
    # The source path, not this default-named sibling, is the daemon's exact collision
    # discriminator. Running from the client cwd makes the old relative-path handoff
    # observably unsafe for a daemon with another cwd.
    (tmp_path / "bosn.toml").write_text("[stack.decoy]\nimage = 'busybox'\n", encoding="utf-8")
    seen: dict[str, object] = {}

    monkeypatch.chdir(tmp_path)
    _stub_gc_stream(monkeypatch, {"ok": True, "result": {}, "errors": []}, capture=seen)
    assert cli.main(["--manifest", "development.toml", "gc", "--json"]) == 0
    assert seen["manifest"] == str(custom.resolve())


def test_gc_json_reports_images_deferred_by_container_dependencies(
    tmp_path, capsys, monkeypatch
) -> None:

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

    _stub_gc_stream(monkeypatch, deferred())

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

        def execute_capture(self, *_args, **_kwargs):
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


def test_doctor_reports_a_desktop_wedge_with_local_read_only_inventory(
    tmp_path, monkeypatch, capsys
) -> None:
    """#136 preserves local evidence but never turns an engine failure into authority."""
    import hashlib

    from bosn import accounting
    from bosn.engine import DesktopEvidence, EngineInfo
    from bosn.registry import Registry

    with Registry(tmp_path / "registry.sqlite3") as registry:
        resource = registry.register_resource(
            kind="volume",
            name="warm-cache",
            stack="dev",
            generation="digest",
            scope="stack",
            workspace="workspace",
        )
        registry.acquire_lease(resource.id, pid=123, proc_start=1.0)
    database = tmp_path / "registry.sqlite3"
    before = hashlib.sha256(database.read_bytes()).hexdigest()

    class WedgeEngine:
        def __init__(self, binary: str = "docker") -> None:
            self.binary = binary

        def info(self):
            return EngineInfo(
                binary=self.binary,
                reachable=False,
                client_version="28.5.1",
                detail="request returned 500 Internal Server Error",
                failure_category="docker_desktop_wedged",
                desktop_evidence=DesktopEvidence(True, True),
            )

    class ScannerMustNotRun:
        def __init__(self, *_args) -> None:
            raise AssertionError("doctor must not scan a wedged engine")

    monkeypatch.setattr(cli, "Engine", WedgeEngine)
    monkeypatch.setattr("bosn.resources.ResourceScanner", ScannerMustNotRun)
    monkeypatch.setattr(
        accounting,
        "configured_desktop_vhdx_allocation",
        lambda: accounting.VhdxAllocation(tmp_path / "docker_data.vhdx", 382 * 1024**3),
    )

    assert cli.main(["--state-dir", str(tmp_path), "doctor"]) == 1
    captured = capsys.readouterr()
    assert "Docker Desktop engine appears wedged" in captured.err
    assert "restart Docker Desktop" in captured.err
    assert "engine resource inventory: unavailable" in captured.out
    assert "Docker Desktop observation: running" in captured.out
    assert "docker-desktop WSL observation: running" in captured.out
    assert "local registered resources: 1" in captured.out
    assert "local leases: 1" in captured.out
    assert "382.0 GiB allocated" in captured.out
    assert "configured VHDX volume free space:" in captured.out
    assert "prune" not in captured.err.lower()
    assert "adopt" not in captured.err.lower()
    assert "compact" not in captured.err.lower()
    assert hashlib.sha256(database.read_bytes()).hexdigest() == before


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


def _stub_engine_with_a_timing_out_listing(monkeypatch, *, foreign_volumes: int = 0):
    """A reachable engine whose `docker images` blows its deadline, as #117 reported it.

    Deliberately not a stubbed `ResourceScanner`: the whole bug is that the real scanner
    lets `EngineError` escape `scan()`, so the test has to drive the real scanner and fail
    at the engine boundary the way a loaded Docker Desktop host does. Returns the recorded
    command list so a test can prove the failure path stayed read-only.
    """
    from bosn import labels
    from bosn.engine import EngineError, EngineInfo, EngineResult

    commands: list[list[str]] = []

    def _foreign_row(index: int) -> str:
        raw = labels.ResourceLabels(
            registry="lost-registry",
            kind="volume",
            stack="dev",
            generation="digest",
            scope="spec",
            workspace="workspace",
            created="2026-01-01T00:00:00Z",
        ).to_dict()
        return json.dumps({"Name": f"warm-cache-{index}", "Labels": json.dumps(raw)})

    class TimingOutEngine:
        def __init__(self, binary: str = "docker") -> None:
            self.binary = binary

        def info(self):
            return EngineInfo(
                binary=self.binary,
                reachable=True,
                client_version="28.5.1",
                server_version="28.5.1",
                clock_skew_seconds=0.0,
            )

        def run(self, args, *, check: bool = False, timeout: float | None = None):
            commands.append(list(args))
            if args[0] == "images":
                raise EngineError(
                    "docker images --no-trunc --format {{json .}} exceeded its 60-second deadline"
                )
            if args[0] == "volume":
                rows = [_foreign_row(i) for i in range(foreign_volumes)]
                return EngineResult(0, "\n".join(rows), "")
            return EngineResult(0, "", "")

    monkeypatch.setattr(cli, "Engine", TimingOutEngine)
    return commands


def test_doctor_reports_an_incomplete_inventory_instead_of_a_traceback(
    tmp_path, monkeypatch, capsys
) -> None:
    """#117: only the resource-inventory phase failed, so only that phase may be lost."""
    import hashlib

    from bosn.registry import Registry

    with Registry(tmp_path / "registry.sqlite3") as registry:
        registry.register_resource(
            kind="volume",
            name="warm-cache",
            stack="dev",
            generation="digest",
            scope="stack",
            workspace="workspace",
        )
    database = tmp_path / "registry.sqlite3"
    before = hashlib.sha256(database.read_bytes()).hexdigest()
    commands = _stub_engine_with_a_timing_out_listing(monkeypatch)

    code = cli.main(["--state-dir", str(tmp_path), "doctor"])

    assert code == 1, "a partial scan must never read as a healthy engine"
    captured = capsys.readouterr()
    # Everything doctor had already established stays on the report.
    assert "client version: 28.5.1" in captured.out
    assert "registry integrity: ok" in captured.out
    assert "docker shims:" in captured.out
    # ...and the phase that failed says so, naming the kind and the reason.
    assert "engine resource inventory: incomplete" in captured.out
    assert "image" in captured.out
    assert "60-second deadline" in captured.out
    assert "Traceback" not in captured.err
    assert "EngineError" not in captured.err
    # Read-only: no engine mutation, no registry mutation.
    assert not [c for c in commands if {"rm", "prune", "rmi", "kill", "stop"} & set(c)]
    assert hashlib.sha256(database.read_bytes()).hexdigest() == before


def test_doctor_withholds_adoption_guidance_when_the_scan_is_incomplete(
    tmp_path, monkeypatch, capsys
) -> None:
    """Absence of evidence is not evidence of absence: half a scan cannot advise adoption."""
    _stub_engine_with_a_timing_out_listing(monkeypatch, foreign_volumes=3)

    code = cli.main(["--state-dir", str(tmp_path), "doctor"])

    assert code == 1
    captured = capsys.readouterr()
    assert "adopt --from-registry" not in captured.err
    assert "adopt --from-registry" not in captured.out
    assert "incomplete" in captured.out


def test_doctor_json_reports_an_incomplete_scan_as_one_envelope(
    tmp_path, monkeypatch, capsys
) -> None:
    _stub_engine_with_a_timing_out_listing(monkeypatch)

    assert cli.main(["--json", "--state-dir", str(tmp_path), "doctor"]) == 1

    payload = json.loads(capsys.readouterr().out)
    assert payload["ok"] is False
    assert payload["code"] == "command.failed"
    assert "engine resource inventory: incomplete" in payload["message"]


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


def test_run_explains_a_closed_stream_instead_of_only_asserting_it(tmp_path, monkeypatch, capsys):
    """#134: `daemon closed the stream before the job ended` said nothing about why.

    The reporter was left with a state directory holding a heartbeat, a registry, a secret
    and an empty startup log, and no bounded diagnostic tying any of it to the failure.
    """
    from bosn import daemon as daemon_mod

    manifest = tmp_path / "bosn.toml"
    manifest.write_text("[stack.linux]\nimage = 'alpine:3.20'\n", encoding="utf-8")
    daemon_mod.DaemonState(
        pid=999_999, port=daemon_mod.port_for(tmp_path), started_at=1.0, version="0.0.1-old"
    ).write(daemon_mod.state_file(tmp_path))

    def closed_stream(*_args, **_kwargs):
        raise ipc.TransportError("daemon closed the stream before the job ended")

    monkeypatch.setattr(daemon_mod, "stream", closed_stream)
    monkeypatch.setattr(daemon_mod, "request", closed_stream)

    code = cli.main(
        [
            "--state-dir",
            str(tmp_path),
            "--manifest",
            str(manifest),
            "run",
            "--stack",
            "linux",
            "true",
        ]
    )

    assert code == 1
    err = capsys.readouterr().err
    assert "daemon closed the stream before the job ended" in err
    assert "control-plane diagnostics" in err
    assert "version=0.0.1-old" in err, "the daemon version the client could not otherwise see"
    assert "process absent" in err
    assert "version skew" in err


def test_degraded_status_reports_the_daemon_identity_it_could_not_reach(
    tmp_path, monkeypatch, capsys
) -> None:
    """A skew refusal names a daemon version; `status` must be able to name it too (#134)."""
    from bosn import daemon as daemon_mod
    from bosn.registry import Registry

    with Registry(tmp_path / "registry.sqlite3"):
        pass
    daemon_mod.DaemonState(
        pid=999_999, port=daemon_mod.port_for(tmp_path), started_at=1.0, version="0.0.1-old"
    ).write(daemon_mod.state_file(tmp_path))
    monkeypatch.setattr(
        daemon_mod,
        "request",
        lambda *a, **k: (_ for _ in ()).throw(ipc.TransportTimeout("no answer")),
    )

    assert cli.main(["--state-dir", str(tmp_path), "status"]) == 0

    payload = json.loads(capsys.readouterr().out)
    recorded = payload["daemon"]["recorded"]
    assert payload["daemon"]["reachable"] is False
    assert recorded["version"] == "0.0.1-old"
    assert recorded["pid"] == 999_999
    assert recorded["process_alive"] is False
    assert recorded["version_skew"] is True
    assert recorded["client_version"] == cli.__version__


def test_status_reports_no_recorded_daemon_rather_than_inventing_one(tmp_path, monkeypatch, capsys):
    """An empty state directory must read as "none", never as a daemon with unknown fields."""
    from bosn import daemon as daemon_mod
    from bosn.registry import Registry

    with Registry(tmp_path / "registry.sqlite3"):
        pass
    monkeypatch.setattr(
        daemon_mod,
        "request",
        lambda *a, **k: (_ for _ in ()).throw(daemon_mod.DaemonError("no bosn daemon is running")),
    )

    assert cli.main(["--state-dir", str(tmp_path), "status"]) == 0

    assert json.loads(capsys.readouterr().out)["daemon"]["recorded"] is None


def test_status_stays_bounded_against_a_listener_that_never_answers(tmp_path, capsys) -> None:
    """#134's other half: after the restart attempt, control commands hung past 30 seconds.

    A *closed* port fails instantly and proves nothing. The failure mode that hangs is a
    socket that accepts the connection and then says nothing -- a wedged daemon, or a
    half-started one -- so this test is the accepting-and-silent listener, and it asserts
    on the wall clock rather than on a constant. Anything that reintroduces an unbounded
    control-plane read fails here regardless of which timeout it forgot.
    """
    import socket
    import threading
    import time as _time

    from bosn import daemon as daemon_mod
    from bosn.registry import Registry

    with Registry(tmp_path / "registry.sqlite3"):
        pass
    daemon_mod.DaemonState(
        pid=999_999, port=daemon_mod.port_for(tmp_path), started_at=1.0, version="0.0.1-old"
    ).write(daemon_mod.state_file(tmp_path))

    listener = socket.socket()
    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    listener.bind(("127.0.0.1", daemon_mod.port_for(tmp_path)))
    listener.listen(8)
    held: list[socket.socket] = []
    stop = threading.Event()

    def accept_and_say_nothing() -> None:
        listener.settimeout(0.2)
        while not stop.is_set():
            try:
                conn, _ = listener.accept()
            except (TimeoutError, OSError):
                continue
            held.append(conn)  # deliberately never written to

    accepter = threading.Thread(target=accept_and_say_nothing, daemon=True)
    accepter.start()
    try:
        started = _time.monotonic()
        code = cli.main(["--state-dir", str(tmp_path), "status"])
        elapsed = _time.monotonic() - started
    finally:
        stop.set()
        accepter.join(timeout=5)
        for conn in held:
            conn.close()
        listener.close()

    assert code == 0
    assert elapsed < 15.0, f"status must stay bounded against a silent daemon; took {elapsed:.1f}s"
    payload = json.loads(capsys.readouterr().out)
    # A listener that accepts and never answers is a *degraded* control plane, not an
    # absent one -- and the recorded identity is what makes it diagnosable.
    assert payload["mode"] == "degraded"
    assert payload["daemon"]["reachable"] is False
    assert payload["daemon"]["recorded"]["version"] == "0.0.1-old"
