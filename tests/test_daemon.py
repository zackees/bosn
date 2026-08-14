"""Phase 3: daemon singleton lifecycle.

The deterministic loopback port is the singleton concurrency primitive: a second daemon
fails its bind with EADDRINUSE, so the operating system is the arbiter. These tests pin
that property along with spawn, heartbeat, idle retirement, and fail-closed behavior.

No Docker involved -- these run on every platform.
"""

from __future__ import annotations

import os
import socket
import sys
import threading
import time
from collections.abc import Iterator
from pathlib import Path

import pytest

from bosn import daemon as daemon_mod
from bosn import ipc
from bosn.clock import FakeClock
from bosn.daemon import Daemon, DaemonError, DaemonState
from bosn.engine import EngineInfo


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


def _wait_until(predicate, timeout: float = 15.0, interval: float = 0.05) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if predicate():
            return True
        time.sleep(interval)
    return predicate()


@pytest.fixture
def served(tmp_path: Path) -> Iterator[Daemon]:
    daemon = Daemon(state_dir=tmp_path, idle_retire_seconds=3600)
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
        thread = threading.Thread(target=daemon.serve_forever, daemon=True)
        thread.start()
        assert _wait_until(lambda: daemon_mod.is_serving(tmp_path))
        assert daemon_mod.stop(tmp_path)
        thread.join(timeout=15)


def test_idle_retirement_stops_an_unused_daemon(tmp_path: Path) -> None:
    daemon = Daemon(state_dir=tmp_path, idle_retire_seconds=0.5)
    thread = threading.Thread(target=daemon.serve_forever, daemon=True)
    thread.start()
    assert _wait_until(lambda: daemon_mod.is_serving(tmp_path))
    thread.join(timeout=20)
    assert not thread.is_alive(), "idle daemon should have retired itself"
    assert not daemon_mod.is_serving(tmp_path)


# -- unattended maintenance ------------------------------------------------


def test_maintenance_catches_up_then_runs_on_the_injected_clock(
    monkeypatch, tmp_path: Path
) -> None:
    """The first pass is due at startup; subsequent passes require no human command."""
    clock = FakeClock()
    calls: list[str] = []

    class ReachableEngine:
        def __init__(self, _binary: str) -> None:
            pass

        def info(self) -> EngineInfo:
            return EngineInfo(binary="docker", reachable=True)

    class RecordingCollector:
        def __init__(self, _registry, _engine) -> None:
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
        def __init__(self, _binary: str) -> None:
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


# -- client behavior -------------------------------------------------------


def test_mutating_request_fails_closed_without_a_daemon(tmp_path: Path) -> None:
    with pytest.raises(DaemonError, match="no bosn daemon is running"):
        daemon_mod.request("status", tmp_path, autostart=False)


def test_transport_error_on_a_dead_port() -> None:
    with pytest.raises(ipc.TransportError, match="unreachable"):
        ipc.send_request(daemon_mod.free_port(), {"verb": "ping"}, timeout=1.0)


def test_stop_returns_false_when_nothing_is_running(tmp_path: Path) -> None:
    assert daemon_mod.stop(tmp_path) is False


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
