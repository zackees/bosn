"""Phase 3: daemon singleton lifecycle.

The deterministic loopback port is the singleton concurrency primitive: a second daemon
fails its bind with EADDRINUSE, so the operating system is the arbiter. These tests pin
that property along with spawn, heartbeat, idle retirement, and fail-closed behavior.

No Docker involved -- these run on every platform.
"""

from __future__ import annotations

import os
import socket
import subprocess
import sys
import threading
import time
from collections.abc import Iterator
from pathlib import Path
from types import SimpleNamespace

import pytest

from bosn import daemon as daemon_mod
from bosn import ipc
from bosn.clock import FakeClock
from bosn.daemon import Daemon, DaemonError, DaemonState
from bosn.engine import EngineError, EngineInfo
from bosn.registry import Registry


def _detach_code() -> str:
    """`_detach`'s body with its docstring stripped, so prose cannot satisfy an assertion."""
    import ast
    import inspect
    import textwrap

    tree = ast.parse(textwrap.dedent(inspect.getsource(daemon_mod._detach)))
    func = tree.body[0]
    assert isinstance(func, ast.FunctionDef)
    body = func.body
    if (
        body
        and isinstance(body[0], ast.Expr)
        and isinstance(body[0].value, ast.Constant)
        and isinstance(body[0].value.value, str)
    ):
        body = body[1:]
    return "\n".join(ast.unparse(node) for node in body)


# 60s rather than 15s -- a budget increase, not race-hiding padding. `is_serving` is an IPC
# ping with its own 2s timeout, so under CPU oversubscription each failed attempt burns 2s
# and a 15s ceiling is only ~7 tries; a busy stretch then fails on attempt count rather than
# on anything being broken. Polling means a healthy run returns immediately regardless, so
# the higher ceiling only changes how long a genuine hang takes to report. Same reasoning,
# at more length, on `wait_until` in test_daemon_jobs.py (issue #95).
_WAIT_TIMEOUT = 60.0


def _wait_until(predicate, timeout: float = _WAIT_TIMEOUT, interval: float = 0.05) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if predicate():
            return True
        time.sleep(interval)
    return predicate()


@pytest.fixture
def served(tmp_path: Path) -> Iterator[Daemon]:
    daemon = Daemon(state_dir=tmp_path, idle_retire_seconds=3600)
    # A fresh registry has no stored maintenance deadline, so `_next_maintenance_at`
    # defaults to `started_at` -- due on the watchdog's very first tick. `shutdown()`
    # joins the watchdog thread with no timeout at all, so if `request_stop()` below lands
    # while that first tick is mid-`_run_maintenance()`, teardown blocks until the pass
    # finishes -- including its engine-reachability probe, which is 60s per call and is
    # called twice on an engine-less/unreachable runner (see
    # test_idle_retirement_stops_an_unused_daemon below). That race, not a fixed-budget
    # timing issue, is what made `thread.join(timeout=15)` occasionally fail under load
    # (see issue #95). Nothing in this fixture exercises maintenance, so it is pushed out
    # of the way for every test that uses `served`.
    daemon._set_next_maintenance(daemon.clock.now() + 3600)
    thread = threading.Thread(target=daemon.serve_forever, daemon=True)
    thread.start()
    assert _wait_until(lambda: daemon_mod.is_serving(tmp_path)), "daemon never came up"
    try:
        yield daemon
    finally:
        daemon.request_stop()
        thread.join(timeout=15)
        daemon.shutdown()


# -- the port as singleton primitive ---------------------------------------


def test_port_is_deterministic_not_random(tmp_path: Path) -> None:
    assert daemon_mod.port_for(tmp_path) == daemon_mod.port_for(tmp_path)
    other = tmp_path / "elsewhere"
    other.mkdir()
    assert daemon_mod.port_for(other) != daemon_mod.port_for(tmp_path)


def test_disconnected_non_streaming_response_does_not_escape_handler(
    served: Daemon, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A client timing out on a large GC response cannot take down the daemon (#120)."""
    handler = object.__new__(daemon_mod._Handler)
    handler.connection = object()
    handler.server = SimpleNamespace(daemon_ref=served)  # type: ignore[assignment]
    monkeypatch.setattr(ipc, "read_request", lambda _conn: {"auth": served.secret, "verb": "gc"})
    monkeypatch.setattr(served, "dispatch", lambda *_args: {"ok": True, "unproven_resources": [{}]})
    real_send_response = ipc.send_response
    monkeypatch.setattr(
        ipc, "send_response", lambda *_args: (_ for _ in ()).throw(OSError("reset"))
    )

    handler.handle()

    monkeypatch.setattr(ipc, "send_response", real_send_response)
    assert _wait_until(lambda: daemon_mod.is_serving(served.state_dir)), "daemon stopped serving"
    assert any(row["kind"] == "ipc.response_disconnected" for row in served.registry.events())


def test_reconcile_volume_version_mismatch_is_rejected_before_dispatch(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Even a preview must match: the same verb may later perform an apply."""

    class NoRegistryMutation:
        def log_event(self, *_args: object) -> None:
            raise AssertionError("the version gate must not mutate the registry")

    class NoDispatch:
        secret = "test-secret"
        registry = NoRegistryMutation()

        def note_activity(self) -> None:
            return None

        def dispatch(self, *_args: object) -> dict[str, object]:
            raise AssertionError("the version gate must reject before dispatch or engine use")

    handler = object.__new__(daemon_mod._Handler)
    handler.connection = object()
    handler.server = SimpleNamespace(daemon_ref=NoDispatch())  # type: ignore[assignment]
    responses: list[dict[str, object]] = []
    monkeypatch.setattr(
        ipc,
        "read_request",
        lambda _conn: {
            "auth": "test-secret",
            "verb": "reconcile-volume",
            "version": "a-client-version-that-does-not-match",
            # Intentionally omit apply: preview is gated with the mutating verb too.
        },
    )
    monkeypatch.setattr(ipc, "send_response", lambda _conn, reply: responses.append(reply))

    handler.handle()

    assert responses == [
        {
            "ok": False,
            "error": (
                "bosn client/daemon version mismatch: client="
                "a-client-version-that-does-not-match daemon="
                + daemon_mod.__version__
                + "; restart the daemon before destructive use"
            ),
            "client_version": "a-client-version-that-does-not-match",
            "daemon_version": daemon_mod.__version__,
        }
    ]


def test_default_state_dir_uses_the_fixed_default_port(monkeypatch) -> None:
    monkeypatch.delenv("BOSN_PORT", raising=False)
    from bosn.registry import default_state_dir

    assert daemon_mod.port_for(default_state_dir()) == daemon_mod.DEFAULT_PORT


def test_port_can_be_overridden_by_env(monkeypatch, tmp_path: Path) -> None:
    monkeypatch.setenv("BOSN_PORT", "50123")
    assert daemon_mod.port_for(tmp_path) == 50123


def test_second_daemon_loses_the_bind_and_refuses_to_start(served: Daemon, tmp_path: Path) -> None:
    """The singleton check IS the failed bind -- not a pid comparison, not a lock file."""
    with pytest.raises(DaemonError, match="already owns port"):
        Daemon(state_dir=tmp_path).serve_forever()


def test_a_foreign_listener_on_the_port_also_blocks_startup(tmp_path: Path) -> None:
    """Anything holding the port wins; the daemon never assumes the port is free."""
    port = daemon_mod.port_for(tmp_path)
    squatter = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    try:
        squatter.bind((ipc.LOOPBACK, port))
        squatter.listen(1)
        with pytest.raises(DaemonError, match="already owns port"):
            Daemon(state_dir=tmp_path).serve_forever()
    finally:
        squatter.close()


def test_a_stale_state_file_cannot_fake_a_live_daemon(tmp_path: Path) -> None:
    DaemonState(
        pid=os.getpid(), port=daemon_mod.port_for(tmp_path), started_at=0.0, version="0"
    ).write(daemon_mod.state_file(tmp_path))
    # nothing is listening, so liveness is false regardless of what the file claims
    assert daemon_mod.running_state(tmp_path) is None
    assert not daemon_mod.state_file(tmp_path).exists()


# -- serving ---------------------------------------------------------------


def test_daemon_publishes_state_and_answers_ping(served: Daemon, tmp_path: Path) -> None:
    state = daemon_mod.running_state(tmp_path)
    assert state is not None
    assert state.port == served.port == daemon_mod.port_for(tmp_path)
    assert state.pid == os.getpid()
    reply = ipc.send_request(state.port, {"verb": "ping", "auth": served.secret})
    assert reply["ok"] and reply["pong"]
    assert daemon_mod.heartbeat_file(tmp_path).exists()


def test_status_reports_registry_id_and_uptime(served: Daemon) -> None:
    reply = ipc.send_request(served.port, {"verb": "status", "auth": served.secret})
    assert reply["ok"]
    assert reply["registry_id"] == served.registry.registry_id
    assert reply["uptime_seconds"] >= 0


def test_unknown_verb_is_rejected_not_ignored(served: Daemon) -> None:
    reply = ipc.send_request(served.port, {"verb": "definitely-not-a-verb", "auth": served.secret})
    assert reply["ok"] is False
    assert "unknown daemon verb" in reply["error"]


def test_done_normalizes_the_request_workspace_before_matching(tmp_path: Path) -> None:
    """The IPC contract must accept any supported spelling, not only the CLI's own.

    `_verb_done` used to pass `request["workspace"]` straight into an exact-match SQL
    query. The bundled CLI always sends an already-normalized identity, so this never
    showed up there -- but any other IPC client sending an equivalent-but-differently-
    spelled path (MSYS, WSL, a trailing slash, drive-letter case) would match zero rows
    and get back `{"ok": True, "marked": 0}`: a silent no-op reported as success. This
    proves a request spelled differently from the stored identity still marks it.
    """
    from bosn.paths import normalize_workspace_path

    daemon = Daemon(state_dir=tmp_path)
    try:
        canonical = normalize_workspace_path(r"C:\Users\Me\work")
        daemon.registry.register_resource(
            kind="volume",
            name="cache",
            stack="s",
            generation="g",
            scope="spec",
            workspace=canonical,
        )
        differently_spelled = "/c/Users/Me/work"
        assert differently_spelled != canonical

        reply = daemon.dispatch("done", {"workspace": differently_spelled})

        assert reply == {"ok": True, "marked": 1}
    finally:
        daemon.registry.close()


def test_requests_refresh_the_heartbeat(served: Daemon) -> None:
    before = served.heartbeat_at
    time.sleep(0.05)
    ipc.send_request(served.port, {"verb": "ping"})
    assert served.heartbeat_at > before


def test_registry_is_usable_from_the_server_threads(served: Daemon) -> None:
    """The daemon serves on threads; a thread-affine sqlite handle would fail here."""
    for _ in range(5):
        assert ipc.send_request(served.port, {"verb": "status", "auth": served.secret})["ok"]


# -- shutdown and retirement -----------------------------------------------


def test_shutdown_verb_stops_the_daemon_and_clears_state(tmp_path: Path) -> None:
    daemon = Daemon(state_dir=tmp_path, idle_retire_seconds=3600)
    # See the `served` fixture above: a due first-tick maintenance pass can make
    # `shutdown()`'s untimed `watchdog.join()` stall for up to ~2 minutes (two 60s engine
    # probes) if `daemon_mod.stop()` below lands mid-pass. This is the test that was
    # actually observed flaking that way (issue #95); nothing here exercises maintenance,
    # so it is pushed out of the way.
    daemon._set_next_maintenance(daemon.clock.now() + 3600)
    thread = threading.Thread(target=daemon.serve_forever, daemon=True)
    thread.start()
    assert _wait_until(lambda: daemon_mod.is_serving(tmp_path))

    assert daemon_mod.stop(tmp_path) is True
    thread.join(timeout=15)
    assert not daemon_mod.is_serving(tmp_path)
    assert not daemon_mod.state_file(tmp_path).exists()
    assert not daemon_mod.heartbeat_file(tmp_path).exists()


def test_the_port_is_released_for_the_next_daemon(tmp_path: Path) -> None:
    """A retired daemon must not leave its port wedged, or the singleton becomes a lockout."""
    for _ in range(2):
        daemon = Daemon(state_dir=tmp_path, idle_retire_seconds=3600)
        # Prophylactic, not observed failing: same untimed-`watchdog.join()`-vs-first-tick-
        # maintenance race as `test_shutdown_verb_stops_the_daemon_and_clears_state` above.
        daemon._set_next_maintenance(daemon.clock.now() + 3600)
        thread = threading.Thread(target=daemon.serve_forever, daemon=True)
        thread.start()
        assert _wait_until(lambda: daemon_mod.is_serving(tmp_path))
        assert daemon_mod.stop(tmp_path)
        thread.join(timeout=15)


def test_idle_retirement_stops_an_unused_daemon(tmp_path: Path) -> None:
    """An unused daemon retires itself.

    The maintenance deadline is pushed out of the way first because `_idle_watchdog`
    runs `run_maintenance_if_due()` in the same 0.5s tick that checks retirement, and a
    due maintenance pass probes the engine twice at `engine.DEFAULT_TIMEOUT` (60s) each.
    On a host where the docker binary is present but its daemon is unreachable -- every
    hosted Windows/macOS runner -- that blocks the tick for up to two minutes and this
    assertion times out having measured engine probing rather than idle retirement.
    """
    # idle_retire_seconds starts long, not the 0.5s this test is actually about. Arming the
    # short window at construction races the watchdog's first tick (also ~0.5s) against
    # nothing more than "the test thread gets scheduled and lands one IPC round trip" --
    # under load that race can be lost: the daemon retires before anyone ever contacts it,
    # and the `is_serving` wait below then fails forever pinging something already gone
    # (issue #95; see test_a_running_job_blocks_idle_retirement in test_daemon_jobs.py for
    # the same fix and a 4/40-under-load confirmation this actually happens). The short
    # window is armed only once the daemon is confirmed up, so "unused" is measured from a
    # point the test can actually observe, which is what this test means by it anyway.
    daemon = Daemon(state_dir=tmp_path, idle_retire_seconds=3600)
    daemon._set_next_maintenance(daemon.clock.now() + 3600)
    thread = threading.Thread(target=daemon.serve_forever, daemon=True)
    thread.start()
    assert _wait_until(lambda: daemon_mod.is_serving(tmp_path))
    daemon.idle_retire_seconds = 0.5
    thread.join(timeout=30)
    assert not thread.is_alive(), "idle daemon should have retired itself"
    assert not daemon_mod.is_serving(tmp_path)


def test_active_execution_session_pins_daemon_and_refuses_shutdown(tmp_path: Path) -> None:
    daemon = Daemon(state_dir=tmp_path, idle_retire_seconds=0)
    try:
        daemon._execution_sessions["session"] = ("lease",)
        assert not daemon.should_retire()
        reply = daemon._verb_shutdown({})
        assert reply["ok"] is False
        assert "active execution" in reply["error"]
    finally:
        daemon.registry.close()


# -- Compose project leases (#48) -------------------------------------------


def test_compose_acquire_leases_every_resource_registered_for_the_workspace(
    tmp_path: Path,
) -> None:
    daemon = Daemon(state_dir=tmp_path)
    try:
        workspace = str(tmp_path / "proj")
        container = daemon.registry.register_resource(
            kind="container",
            name="app",
            stack="app",
            generation="g",
            scope="stack",
            workspace=workspace,
        )
        volume = daemon.registry.register_resource(
            kind="volume",
            name="data",
            stack="data",
            generation="g",
            scope="stack",
            workspace=workspace,
        )
        # A resource from an unrelated workspace must not be swept up in the lease.
        other = daemon.registry.register_resource(
            kind="container",
            name="other",
            stack="other",
            generation="g",
            scope="stack",
            workspace=str(tmp_path / "unrelated"),
        )

        reply = daemon.dispatch(
            "compose-acquire", {"workspace": workspace, "pid": 4242, "proc_start": 100.0}
        )

        assert reply["ok"] is True
        assert reply["leased"] == 2
        leased_ids = {lease.resource_id for lease in daemon.registry.all_leases()}
        assert leased_ids == {container.id, volume.id}
        assert not daemon.registry.leases_for(other.id)
    finally:
        daemon.registry.close()


def test_compose_acquire_holds_the_lease_under_the_callers_identity_not_the_daemons(
    tmp_path: Path,
) -> None:
    """Diverges from `execution-acquire` on purpose: see the docstring on
    `_verb_compose_acquire`. A daemon-held lease (`pid=os.getpid()` inside the daemon)
    survives a SIGKILLed client forever; a client-held one expires within one TTL.
    """
    daemon = Daemon(state_dir=tmp_path)
    try:
        workspace = str(tmp_path / "proj")
        resource = daemon.registry.register_resource(
            kind="container",
            name="app",
            stack="app",
            generation="g",
            scope="stack",
            workspace=workspace,
        )
        client_pid = 999999
        assert client_pid != os.getpid()

        reply = daemon.dispatch(
            "compose-acquire",
            {"workspace": workspace, "pid": client_pid, "proc_start": 55.5},
        )

        assert reply["ok"] is True
        (lease,) = daemon.registry.leases_for(resource.id)
        assert lease.pid == client_pid
        assert lease.proc_start == 55.5
    finally:
        daemon.registry.close()


def test_compose_acquire_normalizes_the_request_workspace_before_matching(
    tmp_path: Path,
) -> None:
    from bosn.paths import normalize_workspace_path

    daemon = Daemon(state_dir=tmp_path)
    try:
        canonical = normalize_workspace_path(r"C:\Users\Me\proj")
        resource = daemon.registry.register_resource(
            kind="container",
            name="app",
            stack="app",
            generation="g",
            scope="stack",
            workspace=canonical,
        )
        differently_spelled = "/c/Users/Me/proj"
        assert differently_spelled != canonical

        reply = daemon.dispatch(
            "compose-acquire",
            {"workspace": differently_spelled, "pid": 111, "proc_start": None},
        )

        assert reply["ok"] is True
        assert reply["leased"] == 1
        assert daemon.registry.leases_for(resource.id)
    finally:
        daemon.registry.close()


def test_compose_acquire_accepts_a_null_proc_start(tmp_path: Path) -> None:
    """`process_start_time()` can return None; the schema must accept it rather than force
    a wall-clock substitute (see the comment on `_own_process_start_time` in converge.py
    about why that guess is dangerous).
    """
    daemon = Daemon(state_dir=tmp_path)
    try:
        workspace = str(tmp_path / "proj")
        resource = daemon.registry.register_resource(
            kind="container",
            name="app",
            stack="app",
            generation="g",
            scope="stack",
            workspace=workspace,
        )

        reply = daemon.dispatch(
            "compose-acquire", {"workspace": workspace, "pid": 111, "proc_start": None}
        )

        assert reply["ok"] is True
        (lease,) = daemon.registry.leases_for(resource.id)
        assert lease.proc_start is None
    finally:
        daemon.registry.close()


def test_compose_release_frees_the_leases_and_the_session_no_longer_pins_the_daemon(
    tmp_path: Path,
) -> None:
    daemon = Daemon(state_dir=tmp_path, idle_retire_seconds=0)
    try:
        workspace = str(tmp_path / "proj")
        resource = daemon.registry.register_resource(
            kind="container",
            name="app",
            stack="app",
            generation="g",
            scope="stack",
            workspace=workspace,
        )
        acquired = daemon.dispatch(
            "compose-acquire", {"workspace": workspace, "pid": 111, "proc_start": None}
        )
        assert not daemon.should_retire()

        released = daemon.dispatch("compose-release", {"session": acquired["session"]})

        assert released == {"ok": True, "released": 1}
        assert not daemon.registry.leases_for(resource.id)
        assert daemon.should_retire()
    finally:
        daemon.registry.close()


def test_compose_release_of_an_unknown_session_is_ok_not_an_error(tmp_path: Path) -> None:
    """A failed `compose-acquire` never hands the client a session, so its own `finally`
    calls release with nothing to release. That must not surface as a failure.
    """
    daemon = Daemon(state_dir=tmp_path)
    try:
        reply = daemon.dispatch("compose-release", {"session": "no-such-session"})
        assert reply == {"ok": True, "released": 0}
    finally:
        daemon.registry.close()


def test_a_leased_compose_resource_survives_pressure_eviction(tmp_path: Path) -> None:
    """The whole reason this lease exists: pressure eviction ignores age for non-machine
    resources, so a volume `compose up` just created is otherwise eligible immediately.
    """
    from bosn.retention import Pressure, evaluate

    daemon = Daemon(state_dir=tmp_path)
    try:
        workspace = str(tmp_path / "proj")
        volume = daemon.registry.register_resource(
            kind="volume",
            name="data",
            stack="data",
            generation="g",
            scope="stack",
            workspace=workspace,
        )
        # Freshly created: far younger than any age-based tier, so only pressure -- not
        # age -- could possibly explain a collect verdict here.
        under_pressure = Pressure(under_pressure=True, bytes_exceeded=True)
        unleased_verdict = evaluate(daemon.registry, volume, pressure=under_pressure)
        assert unleased_verdict.collect is True, "sanity: pressure alone collects this"

        acquired = daemon.dispatch(
            "compose-acquire", {"workspace": workspace, "pid": 111, "proc_start": None}
        )
        assert acquired["ok"] is True

        leased_verdict = evaluate(daemon.registry, volume, pressure=under_pressure)

        assert leased_verdict.collect is False
        assert leased_verdict.reason == "leased"
    finally:
        daemon.registry.close()


def test_a_failed_acquire_does_not_leave_a_session_pinning_the_daemon_forever(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The session is reserved before the lease loop runs (so shutdown sees it immediately),
    but if `acquire_lease` then raises -- e.g. a locked registry under concurrent writers --
    the caller never receives a session id back and can never call compose-release. Leaving
    the reservation behind would pin the daemon (`should_retire`/`request_stop` both key off
    `_execution_sessions`) exactly as permanently as the leak leases exist to prevent.
    """
    daemon = Daemon(state_dir=tmp_path, idle_retire_seconds=0)
    try:
        workspace = str(tmp_path / "proj")
        daemon.registry.register_resource(
            kind="container",
            name="app",
            stack="app",
            generation="g",
            scope="stack",
            workspace=workspace,
        )

        def _boom(*_a: object, **_k: object) -> None:
            raise RuntimeError("database is locked")

        monkeypatch.setattr(daemon.registry, "acquire_lease", _boom)

        with pytest.raises(RuntimeError, match="database is locked"):
            daemon.dispatch(
                "compose-acquire", {"workspace": workspace, "pid": 111, "proc_start": None}
            )

        assert daemon._execution_sessions == {}
        assert daemon.should_retire()
    finally:
        daemon.registry.close()


def test_execution_acquire_serializes_commands_in_one_persistent_container(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    from bosn import resources
    from bosn.converge import Converger, ConvergeResult

    project = tmp_path / "project"
    project.mkdir()
    (project / "Dockerfile").write_text("FROM scratch\n", encoding="utf-8")
    manifest = project / "bosn.toml"
    manifest.write_text(
        "[stack.dev]\ndockerfile = 'Dockerfile'\ndefault = true\n",
        encoding="utf-8",
    )
    result = ConvergeResult("dev", "sha256:g", "reused", "image")
    monkeypatch.setattr(
        Converger,
        "_acquire_execution_container",
        lambda *_args, **_kwargs: ("sha256:immutable-container", ()),
    )
    monkeypatch.setattr(resources, "process_alive", lambda *_args: True)
    request = {
        "manifest": str(manifest),
        "result": result.to_dict(),
        "workspace": str(project),
        "pid": 111,
        "proc_start": 10.0,
    }
    daemon = Daemon(state_dir=tmp_path / "state")
    try:
        first = daemon.dispatch("execution-acquire", request)
        second = daemon.dispatch(
            "execution-acquire", {**request, "engine": "C:/Program Files/Docker/docker.exe"}
        )

        assert first["ok"] is True
        assert second["ok"] is False
        assert "already running" in str(second["error"])

        assert daemon.dispatch("execution-release", {"session": first["session"]})["ok"]
        third = daemon.dispatch("execution-acquire", request)
        assert third["ok"] is True
        assert daemon.dispatch("execution-release", {"session": third["session"]})["ok"]
    finally:
        daemon.registry.close()


def test_dead_execution_owner_is_stopped_and_reaped_before_next_acquire(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    import bosn.engine
    from bosn import resources
    from bosn.converge import Converger, ConvergeResult
    from bosn.engine import EngineResult

    project = tmp_path / "project"
    project.mkdir()
    (project / "Dockerfile").write_text("FROM scratch\n", encoding="utf-8")
    manifest = project / "bosn.toml"
    manifest.write_text(
        "[stack.dev]\ndockerfile = 'Dockerfile'\ndefault = true\n",
        encoding="utf-8",
    )
    result = ConvergeResult("dev", "sha256:g", "reused", "image")
    alive = {111: True, 222: True}
    removals: list[tuple[str, list[str]]] = []

    class FakeEngine:
        def __init__(self, binary: str = "docker", **_kwargs: object) -> None:
            self.binary = binary

        def run(self, args: list[str], **_kwargs: object) -> EngineResult:
            removals.append((self.binary, args))
            return EngineResult(0, "sha256:immutable-container", "")

    daemon = Daemon(state_dir=tmp_path / "state")
    resource = daemon.registry.register_resource(
        kind="container",
        name="dev",
        stack="dev",
        generation="g",
        scope="stack",
        workspace=str(project),
    )

    def acquire(*_args: object, **_kwargs: object):
        lease = daemon.registry.acquire_lease(resource.id, pid=os.getpid(), proc_start=None)
        return "sha256:immutable-container", (lease,)

    monkeypatch.setattr(Converger, "_acquire_execution_container", acquire)
    monkeypatch.setattr(resources, "process_alive", lambda pid, _start: alive[pid])
    monkeypatch.setattr(bosn.engine, "Engine", FakeEngine)
    base_request = {
        "manifest": str(manifest),
        "result": result.to_dict(),
        "workspace": str(project),
        "proc_start": 10.0,
    }
    try:
        first = daemon.dispatch(
            "execution-acquire", {**base_request, "pid": 111, "engine": "podman"}
        )
        assert first["ok"] is True
        first_lease = daemon.registry.all_leases()[0]

        alive[111] = False
        second = daemon.dispatch(
            "execution-acquire", {**base_request, "pid": 222, "engine": "docker"}
        )

        assert second["ok"] is True
        assert removals == [
            ("podman", ["container", "rm", "--force", "sha256:immutable-container"])
        ]
        assert daemon.registry.get_lease(first_lease.id) is None
        assert first["session"] not in daemon._execution_sessions
        assert daemon.dispatch("execution-release", {"session": second["session"]})["ok"]
    finally:
        daemon.registry.close()


def test_status_reports_the_exact_dead_session_recovery_error(tmp_path: Path, monkeypatch) -> None:
    """#119: status must name the failed safe-reap action, not just say "blocked"."""
    from bosn import resources
    from bosn.registry import ExecutionSession

    daemon = Daemon(state_dir=tmp_path / "state")
    try:
        session = ExecutionSession("orphan", "immutable-id", "podman", 111, 10.0, ("lease",))
        daemon.registry.save_execution_session(session)
        daemon._execution_sessions[session.id] = session.lease_ids
        daemon._execution_containers[session.id] = session.container_id
        daemon._execution_owners[session.id] = (session.client_pid, session.client_start)
        daemon._execution_engines[session.id] = session.engine_binary
        daemon.registry.log_event(
            "execution.orphan_reap.error",
            "session=orphan container=immutable-id RuntimeError: busy",
        )
        monkeypatch.setattr(resources, "process_alive", lambda *_args: False)

        report = daemon.dispatch("status", {})

        [reported] = report["execution_sessions"]
        assert reported["id"] == "orphan"
        assert reported["container_id"] == "immutable-id"
        assert reported["client_alive"] is False
        assert reported["blocking_reason"] == "client is dead; awaiting safe exact-container reap"
        assert reported["last_orphan_reap_error"]["detail"].endswith("busy")
        assert "daemon-stop" in reported["recovery"]
    finally:
        daemon.registry.close()


def test_execution_session_survives_daemon_restart_and_still_serializes(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    from bosn import resources
    from bosn.converge import Converger, ConvergeResult
    from bosn.resources import prune_dead_leases

    project = tmp_path / "project"
    project.mkdir()
    (project / "Dockerfile").write_text("FROM scratch\n", encoding="utf-8")
    manifest = project / "bosn.toml"
    manifest.write_text(
        "[stack.dev]\ndockerfile = 'Dockerfile'\ndefault = true\n",
        encoding="utf-8",
    )
    result = ConvergeResult("dev", "sha256:g", "reused", "image")
    monkeypatch.setattr(resources, "process_alive", lambda *_args: True)
    clock = FakeClock()
    request = {
        "manifest": str(manifest),
        "result": result.to_dict(),
        "workspace": str(project),
        "pid": 111,
        "proc_start": 10.0,
        "engine": "podman",
    }

    first_daemon = Daemon(state_dir=tmp_path / "state", clock=clock)
    resource = first_daemon.registry.register_resource(
        kind="container",
        name="dev",
        stack="dev",
        generation="g",
        scope="stack",
        workspace=str(project),
    )

    def acquire(converger, *_args: object, **kwargs: object):
        lease_pid = kwargs["lease_pid"]
        lease_start = kwargs["lease_proc_start"]
        assert isinstance(lease_pid, int)
        assert isinstance(lease_start, float)
        lease = converger.registry.acquire_lease(
            resource.id,
            pid=lease_pid,
            proc_start=lease_start,
        )
        return "sha256:immutable-container", (lease,)

    monkeypatch.setattr(Converger, "_acquire_execution_container", acquire)
    first = first_daemon.dispatch("execution-acquire", request)
    assert first["ok"] is True
    original_lease = first_daemon.registry.all_leases()[0]
    assert (original_lease.pid, original_lease.proc_start) == (111, 10.0)
    first_daemon.registry.close()  # simulate the daemon process disappearing mid-exec

    restarted = Daemon(state_dir=tmp_path / "state", clock=clock)
    try:
        clock.advance(original_lease.ttl_seconds + 1)
        assert (
            prune_dead_leases(
                restarted.registry, alive_probe=lambda pid, start: (pid, start) == (111, 10.0)
            )
            == []
        )
        assert restarted.registry.get_lease(original_lease.id) is not None

        blocked = restarted.dispatch(
            "execution-acquire", {**request, "pid": 222, "proc_start": 20.0}
        )
        assert blocked["ok"] is False
        assert "already running" in str(blocked["error"])
        assert first["session"] in restarted._execution_sessions

        assert restarted.dispatch("execution-release", {"session": first["session"]})["ok"]
        next_acquire = restarted.dispatch(
            "execution-acquire", {**request, "pid": 222, "proc_start": 20.0}
        )
        assert next_acquire["ok"] is True
        assert restarted.dispatch("execution-release", {"session": next_acquire["session"]})["ok"]
    finally:
        restarted.registry.close()


def test_persistence_and_compensating_release_failures_keep_provisional_tracking(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    import bosn.engine
    from bosn import resources
    from bosn.converge import Converger, ConvergeResult
    from bosn.engine import EngineResult

    project = tmp_path / "project"
    project.mkdir()
    (project / "Dockerfile").write_text("FROM scratch\n", encoding="utf-8")
    manifest = project / "bosn.toml"
    manifest.write_text(
        "[stack.dev]\ndockerfile = 'Dockerfile'\ndefault = true\n",
        encoding="utf-8",
    )
    result = ConvergeResult("dev", "sha256:g", "reused", "image")
    alive = True

    class FakeEngine:
        def __init__(self, *_args: object, **_kwargs: object) -> None:
            pass

        def run(self, *_args: object, **_kwargs: object) -> EngineResult:
            return EngineResult(0, "", "")

    daemon = Daemon(state_dir=tmp_path / "state")
    resource = daemon.registry.register_resource(
        kind="container",
        name="dev",
        stack="dev",
        generation="g",
        scope="stack",
        workspace=str(project),
    )

    def acquire(*_args: object, **_kwargs: object):
        lease = daemon.registry.acquire_lease(resource.id, pid=111, proc_start=10.0)
        return "sha256:immutable-container", (lease,)

    monkeypatch.setattr(Converger, "_acquire_execution_container", acquire)
    monkeypatch.setattr(resources, "process_alive", lambda *_args: alive)
    monkeypatch.setattr(bosn.engine, "Engine", FakeEngine)
    monkeypatch.setattr(
        daemon.registry,
        "save_execution_session",
        lambda _session: (_ for _ in ()).throw(RuntimeError("persistence failed")),
    )
    original_release = daemon.registry.release_lease
    monkeypatch.setattr(
        daemon.registry,
        "release_lease",
        lambda _lease_id: (_ for _ in ()).throw(RuntimeError("release failed")),
    )
    request = {
        "manifest": str(manifest),
        "result": result.to_dict(),
        "workspace": str(project),
        "pid": 111,
        "proc_start": 10.0,
    }
    try:
        with pytest.raises(RuntimeError, match="persistence failed"):
            daemon.dispatch("execution-acquire", request)

        session = next(iter(daemon._execution_sessions))
        assert daemon.registry.all_leases()
        assert session in daemon._execution_containers
        assert daemon._reap_dead_execution_sessions() == 0
        assert session in daemon._execution_sessions

        alive = False
        assert daemon._reap_dead_execution_sessions() == 0
        assert session in daemon._execution_sessions
        monkeypatch.setattr(daemon.registry, "release_lease", original_release)
        assert daemon._reap_dead_execution_sessions() == 1
        assert daemon._execution_sessions == {}
    finally:
        daemon.registry.close()


def test_execution_release_failure_retains_retryable_session_tracking(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    from bosn import resources
    from bosn.converge import Converger, ConvergeResult

    project = tmp_path / "project"
    project.mkdir()
    (project / "Dockerfile").write_text("FROM scratch\n", encoding="utf-8")
    manifest = project / "bosn.toml"
    manifest.write_text(
        "[stack.dev]\ndockerfile = 'Dockerfile'\ndefault = true\n",
        encoding="utf-8",
    )
    result = ConvergeResult("dev", "sha256:g", "reused", "image")
    daemon = Daemon(state_dir=tmp_path / "state")
    resource = daemon.registry.register_resource(
        kind="container",
        name="dev",
        stack="dev",
        generation="g",
        scope="stack",
        workspace=str(project),
    )

    def acquire(*_args: object, **_kwargs: object):
        lease = daemon.registry.acquire_lease(resource.id, pid=os.getpid(), proc_start=None)
        return "sha256:immutable-container", (lease,)

    monkeypatch.setattr(resources, "process_alive", lambda *_args: True)
    monkeypatch.setattr(Converger, "_acquire_execution_container", acquire)
    request = {
        "manifest": str(manifest),
        "result": result.to_dict(),
        "workspace": str(project),
        "pid": 111,
        "proc_start": 10.0,
    }
    try:
        acquired = daemon.dispatch("execution-acquire", request)
        session = str(acquired["session"])
        original_release = daemon.registry.release_lease
        monkeypatch.setattr(
            daemon.registry,
            "release_lease",
            lambda _lease_id: (_ for _ in ()).throw(RuntimeError("database is locked")),
        )

        failed = daemon.dispatch("execution-release", {"session": session})

        assert failed["ok"] is False
        assert "database is locked" in str(failed["error"])
        assert session in daemon._execution_sessions
        assert session in daemon._execution_containers
        assert session in daemon._execution_owners
        assert session in daemon._execution_engines
        assert {item.id for item in daemon.registry.execution_sessions()} == {session}

        monkeypatch.setattr(daemon.registry, "release_lease", original_release)
        assert daemon.dispatch("execution-release", {"session": session})["ok"]
    finally:
        daemon.registry.close()


# -- unattended maintenance ------------------------------------------------


def test_maintenance_catches_up_then_runs_on_the_injected_clock(
    monkeypatch, tmp_path: Path
) -> None:
    """The first pass is due at startup; subsequent passes require no human command."""
    clock = FakeClock()
    calls: list[str] = []

    class ReachableEngine:
        def __init__(self, _binary: str, **_kwargs: object) -> None:
            pass

        def info(self) -> EngineInfo:
            return EngineInfo(binary="docker", reachable=True)

    class RecordingCollector:
        def __init__(self, _registry, _engine, *, config=None) -> None:
            pass

        def collect(self, **_kwargs):
            calls.append("gc")

            class Result:
                def summary(self):
                    return {"removed": 0}

            return Result()

    import bosn.engine
    import bosn.gc

    monkeypatch.setattr(bosn.engine, "Engine", ReachableEngine)
    monkeypatch.setattr(bosn.gc, "Collector", RecordingCollector)
    daemon = Daemon(
        state_dir=tmp_path, clock=clock, maintenance_interval_seconds=60, idle_retire_seconds=3600
    )
    try:
        assert daemon.run_maintenance_if_due() is True
        assert calls == ["gc"]
        assert daemon.run_maintenance_if_due() is False
        clock.advance(60)
        assert daemon.run_maintenance_if_due() is True
        assert calls == ["gc", "gc"]
        events = [row["kind"] for row in daemon.registry.events()]
        assert "maintenance.reap.started" in events
        assert "maintenance.gc.started" in events
        assert "maintenance.gc.finished" in events
    finally:
        daemon.registry.close()


def test_maintenance_engine_down_is_visible_and_uses_exponential_backoff(
    monkeypatch, tmp_path: Path
) -> None:
    clock = FakeClock()

    class DownEngine:
        def __init__(self, _binary: str, **_kwargs: object) -> None:
            pass

        def info(self) -> EngineInfo:
            return EngineInfo(binary="docker", reachable=False, detail="daemon asleep")

    import bosn.engine

    monkeypatch.setattr(bosn.engine, "Engine", DownEngine)
    daemon = Daemon(state_dir=tmp_path, clock=clock, maintenance_interval_seconds=60)
    try:
        assert daemon.run_maintenance_if_due()
        assert daemon._next_maintenance_at == clock.now() + 30
        clock.advance(30)
        assert daemon.run_maintenance_if_due()
        assert daemon._next_maintenance_at == clock.now() + 60
        events = daemon.registry.events()
        assert events[0]["kind"] == "maintenance.engine_down"
        assert "retry_in=60s" in events[0]["detail"]
    finally:
        daemon.registry.close()


def test_maintenance_pass_abandoned_for_stop_is_logged(monkeypatch, tmp_path: Path) -> None:
    """Issue #97: a pass in progress when shutdown begins must give up, visibly.

    `_stop` is set before the pass even starts here, which is the simplest way to prove
    the cooperative check fires without needing real threads or timing: the reap phase
    (deliberately left unpatched, so `maintenance.reap.started`/`.finished` prove the pass
    did begin) still runs to completion, but everything after the first `abandoned_after`
    check -- prune_leases, derived_done, the engine probe, GC -- must never start.
    """
    clock = FakeClock()

    class ExplodingEngine:
        """Any construction proves the pass ran past the point it should have stopped."""

        def __init__(self, _binary: str, **_kwargs: object) -> None:
            raise AssertionError("engine must not be constructed once _stop is set")

    import bosn.engine

    monkeypatch.setattr(bosn.engine, "Engine", ExplodingEngine)
    daemon = Daemon(state_dir=tmp_path, clock=clock, maintenance_interval_seconds=60)
    try:
        daemon._stop.set()
        daemon._run_maintenance()
        events = [row["kind"] for row in daemon.registry.events()]
        assert "maintenance.reap.started" in events
        assert "maintenance.reap.finished" in events
        assert "maintenance.aborted" in events
        detail = next(
            row["detail"]
            for row in daemon.registry.events()
            if row["kind"] == "maintenance.aborted"
        )
        assert detail == "stop requested after reap"
        # Nothing past the abandonment point may have run.
        assert "maintenance.prune_leases.started" not in events
        assert "maintenance.derived_done.started" not in events
        assert "maintenance.gc.started" not in events
    finally:
        daemon.registry.close()


def test_shutdown_does_not_hang_on_a_blocked_engine_probe(monkeypatch, tmp_path: Path) -> None:
    """Issue #97: `shutdown()` must not wait out an unbounded engine-reachability probe.

    The old `watchdog.join()` had no timeout at all, so a maintenance pass stuck in
    `Engine.info()` (60s per call, called up to twice) could block `shutdown()` for up to
    two minutes. The engine probe is monkeypatched to block on a `threading.Event` this
    test controls -- deterministic and instant to release, unlike racing a real 60s
    subprocess timeout -- so the pass hangs indefinitely until the test says otherwise.

    `WATCHDOG_JOIN_TIMEOUT_SECONDS` is monkeypatched down so this test (and the repeated
    runs the task asks for) stays fast; `shutdown()` reads the module-level constant by
    name at call time, so patching the module attribute changes what it sees.

    After the bounded shutdown returns, the test releases the blocked probe and lets the
    watchdog thread actually finish, then asserts no exception escaped it unhandled --
    proving the deferred-close path did not hand the watchdog a closed registry to write
    into (`sqlite3.ProgrammingError`-style fallout).
    """
    import bosn.engine

    probe_entered = threading.Event()
    release_probe = threading.Event()
    exceptions: list[BaseException] = []

    def record_exception(args: threading.ExceptHookArgs) -> None:
        if args.exc_value is not None:
            exceptions.append(args.exc_value)

    monkeypatch.setattr(threading, "excepthook", record_exception)
    # raising=False: on the pre-fix code this constant does not exist at all (the join had
    # no timeout to configure), and the point of this test is to observe that difference
    # via the assertions below, not via an AttributeError from monkeypatch itself.
    monkeypatch.setattr(daemon_mod, "WATCHDOG_JOIN_TIMEOUT_SECONDS", 1.0, raising=False)

    class BlockingEngine:
        def __init__(self, _binary: str, **_kwargs: object) -> None:
            pass

        def info(self) -> EngineInfo:
            probe_entered.set()
            release_probe.wait(30)
            return EngineInfo(binary="docker", reachable=True)

    monkeypatch.setattr(bosn.engine, "Engine", BlockingEngine)

    # Deliberately do NOT push the maintenance deadline out -- a fresh registry's first
    # tick is due immediately, which is exactly the scenario this issue is about.
    daemon = Daemon(state_dir=tmp_path, idle_retire_seconds=3600)
    thread = threading.Thread(target=daemon.serve_forever, daemon=True)
    thread.start()
    try:
        assert _wait_until(lambda: daemon_mod.is_serving(tmp_path)), "daemon never came up"
        assert probe_entered.wait(10), "watchdog never reached the blocked engine probe"

        daemon.request_stop()
        # thread runs serve_forever() -> finally: self.shutdown(), which does the bounded
        # watchdog join. A generous outer bound catches a genuine regression without
        # making the test itself flaky under load.
        thread.join(timeout=15)
        assert not thread.is_alive(), "shutdown() hung instead of returning within its bound"
        assert not daemon_mod.is_serving(tmp_path)
    finally:
        # Let the stuck watchdog finish so it does not leak into later tests, and so any
        # exception it raises is observed by our excepthook above.
        release_probe.set()
        watchdog = daemon._watchdog_thread
        if watchdog is not None:
            watchdog.join(timeout=15)
            assert not watchdog.is_alive(), "watchdog never noticed the probe unblocking"

    assert exceptions == [], f"watchdog raised unhandled exception(s): {exceptions}"


def test_shutdown_does_not_close_registry_while_startup_reconcile_still_running(
    monkeypatch, tmp_path: Path
) -> None:
    """Issue #101: `shutdown()` must not close the registry while the startup-reconcile
    thread (`_reconcile_startup_resources`) can still write to it.

    The captured traceback that opened #101 was not "nothing catches the scan failure" --
    the handler *is* guarded (`except EngineError`, logging the failure). The actual crash
    was the guard's own logging call: `shutdown()` had already closed the registry by the
    time the handler ran, so `self.registry.log_event(...)` raised
    `sqlite3.ProgrammingError: Cannot operate on a closed database` from a background
    thread with nothing positioned to catch it.

    This reproduces that race deterministically instead of trying to win it against a real
    engine scan: `ResourceScanner.scan` is replaced with a version that blocks on a
    `threading.Event` until the test releases it, so the test controls exactly when the
    scan (and thus the `EngineError` it raises) happens relative to `shutdown()` -- release
    it only after `shutdown()` has already run and decided what to do about the registry.

    `RECONCILE_JOIN_TIMEOUT_SECONDS` is monkeypatched to 0.0 (its shipped default, as of
    the fix for the shutdown-latency regression a first version of this bound introduced --
    see the constant's own comment for the measurements) rather than left to whatever the
    module happens to define. That is deliberate, not redundant: pinning it here means this
    test stays fast and keeps exercising the deferred-close path even if a future change
    raises the production default back into the seconds -- exactly the kind of "helpful"
    edit the constant's comment warns against. Without this pin, that edit would slip in
    silently, this test would just get slower, and the doubling-of-the-suite regression the
    constant's comment describes could recur without any test catching it. `raising=False`
    because on the pre-fix code this constant does not exist at all -- the startup-reconcile
    thread was never tracked or joined by `shutdown()` in the first place, which is exactly
    the bug.
    """
    import bosn.engine
    import bosn.resources

    scan_entered = threading.Event()
    release_scan = threading.Event()
    exceptions: list[BaseException] = []

    def record_exception(args: threading.ExceptHookArgs) -> None:
        if args.exc_value is not None:
            exceptions.append(args.exc_value)

    monkeypatch.setattr(threading, "excepthook", record_exception)
    monkeypatch.setattr(daemon_mod, "RECONCILE_JOIN_TIMEOUT_SECONDS", 0.0, raising=False)

    class MinimalEngine:
        """Has a callable `.run` so `_reconcile_startup_resources` doesn't take its
        "test double, not a resource-listing engine" early return. `.run` itself is never
        actually invoked -- the scan below is what blocks and then raises.
        """

        def __init__(self, _binary: str, **_kwargs: object) -> None:
            pass

        def run(self, *_args: object, **_kwargs: object) -> None:
            raise AssertionError("engine.run should not be reached; the scan is mocked")

    def blocking_scan(self, _registry_id: str) -> None:
        scan_entered.set()
        release_scan.wait(30)
        raise EngineError("engine unreachable")

    monkeypatch.setattr(bosn.engine, "Engine", MinimalEngine)
    monkeypatch.setattr(bosn.resources.ResourceScanner, "scan", blocking_scan)

    daemon = Daemon(state_dir=tmp_path, idle_retire_seconds=3600)
    # Keep the watchdog's own maintenance pass from firing during this test -- it is not
    # what is under test here, and a fresh registry's first tick is otherwise due
    # immediately (issue #95/#98's deflaking pattern).
    daemon._set_next_maintenance(daemon.clock.now() + 3600)
    thread = threading.Thread(target=daemon.serve_forever, daemon=True)
    thread.start()
    try:
        assert _wait_until(lambda: daemon_mod.is_serving(tmp_path)), "daemon never came up"
        assert scan_entered.wait(10), "startup reconcile never reached the blocked scan"

        daemon.request_stop()
        # thread runs serve_forever() -> finally: self.shutdown(). The scan is still
        # blocked at this point, so this is exercising the exact moment the pre-fix code
        # would already have closed the registry out from under the reconcile thread.
        thread.join(timeout=15)
        assert not thread.is_alive(), "shutdown() hung instead of returning within its bound"
        assert not daemon_mod.is_serving(tmp_path)
    finally:
        # Let the blocked scan raise now, so the handler's `log_event` call runs -- either
        # into a still-open registry (fixed) or a closed one (the bug). Either way, let it
        # finish before this test ends so it cannot leak into a later test.
        release_scan.set()
        # getattr, not attribute access: on the pre-fix code `_reconcile_startup_resources`
        # runs on a bare, untracked `threading.Thread` -- there is no `_reconcile_thread`
        # attribute to join, which is exactly the bug. Fall back to a short sleep so the
        # background thread still gets a chance to run its (crashing, pre-fix) handler
        # before this test's assertions run.
        reconcile = getattr(daemon, "_reconcile_thread", None)
        if reconcile is not None:
            reconcile.join(timeout=15)
            assert not reconcile.is_alive(), "startup reconcile never noticed the scan unblocking"
        else:
            time.sleep(0.5)

    assert exceptions == [], f"startup reconcile raised unhandled exception(s): {exceptions}"

    # Not just "nothing crashed" -- the diagnostic the handler exists to record must have
    # actually landed. A fix that silently swallowed the log (e.g. by guarding it on
    # `_stop.is_set()` alone, which the #101 comment thread explicitly rejected as
    # insufficient) would pass a crash-only assertion while losing the event. Read through
    # a fresh read-only connection rather than `daemon.registry`: by this point the
    # deferred-close thread may already have closed the daemon's own connection.
    reader = Registry(tmp_path / "registry.sqlite3", read_only=True)
    try:
        kinds = [row["kind"] for row in reader.events()]
    finally:
        reader.close()
    assert "recovery.scan.unavailable" in kinds


def test_a_second_shutdown_after_a_deferred_close_does_not_raise(
    monkeypatch, tmp_path: Path
) -> None:
    """`shutdown()` must survive being called again after a deferred close already ran.

    `shutdown()` runs twice in normal operation -- once from `serve_forever`'s `finally`,
    once explicitly by a caller or fixture. When the first call defers the registry close
    to a background thread, that thread can close the connection at a moment the second
    call has no way to observe. The second call then reaches its own
    `shutdown.background_join_timeout` event and writes to a connection that is already
    gone.

    CI caught exactly that as a teardown error -- `sqlite3.ProgrammingError: Cannot
    operate on a closed database` raised from `shutdown()` itself -- which is the very
    failure class #101 set out to remove, reappearing one line away from where it was
    fixed. The lesson of that issue is that a *diagnostic* must never be the thing that
    crashes, and it applies to the code doing the fixing too.

    A first version of this test drove two real `shutdown()` calls around a thread that
    finished in between. It passed against the unguarded code: by the second call the
    thread was done, `outstanding` was empty, and the unguarded line was never reached at
    all. Running it against the unguarded code before trusting it is the only reason that
    was caught -- which is the same discipline the fix's own RED check applies.
    """
    release = threading.Event()
    monkeypatch.setattr(daemon_mod, "RECONCILE_JOIN_TIMEOUT_SECONDS", 0.0, raising=False)

    daemon = Daemon(state_dir=tmp_path, idle_retire_seconds=3600)
    daemon._set_next_maintenance(daemon.clock.now() + 3600)
    busy = threading.Thread(target=lambda: release.wait(30), daemon=True)
    busy.start()
    try:
        # The two conditions are established directly rather than raced for. The real
        # sequence is narrow -- an earlier call's deferred close has to land between this
        # call's `_bounded_join` deciding a thread is outstanding and its logging of that
        # decision -- and a test that tried to hit that window by timing would be the
        # flaky, proves-nothing kind this suite has already had to fix once (#95). What
        # must hold is simpler than the race that exposes it: `shutdown()` must not raise
        # when a thread is outstanding and the registry is already closed, however those
        # two came to be true together.
        daemon._reconcile_thread = busy  # outstanding: alive, and the bound above is zero
        daemon.registry.close()  # exactly what an earlier deferred close leaves behind

        # Must not raise. Against the unguarded code this is `sqlite3.ProgrammingError`
        # straight out of `shutdown()` -- the CI teardown error this test exists for.
        daemon.shutdown()
    finally:
        release.set()
        busy.join(timeout=15)
        assert not busy.is_alive()


def test_maintenance_deadline_persists_across_scheduler_wake(monkeypatch, tmp_path: Path) -> None:
    clock = FakeClock()
    calls: list[str] = []

    class Engine:
        def __init__(self, *_args, **_kwargs):
            pass

        def info(self):
            return EngineInfo(binary="docker", reachable=True)

    class Collector:
        def __init__(self, *_args, **_kwargs):
            pass

        def collect(self, **_kwargs):
            calls.append("gc")
            return type("Result", (), {"summary": lambda self: {}})()

    import bosn.engine
    import bosn.gc

    monkeypatch.setattr(bosn.engine, "Engine", Engine)
    monkeypatch.setattr(bosn.gc, "Collector", Collector)
    first = Daemon(
        state_dir=tmp_path, clock=clock, maintenance_interval_seconds=60, idle_retire_seconds=1
    )
    assert first.run_maintenance_if_due()
    assert first.registry.meta("maintenance.next_deadline") == f"{clock.now() + 60:.6f}"
    thread = threading.Thread(target=first.serve_forever, daemon=True)
    thread.start()
    assert _wait_until(lambda: daemon_mod.is_serving(tmp_path))
    clock.advance(1)
    thread.join(timeout=15)
    assert not thread.is_alive(), "the first daemon must retire before the scheduler wakes it"
    assert not daemon_mod.is_serving(tmp_path)
    clock.advance(120)
    wake = Daemon(state_dir=tmp_path, clock=clock, maintenance_interval_seconds=60)
    try:
        assert wake._next_maintenance_at == clock.now() - 61
        assert wake.run_maintenance_if_due(), "a missed deadline catches up on scheduler wake"
        assert calls == ["gc", "gc"]
    finally:
        wake.registry.close()


def test_maintenance_logs_prune_leases_started_and_finished(monkeypatch, tmp_path: Path) -> None:
    """Issue #45's dead-lease cleanup must be observable, not just idempotent."""
    clock = FakeClock()

    class ReachableEngine:
        def __init__(self, _binary: str, **_kwargs: object) -> None:
            pass

        def info(self) -> EngineInfo:
            return EngineInfo(binary="docker", reachable=True)

    class RecordingCollector:
        def __init__(self, _registry, _engine, *, config=None) -> None:
            pass

        def collect(self, **_kwargs):
            class Result:
                def summary(self):
                    return {"removed": 0}

            return Result()

    import bosn.engine
    import bosn.gc
    import bosn.resources

    monkeypatch.setattr(bosn.engine, "Engine", ReachableEngine)
    monkeypatch.setattr(bosn.gc, "Collector", RecordingCollector)
    monkeypatch.setattr(bosn.resources, "prune_dead_leases", lambda _registry: ["lease-1"])
    daemon = Daemon(state_dir=tmp_path, clock=clock, maintenance_interval_seconds=60)
    try:
        assert daemon.run_maintenance_if_due() is True
        # events() returns newest-first; reverse to read the pass in chronological order.
        events = [(row["kind"], row["detail"]) for row in reversed(daemon.registry.events())]
        kinds = [kind for kind, _detail in events]
        assert "maintenance.prune_leases.started" in kinds
        assert ("maintenance.prune_leases.finished", "pruned=1") in events
        # Prune runs strictly between reap and gc.
        assert kinds.index("maintenance.reap.finished") < kinds.index(
            "maintenance.prune_leases.started"
        )
        assert kinds.index("maintenance.prune_leases.finished") < kinds.index(
            "maintenance.gc.started"
        )
    finally:
        daemon.registry.close()


def test_maintenance_prune_leases_failure_is_logged_and_does_not_block_gc(
    monkeypatch, tmp_path: Path
) -> None:
    """A pruning failure must be visible, and the pass must still continue on to GC."""
    clock = FakeClock()

    class ReachableEngine:
        def __init__(self, _binary: str, **_kwargs: object) -> None:
            pass

        def info(self) -> EngineInfo:
            return EngineInfo(binary="docker", reachable=True)

    gc_calls: list[str] = []

    class RecordingCollector:
        def __init__(self, _registry, _engine, *, config=None) -> None:
            pass

        def collect(self, **_kwargs):
            gc_calls.append("gc")

            class Result:
                def summary(self):
                    return {"removed": 0}

            return Result()

    def _boom(_registry):
        raise RuntimeError("lease table is locked")

    import bosn.engine
    import bosn.gc
    import bosn.resources

    monkeypatch.setattr(bosn.engine, "Engine", ReachableEngine)
    monkeypatch.setattr(bosn.gc, "Collector", RecordingCollector)
    monkeypatch.setattr(bosn.resources, "prune_dead_leases", _boom)
    daemon = Daemon(state_dir=tmp_path, clock=clock, maintenance_interval_seconds=60)
    try:
        assert daemon.run_maintenance_if_due() is True
        events = [(row["kind"], row["detail"]) for row in daemon.registry.events()]
        kinds = [kind for kind, _detail in events]
        assert "maintenance.prune_leases.started" in kinds
        assert any(
            kind == "maintenance.prune_leases.error" and "lease table is locked" in detail
            for kind, detail in events
        )
        assert "maintenance.prune_leases.finished" not in kinds
        # A pruning failure must not block the rest of the maintenance pass.
        assert "maintenance.gc.started" in kinds
        assert gc_calls == ["gc"]
    finally:
        daemon.registry.close()


def test_malformed_persisted_maintenance_deadline_recovers(tmp_path: Path) -> None:
    clock = FakeClock()
    registry_path = tmp_path / "registry.sqlite3"
    from bosn.registry import Registry

    with Registry(registry_path, clock=clock) as registry:
        registry.set_meta("maintenance.next_deadline", "not-a-timestamp")
    daemon = Daemon(state_dir=tmp_path, clock=clock)
    try:
        assert daemon._next_maintenance_at == clock.now()
        assert any(
            event["kind"] == "maintenance.deadline.recovered" for event in daemon.registry.events()
        )
    finally:
        daemon.registry.close()


# -- derived done-signals (#49) ---------------------------------------------
#
# `classify_workspace` is faked throughout: the classifier module (bosn.gitstate) proves
# its own git-state logic against real repositories in its own test module. These tests
# only prove the daemon's wiring -- enumeration, the mark-done call, event evidence, and
# fault isolation -- so nothing here builds a real git repo.


def _seed_workspace(registry, workspace: str, name: str, *, scope: str = "spec") -> None:
    registry.register_resource(
        kind="volume", name=name, stack="s", generation="g", scope=scope, workspace=workspace
    )


def _fake_verdict(state, evidence: str):
    from bosn.gitstate import WorkspaceState, WorkspaceVerdict

    resolved = state if isinstance(state, WorkspaceState) else WorkspaceState(state)
    safe = resolved in (WorkspaceState.ABSENT, WorkspaceState.CLEAN)
    return WorkspaceVerdict(state=resolved, safe_to_mark_done=safe, evidence=evidence)


def test_derived_done_marks_absent_and_clean_workspaces(monkeypatch, tmp_path: Path) -> None:
    import bosn.gitstate

    clock = FakeClock()
    daemon = Daemon(state_dir=tmp_path, clock=clock)
    try:
        _seed_workspace(daemon.registry, "/absent", "vol-absent")
        _seed_workspace(daemon.registry, "/clean", "vol-clean")

        def fake_classify(path):
            from bosn.gitstate import WorkspaceState

            mapping = {
                "/absent": (WorkspaceState.ABSENT, "does not exist"),
                "/clean": (WorkspaceState.CLEAN, "'main' is clean and matches upstream"),
            }
            state, evidence = mapping[str(path)]
            return _fake_verdict(state, evidence)

        monkeypatch.setattr(bosn.gitstate, "classify_workspace", fake_classify)

        candidates, reclaimed = daemon._derived_done_pass()

        assert candidates == 2
        assert reclaimed == 2
        assert daemon.registry.done_workspace_ids() == {"/absent", "/clean"}
    finally:
        daemon.registry.close()


@pytest.mark.parametrize(
    "state",
    [
        "dirty",
        "unpushed",
        "detached",
        "no_upstream",
        "not_a_repo",
        "unavailable",
    ],
)
def test_derived_done_leaves_unsafe_states_untouched(
    monkeypatch, tmp_path: Path, state: str
) -> None:
    import bosn.gitstate

    clock = FakeClock()
    daemon = Daemon(state_dir=tmp_path, clock=clock)
    try:
        _seed_workspace(daemon.registry, "/w", "vol")

        monkeypatch.setattr(
            bosn.gitstate, "classify_workspace", lambda _path: _fake_verdict(state, "evidence")
        )

        candidates, reclaimed = daemon._derived_done_pass()

        assert candidates == 1
        assert reclaimed == 0
        assert daemon.registry.done_workspace_ids() == set()
    finally:
        daemon.registry.close()


def test_derived_done_does_not_reclassify_an_already_done_workspace(
    monkeypatch, tmp_path: Path
) -> None:
    import bosn.gitstate
    from bosn.gc import mark_done

    clock = FakeClock()
    daemon = Daemon(state_dir=tmp_path, clock=clock)
    try:
        _seed_workspace(daemon.registry, "/already-done", "vol-done")
        _seed_workspace(daemon.registry, "/pending", "vol-pending")
        assert mark_done(daemon.registry, "/already-done") == 1

        classified: list[str] = []

        def fake_classify(path):
            classified.append(str(path))
            return _fake_verdict("clean", "clean and pushed")

        monkeypatch.setattr(bosn.gitstate, "classify_workspace", fake_classify)

        candidates, reclaimed = daemon._derived_done_pass()

        assert candidates == 1
        assert reclaimed == 1
        assert classified == ["/pending"], "an already-done workspace must never be reclassified"
    finally:
        daemon.registry.close()


def test_derived_done_logs_evidence_for_both_directions(monkeypatch, tmp_path: Path) -> None:
    import bosn.gitstate

    clock = FakeClock()
    daemon = Daemon(state_dir=tmp_path, clock=clock)
    try:
        _seed_workspace(daemon.registry, "/safe", "vol-safe")
        _seed_workspace(daemon.registry, "/unsafe", "vol-unsafe")

        def fake_classify(path):
            from bosn.gitstate import WorkspaceState

            if str(path) == "/safe":
                return _fake_verdict(WorkspaceState.CLEAN, "'main' is clean and matches upstream")
            return _fake_verdict(WorkspaceState.DIRTY, "3 uncommitted changes")

        monkeypatch.setattr(bosn.gitstate, "classify_workspace", fake_classify)

        daemon._derived_done_pass()

        events = [(row["kind"], row["detail"]) for row in daemon.registry.events()]
        reclaimed = [
            detail for kind, detail in events if kind == "maintenance.derived_done.reclaimed"
        ]
        protected = [
            detail for kind, detail in events if kind == "maintenance.derived_done.protected"
        ]
        assert len(reclaimed) == 1
        assert "/safe" in reclaimed[0]
        assert "clean and matches upstream" in reclaimed[0]
        assert len(protected) == 1
        assert "/unsafe" in protected[0]
        assert "3 uncommitted changes" in protected[0]
    finally:
        daemon.registry.close()


def test_derived_done_raising_classifier_protects_and_does_not_block_gc(
    monkeypatch, tmp_path: Path
) -> None:
    """A classifier crash must be logged, must protect the workspace it was evaluating, and
    must not prevent the GC step that follows it in the same pass from running."""
    import bosn.gitstate

    clock = FakeClock()

    class ReachableEngine:
        def __init__(self, _binary: str, **_kwargs: object) -> None:
            pass

        def info(self) -> EngineInfo:
            return EngineInfo(binary="docker", reachable=True)

    gc_calls: list[str] = []

    class RecordingCollector:
        def __init__(self, _registry, _engine, *, config=None) -> None:
            pass

        def collect(self, **_kwargs):
            gc_calls.append("gc")

            class Result:
                def summary(self):
                    return {"removed": 0}

            return Result()

    import bosn.engine
    import bosn.gc

    monkeypatch.setattr(bosn.engine, "Engine", ReachableEngine)
    monkeypatch.setattr(bosn.gc, "Collector", RecordingCollector)

    def boom(_path):
        raise RuntimeError("git executable vanished mid-classification")

    monkeypatch.setattr(bosn.gitstate, "classify_workspace", boom)

    daemon = Daemon(state_dir=tmp_path, clock=clock, maintenance_interval_seconds=60)
    try:
        _seed_workspace(daemon.registry, "/flaky", "vol-flaky")

        assert daemon.run_maintenance_if_due() is True

        events = [(row["kind"], row["detail"]) for row in reversed(daemon.registry.events())]
        kinds = [kind for kind, _detail in events]
        assert any(
            kind == "maintenance.derived_done.error" and "/flaky" in detail
            for kind, detail in events
        )
        assert daemon.registry.done_workspace_ids() == set()
        assert "maintenance.gc.started" in kinds
        assert gc_calls == ["gc"], "a classifier failure must not prevent GC from running"
    finally:
        daemon.registry.close()


def test_adopt_with_multiple_foreign_registries_prints_selectable_commands(
    monkeypatch, tmp_path: Path
) -> None:
    """Ambiguity must produce exact `--from-registry <id>` commands, not a blanket refusal."""
    from bosn import labels
    from bosn.resources import DiscoveredResource, ScanResult

    first, second = "lost-registry-1", "lost-registry-2"

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

    class Scanner:
        def __init__(self, _engine) -> None:
            pass

        def scan(self, registry_id: str, **_kwargs):
            foreign = [
                DiscoveredResource("volume", "cache-1", _raw(first)),
                DiscoveredResource("volume", "cache-2", _raw(second)),
            ]
            return ScanResult(foreign=foreign)

    import bosn.resources

    monkeypatch.setattr(bosn.resources, "ResourceScanner", Scanner)
    daemon = Daemon(state_dir=tmp_path)
    try:
        original = daemon.registry.registry_id
        reply = daemon._verb_adopt({"engine": "docker"})
        assert not reply["ok"]
        error = str(reply["error"])
        # Exact, selectable commands for each candidate -- never a blanket refusal.
        assert f"bosn adopt --from-registry {first}" in error
        assert f"bosn adopt --from-registry {second}" in error
        assert "refused" not in error
        assert daemon.registry.registry_id == original
    finally:
        daemon.registry.close()


def test_adopt_preserves_a_nonempty_registry_identity(monkeypatch, tmp_path: Path) -> None:
    """Recovery of a lost database must never strand rows in an existing one."""
    from bosn import labels
    from bosn.resources import DiscoveredResource, ScanResult

    lost = "lost-registry"
    raw = labels.ResourceLabels(
        registry=lost,
        kind="volume",
        stack="dev",
        generation="digest",
        scope="spec",
        workspace="workspace",
        created="2026-01-01T00:00:00Z",
    ).to_dict()

    class Scanner:
        def __init__(self, _engine) -> None:
            pass

        def scan(self, registry_id: str, **_kwargs):
            resource = DiscoveredResource("volume", "cache", raw)
            return ScanResult(owned=[resource] if registry_id == lost else [], foreign=[resource])

    import bosn.resources

    monkeypatch.setattr(bosn.resources, "ResourceScanner", Scanner)
    daemon = Daemon(state_dir=tmp_path)
    try:
        original = daemon.registry.registry_id
        daemon.registry.register_resource(
            kind="volume",
            name="current-cache",
            stack="dev",
            generation="current",
            scope="spec",
            workspace="workspace",
        )
        reply = daemon._verb_adopt({"engine": "docker", "source_registry": lost})
        assert not reply["ok"]
        assert daemon.registry.registry_id == original
        assert "empty registry" in str(reply["error"])
    finally:
        daemon.registry.close()


def test_adopt_recovers_the_selected_lost_identity(monkeypatch, tmp_path: Path) -> None:
    from bosn import labels
    from bosn.resources import DiscoveredResource, ScanResult

    lost = "lost-registry"
    raw = labels.ResourceLabels(
        registry=lost,
        kind="volume",
        stack="dev",
        generation="digest",
        scope="spec",
        workspace="workspace",
        created="2026-01-01T00:00:00Z",
    ).to_dict()

    class Scanner:
        def __init__(self, _engine) -> None:
            pass

        def scan(self, registry_id: str, **_kwargs):
            resource = DiscoveredResource("volume", "cache", raw)
            return ScanResult(owned=[resource] if registry_id == lost else [], foreign=[resource])

    import bosn.resources

    monkeypatch.setattr(bosn.resources, "ResourceScanner", Scanner)
    daemon = Daemon(state_dir=tmp_path)
    try:
        reply = daemon._verb_adopt({"engine": "docker", "source_registry": lost})
        assert reply["ok"]
        assert daemon.registry.registry_id == lost
        assert daemon.registry.get_resource_by_engine_identity("volume", "cache") is not None
    finally:
        daemon.registry.close()


# -- startup reconciliation -------------------------------------------------
#
# gc.Collector.collect() has exactly one persistent removal boundary: it calls the
# engine to remove a resource, and only on success does it call registry.remove_resource
# (src/bosn/gc.py, the final loop). A crash between those two calls leaves a stale
# registry row for an engine object that no longer exists -- the "remove-before-registry"
# case below. The container-stop step earlier in collect() only issues `container stop`
# and a log_event; it never deletes a registry row, so it is not a removal boundary.
#
# The complementary creation boundary lives in converge.py: engine.run(["...", "create",
# ...]) followed by self._register(...) -> registry.register_resource(...). A crash
# between those two calls leaves a complete labeled engine object with no registry row --
# the "create-before-registry" case below.
#
# Both boundaries are repaired by Daemon._reconcile_startup_resources via
# resources.reconcile_owned. These tests exercise that daemon method directly (not just
# the underlying resources.reconcile_owned unit), to prove the daemon wires scanning,
# prior-resource capture, and event logging together correctly.


class _StubEngine:
    """Only needs to look like an Engine to the reachability check in the daemon."""

    def run(self, args, *, check: bool = False):  # pragma: no cover - not exercised
        raise AssertionError("the stubbed ResourceScanner should intercept all calls")


def test_startup_reconciliation_repairs_create_before_registry_crash(
    monkeypatch, tmp_path: Path
) -> None:
    """Daemon startup must recover a labeled engine object with no registry row."""
    from bosn import labels
    from bosn.resources import DiscoveredResource, ScanResult

    raw = labels.ResourceLabels(
        registry="placeholder",
        kind="volume",
        stack="dev",
        generation="digest",
        scope="spec",
        workspace="workspace",
        created="2026-01-01T00:00:00Z",
    ).to_dict()

    class Scanner:
        def __init__(self, _engine) -> None:
            pass

        def scan(self, registry_id: str, **_kwargs):
            raw["bosn.registry"] = registry_id
            resource = DiscoveredResource("volume", "created-first", raw)
            return ScanResult(owned=[resource], scanned_kinds={"volume"})

    import bosn.engine
    import bosn.resources

    monkeypatch.setattr(bosn.resources, "ResourceScanner", Scanner)
    monkeypatch.setattr(bosn.engine, "Engine", lambda *a, **k: _StubEngine())
    daemon = Daemon(state_dir=tmp_path)
    try:
        daemon._reconcile_startup_resources()
        recovered = daemon.registry.get_resource_by_engine_identity("volume", "created-first")
        assert recovered is not None
        assert recovered.state == "adopted"
        assert any(event["kind"] == "recovery.startup" for event in daemon.registry.events())
    finally:
        daemon.registry.close()


def test_startup_reconciliation_repairs_remove_before_registry_crash(
    monkeypatch, tmp_path: Path
) -> None:
    """Daemon startup must drop a registry row whose engine object is already gone."""
    from bosn.resources import ScanResult

    class Scanner:
        def __init__(self, _engine) -> None:
            pass

        def scan(self, registry_id: str, **_kwargs):
            return ScanResult(scanned_kinds={"volume"})

    import bosn.engine
    import bosn.resources

    monkeypatch.setattr(bosn.resources, "ResourceScanner", Scanner)
    monkeypatch.setattr(bosn.engine, "Engine", lambda *a, **k: _StubEngine())
    daemon = Daemon(state_dir=tmp_path)
    try:
        stale = daemon.registry.register_resource(
            kind="volume",
            name="removed-first",
            stack="dev",
            generation="digest",
            scope="spec",
            workspace="workspace",
        )
        daemon._reconcile_startup_resources()
        assert daemon.registry.get_resource(stale.id) is None
    finally:
        daemon.registry.close()


def test_startup_reconciliation_is_idempotent(monkeypatch, tmp_path: Path) -> None:
    """Mirrors the prune_dead_leases idempotency test: a second pass repairs nothing."""
    from bosn import labels
    from bosn.resources import DiscoveredResource, ScanResult

    raw = labels.ResourceLabels(
        registry="placeholder",
        kind="volume",
        stack="dev",
        generation="digest",
        scope="spec",
        workspace="workspace",
        created="2026-01-01T00:00:00Z",
    ).to_dict()

    class Scanner:
        def __init__(self, _engine) -> None:
            pass

        def scan(self, registry_id: str, **_kwargs):
            raw["bosn.registry"] = registry_id
            resource = DiscoveredResource("volume", "created-first", raw)
            return ScanResult(owned=[resource], scanned_kinds={"volume"})

    import bosn.engine
    import bosn.resources

    monkeypatch.setattr(bosn.resources, "ResourceScanner", Scanner)
    monkeypatch.setattr(bosn.engine, "Engine", lambda *a, **k: _StubEngine())
    daemon = Daemon(state_dir=tmp_path)
    try:
        daemon._reconcile_startup_resources()
        after_first = daemon.registry.list_resources()
        repairs_after_first = sum(
            1 for event in daemon.registry.events() if event["kind"] == "recovery.startup"
        )
        assert repairs_after_first == 1

        daemon._reconcile_startup_resources()
        after_second = daemon.registry.list_resources()
        repairs_after_second = sum(
            1 for event in daemon.registry.events() if event["kind"] == "recovery.startup"
        )

        assert [r.id for r in after_second] == [r.id for r in after_first]
        assert repairs_after_second == repairs_after_first, "second pass must log no repair"
    finally:
        daemon.registry.close()


# -- client behavior -------------------------------------------------------


def test_mutating_request_fails_closed_without_a_daemon(tmp_path: Path) -> None:
    with pytest.raises(DaemonError, match="no bosn daemon is running"):
        daemon_mod.request("status", tmp_path, autostart=False)


def test_mutating_request_restarts_version_skew_only_without_live_sessions(
    tmp_path: Path, monkeypatch
) -> None:
    monkeypatch.setattr(daemon_mod, "is_serving", lambda *_a, **_k: True)
    replies = iter(
        [
            {"ok": False, "daemon_version": "old", "client_version": "new"},
            {"ok": True, "execution_sessions": []},
            {"ok": True, "result": "retried"},
        ]
    )
    monkeypatch.setattr(ipc, "send_request", lambda *_a, **_k: next(replies))
    stopped = []
    spawned = []
    monkeypatch.setattr(daemon_mod, "stop", lambda *_a, **_k: stopped.append(True) or True)
    monkeypatch.setattr(daemon_mod, "spawn", lambda *_a, **_k: spawned.append(True))

    assert daemon_mod.request("done", tmp_path)["result"] == "retried"
    assert stopped == [True]
    assert spawned == [True]


def test_mutating_request_preserves_live_session_during_version_skew(
    tmp_path: Path, monkeypatch
) -> None:
    monkeypatch.setattr(daemon_mod, "is_serving", lambda *_a, **_k: True)
    replies = iter(
        [
            {"ok": False, "error": "version mismatch", "daemon_version": "old"},
            {"ok": True, "execution_sessions": [{"id": "session-1", "client_pid": 42}]},
        ]
    )
    monkeypatch.setattr(ipc, "send_request", lambda *_a, **_k: next(replies))
    monkeypatch.setattr(
        daemon_mod, "stop", lambda *_a, **_k: (_ for _ in ()).throw(AssertionError())
    )

    reply = daemon_mod.request("done", tmp_path)
    assert not reply["ok"]
    assert "session-1 (pid 42)" in reply["error"]


def test_transport_error_on_a_dead_port() -> None:
    with pytest.raises(ipc.TransportError, match="unreachable"):
        ipc.send_request(daemon_mod.free_port(), {"verb": "ping"}, timeout=1.0)


def test_stop_returns_false_when_nothing_is_running(tmp_path: Path) -> None:
    assert daemon_mod.stop(tmp_path) is False


def test_stop_raises_immediately_when_daemon_refuses_shutdown(tmp_path: Path, monkeypatch) -> None:
    monkeypatch.setattr(daemon_mod, "is_serving", lambda *a, **k: True)
    monkeypatch.setattr(
        ipc,
        "send_request",
        lambda *a, **k: {
            "ok": False,
            "error": "daemon shutdown blocked by live execution session session-1 (pid 42)",
        },
    )

    with pytest.raises(DaemonError, match=r"live execution session session-1 \(pid 42\)"):
        daemon_mod.stop(tmp_path, timeout=0.0)


def test_spawn_failure_includes_bounded_detached_diagnostics(tmp_path: Path, monkeypatch) -> None:
    monkeypatch.setattr(daemon_mod, "is_serving", lambda *_a, **_k: False)
    monkeypatch.setattr(daemon_mod, "running_state", lambda *_a, **_k: None)
    monkeypatch.setattr(daemon_mod, "_detach", lambda *_a, **_k: 4321)
    log = daemon_mod.startup_log_file(tmp_path)
    log.write_text("fatal: token=top-secret\n", encoding="utf-8")

    with pytest.raises(DaemonError) as caught:
        daemon_mod.spawn(tmp_path, timeout=0)
    message = str(caught.value)
    assert "pid 4321" in message
    assert "fatal:" in message
    assert "top-secret" not in message


def test_detach_does_not_depend_on_the_running_process_broker() -> None:
    """Detachment must not require a broker daemon or a bundled trampoline binary.

    Both running-process detach entry points are broker clients: spawn_daemon needs a
    trampoline binary in its wheel assets, and launch_detached dials a named-pipe broker.
    bosn must be able to start its own daemon with neither present.
    """
    code = _detach_code()
    assert "spawn_daemon" not in code
    assert "launch_detached" not in code
    assert "rp_daemon" not in code
    assert "subprocess.Popen" in code


def test_detach_truncates_diagnostics_for_each_launch(tmp_path: Path, monkeypatch) -> None:
    log = daemon_mod.startup_log_file(tmp_path)
    log.write_text("old failure that must not accumulate", encoding="utf-8")

    class Child:
        pid = 4321

    monkeypatch.setattr(subprocess, "Popen", lambda *_a, **_k: Child())
    assert daemon_mod._detach(tmp_path) == 4321
    assert log.read_bytes() == b""


@pytest.mark.skipif(not sys.platform.startswith("win"), reason="Windows console flags")
def test_windows_detach_uses_create_no_window_not_detached_process() -> None:
    """DETACHED_PROCESS gives the child its own console -- a window pops up on screen."""
    code = _detach_code()
    assert "CREATE_NO_WINDOW" in code
    assert "DETACHED_PROCESS" not in code


@pytest.mark.slow
def test_real_detached_spawn_and_autostart(tmp_path: Path) -> None:
    """End-to-end: the CLI lazily spawns a real detached daemon and talks to it."""
    state = daemon_mod.spawn(tmp_path, timeout=60)
    try:
        assert state.port == daemon_mod.port_for(tmp_path)
        assert state.pid != os.getpid(), "daemon must be a separate process"
        assert ipc.send_request(state.port, {"verb": "ping", "auth": daemon_mod._secret(tmp_path)})[
            "ok"
        ]

        # spawn is idempotent and autostart reaches the same daemon
        assert daemon_mod.spawn(tmp_path).pid == state.pid
        assert daemon_mod.request("status", tmp_path)["pid"] == state.pid
    finally:
        assert daemon_mod.stop(tmp_path, timeout=30)
