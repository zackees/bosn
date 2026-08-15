"""Unit coverage for engine diagnostics that must not require a live daemon."""

from __future__ import annotations

import sys

import pytest

from bosn import engine as engine_mod
from bosn.engine import Engine, EngineError, EngineResult


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
