"""Cooperative Python-v4 cutover guard characterization."""

from __future__ import annotations

import json
import os
import queue
import subprocess
import threading
import time
from pathlib import Path

import pytest

from bosn import daemon as daemon_mod
from bosn import ipc
from bosn import registry as registry_mod
from bosn.daemon import Daemon
from bosn.migration_lock import (
    CUTOVER_MARKER,
    CutoverError,
    acquire_shared,
    assert_python_writes_allowed,
    publish_cutover_marker,
)
from bosn.registry import Registry, RegistryError


@pytest.fixture(scope="session")
def rust_guard_helper() -> Path:
    """Build the cross-language helper once; lock tests never include broker startup."""
    root = Path(__file__).parents[1]
    try:
        result = subprocess.run(
            [
                "soldr",
                "cargo",
                "build",
                "--locked",
                "-p",
                "bosn-registry",
                "--features",
                "migration-test-helper",
                "--bin",
                "migration-guard-test",
                "--message-format=json",
            ],
            cwd=root,
            capture_output=True,
            text=True,
            timeout=float(os.environ.get("BOSN_MIGRATION_GUARD_BUILD_TIMEOUT", "180")),
            check=False,
        )
    except subprocess.TimeoutExpired as exc:
        pytest.fail(f"Rust migration-guard helper build timed out: {exc}")
    if result.returncode != 0:
        pytest.fail(f"Rust migration-guard helper build failed:\n{result.stderr}")
    for line in result.stdout.splitlines():
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if event.get("reason") == "compiler-artifact" and event.get("target", {}).get("name") == (
            "migration-guard-test"
        ):
            executable = event.get("executable")
            if isinstance(executable, str):
                return Path(executable)
    pytest.fail("Rust migration-guard helper build produced no executable artifact")


def test_cutover_marker_is_private_create_new_and_blocks_legacy_writers(tmp_path):
    state = tmp_path / "state"
    with Registry(state / "registry.sqlite3") as registry:
        registry_id = registry.registry_id

    marker = publish_cutover_marker(state, registry_id)
    assert marker == state / CUTOVER_MARKER
    assert json.loads(marker.read_text(encoding="utf-8"))["registry_id"] == registry_id
    with pytest.raises(CutoverError):
        publish_cutover_marker(state, registry_id)
    with pytest.raises(RegistryError, match="Rust migration cutover"):
        Registry(state / "registry.sqlite3")


def test_invalid_or_conflicting_marker_fails_closed(tmp_path):
    state = tmp_path / "state"
    state.mkdir()
    (state / CUTOVER_MARKER).write_text("not json", encoding="utf-8")
    with pytest.raises(CutoverError):
        assert_python_writes_allowed(state)


def test_marker_is_owner_private_and_dangling_marker_fails_closed(tmp_path):
    state = tmp_path / "state"
    marker = publish_cutover_marker(state, "11111111-2222-4333-8444-555555555555")
    assert marker.stat().st_mode & 0o077 == 0
    dangling_state = tmp_path / "dangling"
    dangling_state.mkdir()
    try:
        (dangling_state / CUTOVER_MARKER).symlink_to(dangling_state / "missing")
    except (NotImplementedError, OSError):
        pytest.skip("platform cannot create a dangling symlink")
    with pytest.raises(CutoverError):
        assert_python_writes_allowed(dangling_state)


def test_constructor_failure_releases_shared_guard(tmp_path, monkeypatch):
    state = tmp_path / "state"

    def fail_connect(*_args, **_kwargs):
        raise RuntimeError("synthetic connection failure")

    monkeypatch.setattr(registry_mod.sqlite3, "connect", fail_connect)
    with pytest.raises(RuntimeError, match="synthetic"):
        Registry(state / "registry.sqlite3")
    if os.name == "nt":
        pytest.skip("POSIX lock probe is covered by cross-language tests on Windows")
    import fcntl

    with open(state / "registry.migration.lock", "r+b") as lock:
        fcntl.flock(lock.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)


def test_close_failure_retains_shared_guard_until_sqlite_close_succeeds(tmp_path):
    registry = Registry(tmp_path / "state" / "registry.sqlite3")
    real_connection = registry.conn

    class FailingClose:
        def close(self) -> None:
            raise RuntimeError("synthetic close failure")

    registry.conn = FailingClose()  # type: ignore[assignment]
    with pytest.raises(RuntimeError, match="synthetic"):
        registry.close()
    assert registry._migration_lock is not None
    registry.conn = real_connection
    registry.close()
    assert registry._migration_lock is None


def test_authenticated_daemon_cutover_publishes_marker_and_closes_admission(tmp_path):
    state = tmp_path / "state"
    daemon = Daemon(state_dir=state, idle_retire_seconds=3600)
    reply = daemon.dispatch("migration-cutover", {})
    assert reply["ok"] is True
    assert (state / CUTOVER_MARKER).exists()
    assert daemon.begin_request("gc") is False
    daemon.shutdown()


def test_cutover_requires_real_authenticated_ipc_and_refuses_active_admission(tmp_path):
    state = tmp_path / "state"
    daemon = Daemon(state_dir=state, idle_retire_seconds=3600)
    thread = threading.Thread(target=daemon.serve_forever, daemon=True)
    thread.start()
    deadline = time.monotonic() + 5
    while not daemon_mod.is_serving(state) and time.monotonic() < deadline:
        time.sleep(0.02)
    assert daemon_mod.is_serving(state)
    try:
        denied = ipc.send_request(daemon.port, {"verb": "migration-cutover", "auth": "wrong"})
        assert denied["ok"] is False
        assert not (state / CUTOVER_MARKER).exists()
        accepted = ipc.send_request(
            daemon.port,
            {"verb": "migration-cutover", "auth": daemon.secret, "version": daemon_mod.__version__},
        )
        assert accepted["ok"] is True
        assert (state / CUTOVER_MARKER).exists()
    finally:
        daemon.request_stop()
        thread.join(timeout=5)
        daemon.shutdown()


def test_cutover_refuses_active_requests_jobs_and_execution_ownership(tmp_path):
    daemon = Daemon(state_dir=tmp_path / "state", idle_retire_seconds=3600)
    try:
        assert daemon.begin_request("status") is True
        assert daemon.begin_request("status") is True
        assert daemon.dispatch("migration-cutover", {})["ok"] is False
        daemon.finish_request()
        daemon.finish_request()
        daemon.jobs.active_count = lambda: 1  # type: ignore[method-assign]
        assert daemon.dispatch("migration-cutover", {})["ok"] is False
        daemon.jobs.active_count = lambda: 0  # type: ignore[method-assign]
        daemon._execution_sessions["session"] = ("lease",)
        assert daemon.dispatch("migration-cutover", {})["ok"] is False
    finally:
        daemon.shutdown()


def test_real_python_shared_lock_blocks_the_rust_kernel_guard(tmp_path, rust_guard_helper):
    state = tmp_path / "state"
    guard = acquire_shared(state)
    try:
        result = subprocess.run(
            [
                str(rust_guard_helper),
                str(state),
            ],
            capture_output=True,
            text=True,
            timeout=10,
            check=False,
        )
        assert result.returncode != 0
        assert "MigrationGuardHeld" in result.stderr
    finally:
        guard.close()


def test_real_rust_kernel_guard_blocks_python_shared_lock_until_released(
    tmp_path, rust_guard_helper
):
    state = tmp_path / "state"
    state.mkdir()
    child = subprocess.Popen(
        [
            str(rust_guard_helper),
            str(state),
        ],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    try:
        stdout = child.stdout
        assert stdout is not None
        ready: queue.Queue[str] = queue.Queue()
        reader = threading.Thread(target=lambda: ready.put(stdout.readline()), daemon=True)
        reader.start()
        line = ready.get(timeout=10)
        if line != "ready\n":
            assert child.stderr is not None
            raise AssertionError(f"Rust guard never became ready: {child.stderr.read()}")
        acquired = threading.Event()
        waiting = threading.Event()

        def wait_for_shared() -> None:
            waiting.set()
            guard = acquire_shared(state)
            acquired.set()
            guard.close()

        waiter = threading.Thread(target=wait_for_shared, daemon=True)
        waiter.start()
        assert waiting.wait(5)
        assert not acquired.wait(0.2)
        assert child.stdin is not None
        child.stdin.write("release\n")
        child.stdin.flush()
        assert acquired.wait(10)
        assert child.wait(timeout=10) == 0
    finally:
        if child.poll() is None:
            child.terminate()
            child.wait(timeout=10)
