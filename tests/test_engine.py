"""Unit coverage for engine diagnostics that must not require a live daemon."""

from __future__ import annotations

import sys
import threading
import time

import pytest

from bosn import engine as engine_mod
from bosn.engine import DesktopEvidence, Engine, EngineError, EngineResult


class StalledProcess:
    """Deterministic local Docker-CLI stand-in: pending until the monitor kills it."""

    KEYBOARD_INTERRUPT_EXIT_CODES: set[int] = set()

    def __init__(self, *_args, **_kwargs) -> None:
        self.killed = False
        self._killed = threading.Event()
        self.output_bytes = 0

    @property
    def finished(self) -> bool:
        return self.killed

    def has_pending_output(self) -> bool:
        return False

    def get_next_line_non_blocking(self):
        return engine_mod.rp.EndOfStream() if self.killed else None

    def wait(self, *, timeout=None, echo=False):
        del echo
        if not self._killed.wait(timeout):
            raise TimeoutError()
        return -9

    def kill(self) -> None:
        self.killed = True
        self._killed.set()

    def captured_output_bytes(self) -> int:
        return self.output_bytes

    def poll(self) -> int | None:
        return -9 if self.killed else None


def _fast_health_monitor(monkeypatch) -> None:
    monkeypatch.setattr(engine_mod, "ENGINE_HEALTH_GRACE_SECONDS", 0.01)
    monkeypatch.setattr(engine_mod, "ENGINE_HEALTH_INTERVAL_SECONDS", 0.01)
    monkeypatch.setattr(engine_mod, "ENGINE_HEALTH_FAILURE_LIMIT", 2)


def test_stream_fails_when_repeated_bounded_probes_cannot_reach_engine(monkeypatch) -> None:
    _fast_health_monitor(monkeypatch)
    process = StalledProcess()
    monkeypatch.setattr(engine_mod.rp, "RunningProcess", lambda *_a, **_k: process)
    engine = Engine("docker")
    probes: list[float] = []

    def down(timeout: float) -> tuple[bool, str]:
        probes.append(timeout)
        return False, "server-version probe exceeded its deadline"

    monkeypatch.setattr(engine, "_probe_server", down)

    with pytest.raises(EngineError, match="down or unresponsive.*bad state") as raised:
        engine.stream(["build", "."])

    assert "bosn doctor" in str(raised.value)
    assert process.killed
    assert len(probes) == 2


def test_execute_fails_when_repeated_bounded_probes_cannot_reach_engine(monkeypatch) -> None:
    _fast_health_monitor(monkeypatch)
    process = StalledProcess()
    monkeypatch.setattr(engine_mod.rp, "RunningProcess", lambda *_a, **_k: process)
    engine = Engine("docker")
    monkeypatch.setattr(engine, "_probe_server", lambda _timeout: (False, "named pipe stalled"))
    aborted: list[str | None] = []
    monkeypatch.setattr(engine, "_abort_container", lambda value: aborted.append(value))

    with pytest.raises(EngineError, match="down or unresponsive.*bad state"):
        engine.execute(["exec", "exact-id", "true"], timeout=60, abort_container="exact-id")

    assert process.killed
    assert aborted == ["exact-id"]


def test_captured_execute_reports_the_same_engine_health_failure(monkeypatch) -> None:
    _fast_health_monitor(monkeypatch)
    process = StalledProcess()
    monkeypatch.setattr(engine_mod.rp, "RunningProcess", lambda *_a, **_k: process)
    engine = Engine("docker")
    monkeypatch.setattr(engine, "_probe_server", lambda _timeout: (False, "API stalled"))

    with pytest.raises(EngineError, match="down or unresponsive.*bosn doctor"):
        engine.execute_capture(["exec", "exact-id", "true"], timeout=60)

    assert process.killed


def test_health_monitor_tolerates_a_transient_probe_failure(monkeypatch) -> None:
    _fast_health_monitor(monkeypatch)
    monkeypatch.setattr(engine_mod, "ENGINE_HEALTH_FAILURE_LIMIT", 2)
    attempts = 0
    process = StalledProcess()
    engine = Engine("docker")

    def recovers(_timeout: float) -> tuple[bool, str]:
        nonlocal attempts
        attempts += 1
        return (False, "busy") if attempts == 1 else (True, "")

    monkeypatch.setattr(engine, "_probe_server", recovers)
    monitor = engine._monitor(process)
    monitor.start()
    time.sleep(0.05)
    monitor.stop()

    assert monitor.error is None
    assert not process.killed


def test_continuing_output_defers_health_failure_classification(monkeypatch) -> None:
    _fast_health_monitor(monkeypatch)
    process = StalledProcess()
    engine = Engine("docker")
    probes = 0

    def down(_timeout: float) -> tuple[bool, str]:
        nonlocal probes
        probes += 1
        return False, "busy"

    monkeypatch.setattr(engine, "_probe_server", down)
    monitor = engine._monitor(process)
    monitor.start()
    for _ in range(8):
        process.output_bytes += 1
        time.sleep(0.005)
    monitor.stop()

    assert monitor.error is None
    assert not process.killed
    assert probes <= 1


def test_native_pending_process_is_bounded_by_engine_health_monitor(monkeypatch) -> None:
    """A real local child cannot become immortal when the independent probe is wedged."""
    _fast_health_monitor(monkeypatch)
    engine = Engine(sys.executable)
    monkeypatch.setattr(engine, "_probe_server", lambda _timeout: (False, "transport stalled"))
    started = time.monotonic()

    with pytest.raises(EngineError, match="down or unresponsive"):
        engine.stream(["-c", "import time; time.sleep(60)"])

    assert time.monotonic() - started < 2.0


class FakeEngine(Engine):
    def available(self) -> bool:
        return True

    def run(
        self, args: list[str], *, check: bool = False, timeout: float | None = None
    ) -> EngineResult:
        del check, timeout
        if args == ["version", "--format", "{{.Client.Version}}"]:
            return EngineResult(0, "28.5.1", "")
        if args == ["version", "--format", "{{.Server.Version}}"]:
            return EngineResult(0, "28.5.1", "")
        if args == ["info", "--format", "{{.SystemTime}}"]:
            return EngineResult(0, "1970-01-01T00:01:41.000000000Z", "")
        raise AssertionError(f"unexpected engine call: {args}")


def test_engine_info_measures_clock_skew_around_the_probe(monkeypatch) -> None:
    timestamps = iter([100.0, 102.0])
    monkeypatch.setattr(engine_mod.time, "time", lambda: next(timestamps))

    info = FakeEngine().info()

    assert info.reachable
    assert info.clock_skew_seconds == 0.0


def test_engine_info_tolerates_an_unparseable_engine_clock(monkeypatch) -> None:
    engine = FakeEngine()
    real_run = engine.run

    def run(args, **kwargs):
        if args == ["info", "--format", "{{.SystemTime}}"]:
            return EngineResult(0, "not-a-time", "")
        return real_run(args, **kwargs)

    monkeypatch.setattr(engine, "run", run)

    info = engine.info()

    assert info.reachable
    assert info.clock_skew_seconds is None


def test_engine_info_stays_reachable_when_clock_probe_fails(monkeypatch) -> None:
    engine = FakeEngine()
    real_run = engine.run

    def run(args, **kwargs):
        if args == ["info", "--format", "{{.SystemTime}}"]:
            raise EngineError("clock probe unsupported")
        return real_run(args, **kwargs)

    monkeypatch.setattr(engine, "run", run)

    info = engine.info()

    assert info.reachable
    assert info.clock_skew_seconds is None


def test_engine_info_classifies_a_windows_desktop_server_500_only_with_supporting_evidence(
    monkeypatch,
) -> None:
    """#136: a live client plus Desktop/WSL evidence is not generic unreachable.

    The raw CLI response remains operator evidence.  A server HTTP 5xx alone is not
    enough to claim a Docker Desktop wedge, because a remote/context engine can emit
    the same text without a local Desktop recovery path.
    """

    class WedgeEngine(Engine):
        def available(self) -> bool:
            return True

        def run(self, args, **_kwargs):
            if args == ["version", "--format", "{{.Client.Version}}"]:
                return EngineResult(0, "28.5.1", "")
            if args == ["version", "--format", "{{.Server.Version}}"]:
                return EngineResult(
                    1,
                    "",
                    "request returned 500 Internal Server Error for API route and version",
                )
            raise AssertionError(f"unexpected engine call: {args}")

    monkeypatch.setattr(
        engine_mod,
        "docker_desktop_evidence",
        lambda: DesktopEvidence(desktop_running=True, wsl_distro_running=True),
    )
    info = WedgeEngine().info()

    assert not info.reachable
    assert info.failure_category == "docker_desktop_wedged"
    assert info.client_version == "28.5.1"
    assert "500 Internal Server Error" in (info.detail or "")
    assert info.desktop_evidence == DesktopEvidence(desktop_running=True, wsl_distro_running=True)


def test_engine_info_keeps_server_500_generic_without_windows_desktop_evidence(monkeypatch) -> None:
    class ServerErrorEngine(Engine):
        def available(self) -> bool:
            return True

        def run(self, args, **_kwargs):
            if args == ["version", "--format", "{{.Client.Version}}"]:
                return EngineResult(0, "28.5.1", "")
            if args == ["version", "--format", "{{.Server.Version}}"]:
                return EngineResult(1, "", "request returned 500 Internal Server Error")
            raise AssertionError(f"unexpected engine call: {args}")

    monkeypatch.setattr(
        engine_mod,
        "docker_desktop_evidence",
        lambda: DesktopEvidence(desktop_running=None, wsl_distro_running=None),
    )
    info = ServerErrorEngine().info()

    assert not info.reachable
    assert info.failure_category == "server_error"
    assert info.desktop_evidence == DesktopEvidence(desktop_running=None, wsl_distro_running=None)


def test_engine_execute_relays_stdout_and_stderr_while_running(capfd) -> None:
    code = Engine(sys.executable).execute(
        [
            "-c",
            "import sys; print('live-out', flush=True); "
            "print('live-err', file=sys.stderr, flush=True)",
        ],
        timeout=10,
    )

    assert code == 0
    captured = capfd.readouterr()
    assert "live-out" in captured.out
    assert "live-err" in captured.err


def test_engine_execute_returns_an_ordinary_child_exit_130() -> None:
    assert Engine(sys.executable).execute(["-c", "raise SystemExit(130)"], timeout=10) == 130


def test_run_reports_an_actual_timeout_as_a_deadline(monkeypatch) -> None:
    """`rp.subprocess_run` re-raises a real timeout as `RuntimeError(...) from exc`, with
    `exc` being an `rp.TimeoutExpired`. `Engine.run` must still report this as the deadline
    message -- only the *other* kind of `RuntimeError` (see the sibling test below) changed.
    """
    cause = engine_mod.rp.TimeoutExpired(cmd=["docker", "ps"], timeout=7)
    wrapped = RuntimeError("CRITICAL: Process timed out after 7 seconds: ['docker', 'ps']")
    wrapped.__cause__ = cause

    def fake_subprocess_run(*_args, **_kwargs):
        raise wrapped

    # `run` checks `available()` (a PATH lookup for the binary) before it ever reaches the
    # faked `subprocess_run`, and raises a different EngineError -- "'docker' is not on
    # PATH" -- when that lookup fails. On a machine with Docker installed the fake is
    # reached and this test passes; on one without it (the macOS CI lane) the test would
    # fail against a message it was never about. This test is about how `run` classifies a
    # RuntimeError from the spawn, not about whether the host has Docker, so the PATH
    # question is answered here rather than inherited from the runner.
    monkeypatch.setattr(engine_mod.Engine, "available", lambda _self: True)
    monkeypatch.setattr(engine_mod.rp, "subprocess_run", fake_subprocess_run)
    monkeypatch.setattr(engine_mod.Engine, "_probe_server", lambda *_args: (True, ""))

    with pytest.raises(EngineError, match="7-second deadline") as raised:
        Engine("docker", timeout=7).run(["ps"])

    assert raised.value.__cause__ is wrapped
    assert wrapped.__cause__ is cause


def test_run_timeout_reports_engine_unresponsive_when_probe_also_fails(monkeypatch) -> None:
    cause = engine_mod.rp.TimeoutExpired(cmd=["docker", "ps"], timeout=7)
    wrapped = RuntimeError("timed out")
    wrapped.__cause__ = cause
    monkeypatch.setattr(engine_mod.Engine, "available", lambda _self: True)
    monkeypatch.setattr(
        engine_mod.rp,
        "subprocess_run",
        lambda *_args, **_kwargs: (_ for _ in ()).throw(wrapped),
    )
    monkeypatch.setattr(
        engine_mod.Engine,
        "_probe_server",
        lambda *_args: (False, "server pipe did not respond"),
    )

    with pytest.raises(EngineError, match="down or unresponsive.*bad state.*bosn doctor"):
        Engine("docker", timeout=7).run(["ps"])


def test_run_distinguishes_a_spawn_failure_from_a_timeout(monkeypatch) -> None:
    """Issue #101: a captured traceback showed a missing-binary spawn failure --
    `RuntimeError: failed to spawn process: program not found`, raised immediately, with
    no `TimeoutExpired` anywhere in its cause chain -- reported as
    "exceeded its 60-second deadline". That sends someone debugging a missing binary
    looking for a slow Docker daemon instead. `Engine.run` must describe this failure for
    what it is, and must still chain the original exception so the real cause survives in
    a traceback.
    """
    spawn_failure = RuntimeError("failed to spawn process: program not found")

    def fake_subprocess_run(*_args, **_kwargs):
        raise spawn_failure

    # `run` checks `available()` (a PATH lookup for the binary) before it ever reaches the
    # faked `subprocess_run`, and raises a different EngineError -- "'docker' is not on
    # PATH" -- when that lookup fails. On a machine with Docker installed the fake is
    # reached and this test passes; on one without it (the macOS CI lane) the test would
    # fail against a message it was never about. This test is about how `run` classifies a
    # RuntimeError from the spawn, not about whether the host has Docker, so the PATH
    # question is answered here rather than inherited from the runner.
    monkeypatch.setattr(engine_mod.Engine, "available", lambda _self: True)
    monkeypatch.setattr(engine_mod.rp, "subprocess_run", fake_subprocess_run)

    with pytest.raises(EngineError) as raised:
        Engine("docker", timeout=60).run(["ps"])

    message = str(raised.value)
    assert "exceeded its" not in message
    assert "60-second deadline" not in message
    assert "could not start" in message
    assert "failed to spawn process: program not found" in message
    assert raised.value.__cause__ is spawn_failure


class AbortingProcess:
    finished = False
    returncode: int | None = None
    end_time: float | None = None

    def __init__(self, *_args, error: BaseException, **_kwargs) -> None:
        self.error = error
        self.killed = False

    def wait(self, *, timeout, echo=False):
        del timeout, echo
        raise self.error

    def kill(self) -> None:
        self.killed = True


def test_timeout_cleanup_failure_does_not_mask_the_deadline(monkeypatch) -> None:
    monkeypatch.setattr(
        engine_mod.rp,
        "RunningProcess",
        lambda *_args, **_kwargs: AbortingProcess(error=TimeoutError()),
    )
    engine = FakeEngine()
    monkeypatch.setattr(
        engine,
        "run",
        lambda *_args, **_kwargs: (_ for _ in ()).throw(EngineError("cleanup unavailable")),
    )

    with pytest.raises(EngineError, match="1-second deadline") as raised:
        engine.execute(["exec", "immutable-id", "sleep", "10"], timeout=1, abort_container="id")

    assert "cleanup unavailable" in str(raised.value)


def test_interrupt_cleanup_failure_does_not_mask_keyboard_interrupt(monkeypatch) -> None:
    process = AbortingProcess(error=KeyboardInterrupt())
    monkeypatch.setattr(engine_mod.rp, "RunningProcess", lambda *_args, **_kwargs: process)
    engine = FakeEngine()
    monkeypatch.setattr(
        engine,
        "run",
        lambda *_args, **_kwargs: (_ for _ in ()).throw(EngineError("cleanup unavailable")),
    )

    with pytest.raises(KeyboardInterrupt):
        engine.execute(["exec", "immutable-id", "sleep", "10"], abort_container="id")

    assert process.killed


def test_host_interrupt_with_child_returncode_130_still_aborts_container(monkeypatch) -> None:
    process = AbortingProcess(error=KeyboardInterrupt())
    process.returncode = 130
    process.finished = True
    aborted: list[str | None] = []
    monkeypatch.setattr(engine_mod.rp, "RunningProcess", lambda *_args, **_kwargs: process)
    engine = FakeEngine()
    monkeypatch.setattr(
        engine,
        "_abort_container",
        lambda container_id: aborted.append(container_id),
    )

    with pytest.raises(KeyboardInterrupt):
        engine.execute(["exec", "immutable-id", "sleep", "10"], abort_container="immutable-id")

    assert not process.killed
    assert aborted == ["immutable-id"]
