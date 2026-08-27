"""Container engine access.

Engine access goes through the `docker` CLI rather than a Docker-specific API binding,
which is what keeps podman cheap as a second target. Commands are always argument lists,
never shell strings.
"""

from __future__ import annotations

import shutil
import subprocess
import sys
import threading
import time
from collections.abc import Callable
from dataclasses import dataclass
from datetime import datetime
from typing import Any

import running_process as rp

DEFAULT_TIMEOUT = 60
# How often a streaming command checks whether it has been cancelled.
STREAM_POLL_SECONDS = 0.05
CLOCK_SKEW_BUDGET_SECONDS = 1.0
# A quiet Docker command is valid, so silence only schedules a read-only liveness probe.
# Two consecutive failures avoid turning a transient overloaded daemon into a destructive
# false positive.  These bounds detect a wedge in roughly one minute without imposing an
# idle-output deadline on legitimate cold builds or silent tests.
ENGINE_HEALTH_GRACE_SECONDS = 30.0
ENGINE_HEALTH_INTERVAL_SECONDS = 10.0
ENGINE_HEALTH_PROBE_TIMEOUT_SECONDS = 5.0
ENGINE_HEALTH_FAILURE_LIMIT = 2


class EngineError(RuntimeError):
    """The engine could not be reached or refused a command."""


@dataclass(frozen=True)
class EngineResult:
    returncode: int
    stdout: str
    stderr: str

    @property
    def ok(self) -> bool:
        return self.returncode == 0


@dataclass(frozen=True)
class EngineInfo:
    binary: str
    reachable: bool
    client_version: str | None = None
    server_version: str | None = None
    clock_skew_seconds: float | None = None
    detail: str | None = None
    failure_category: str | None = None
    desktop_evidence: DesktopEvidence | None = None


@dataclass(frozen=True)
class DesktopEvidence:
    """Best-effort native Windows observations for a Docker Desktop wedge."""

    desktop_running: bool | None
    wsl_distro_running: bool | None


def _read_only_command(args: list[str]) -> str | None:
    try:
        completed = subprocess.run(args, capture_output=True, text=True, timeout=3, check=False)
    except (OSError, subprocess.SubprocessError):
        return None
    return f"{completed.stdout}\n{completed.stderr}"


def docker_desktop_evidence() -> DesktopEvidence:
    """Read-only, bounded Windows support evidence for an HTTP-5xx server probe."""
    if sys.platform != "win32":
        return DesktopEvidence(desktop_running=None, wsl_distro_running=None)
    desktop = _read_only_command(
        ["tasklist", "/FI", "IMAGENAME eq Docker Desktop.exe", "/FO", "CSV", "/NH"]
    )
    wsl = _read_only_command(["wsl.exe", "--list", "--verbose"])
    return DesktopEvidence(
        desktop_running=("Docker Desktop.exe" in desktop) if desktop is not None else None,
        wsl_distro_running=("docker-desktop" in wsl and "Running" in wsl)
        if wsl is not None
        else None,
    )


def _server_failure_category(detail: str, evidence: DesktopEvidence) -> str:
    lowered = detail.lower()
    http_5xx = ("http" in lowered and "500" in detail) or "500 internal server error" in lowered
    if http_5xx and evidence.desktop_running is True and evidence.wsl_distro_running is True:
        return "docker_desktop_wedged"
    return "server_error"


class _EngineHealthMonitor:
    """Kill one pending local engine CLI after repeated bounded server-probe failures."""

    def __init__(self, engine: Engine, process: Any) -> None:
        self._engine = engine
        self._process = process
        self._stop = threading.Event()
        self._activity = threading.Event()
        self._thread = threading.Thread(target=self._run, name="bosn-engine-health", daemon=True)
        self._captured_bytes = self._output_bytes()
        self.error: str | None = None

    def start(self) -> None:
        self._thread.start()

    def activity(self) -> None:
        self._activity.set()

    def stop(self) -> None:
        self._stop.set()
        self._activity.set()
        self._thread.join(timeout=ENGINE_HEALTH_PROBE_TIMEOUT_SECONDS + 1.0)

    def _wait(self, seconds: float) -> bool:
        """Return true when stopped; activity restarts the conservative grace period."""
        deadline = time.monotonic() + seconds
        while not self._stop.is_set():
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return False
            if self._activity.wait(remaining):
                self._activity.clear()
                deadline = time.monotonic() + ENGINE_HEALTH_GRACE_SECONDS
        return True

    def _run(self) -> None:
        if self._wait(ENGINE_HEALTH_GRACE_SECONDS):
            return
        failures = 0
        details: list[str] = []
        while not self._stop.is_set() and not self._finished():
            if self._output_advanced():
                failures = 0
                details.clear()
                if self._wait(ENGINE_HEALTH_GRACE_SECONDS):
                    return
                continue
            reachable, detail = self._engine._probe_server(ENGINE_HEALTH_PROBE_TIMEOUT_SECONDS)
            if reachable:
                failures = 0
                details.clear()
            else:
                failures += 1
                details.append(detail)
                if failures >= ENGINE_HEALTH_FAILURE_LIMIT:
                    summary = "; ".join(details[-ENGINE_HEALTH_FAILURE_LIMIT:])
                    self.error = self._engine._unresponsive_message(summary)
                    self._process.kill()
                    return
            if self._wait(ENGINE_HEALTH_INTERVAL_SECONDS):
                return

    def _finished(self) -> bool:
        finished = getattr(self._process, "finished", None)
        if finished is not None:
            return bool(finished)
        poll = getattr(self._process, "poll", None)
        return callable(poll) and poll() is not None

    def _output_bytes(self) -> int | None:
        measure = getattr(self._process, "captured_output_bytes", None)
        if not callable(measure):
            return None
        try:
            measured = measure()
            return measured if isinstance(measured, int) else None
        except KeyboardInterrupt:
            raise
        except Exception:  # noqa: BLE001 - optional false-positive guard
            return None

    def _output_advanced(self) -> bool:
        current = self._output_bytes()
        if current is None or self._captured_bytes is None:
            self._captured_bytes = current
            return False
        advanced = current > self._captured_bytes
        self._captured_bytes = current
        return advanced


def _engine_timestamp(value: str) -> float | None:
    """Parse Docker's RFC3339 ``SystemTime`` without making diagnostics fragile."""
    try:
        parsed = datetime.fromisoformat(value.strip().replace("Z", "+00:00"))
        if parsed.tzinfo is None:
            return None
        return parsed.timestamp()
    except (OSError, OverflowError, ValueError):
        return None


class Engine:
    """A thin wrapper over the `docker` (or `podman`) CLI."""

    def __init__(self, binary: str = "docker", timeout: int = DEFAULT_TIMEOUT) -> None:
        self.binary = binary
        self.timeout = timeout

    def available(self) -> bool:
        """True when the engine binary is on PATH. Does not prove the daemon is up."""
        return shutil.which(self.binary) is not None

    def _probe_server(self, timeout: float) -> tuple[bool, str]:
        """Perform one narrow read-only daemon probe, preserving a useful failure reason."""
        if not self.available():
            return False, f"{self.binary!r} is not on PATH"
        try:
            completed = rp.subprocess_run(
                [self.binary, "version", "--format", "{{.Server.Version}}"],
                cwd=None,
                check=False,
                timeout=max(1, int(timeout)),
            )
        except KeyboardInterrupt:
            raise
        except Exception as exc:  # noqa: BLE001 - the monitor reports every probe failure
            return False, str(exc)
        result = EngineResult(
            returncode=completed.returncode,
            stdout=(completed.stdout or "").strip(),
            stderr=(getattr(completed, "stderr", "") or "").strip(),
        )
        if result.ok:
            return True, ""
        return False, result.stderr or result.stdout or "server-version probe failed"

    def _monitor(self, process: Any) -> _EngineHealthMonitor:
        return _EngineHealthMonitor(self, process)

    def _unresponsive_message(self, detail: str) -> str:
        return (
            f"{self.binary} engine is down or unresponsive and may be in a bad state "
            f"({detail}); run `bosn doctor` and recover or restart the engine before retrying"
        )

    def run(
        self, args: list[str], *, check: bool = False, timeout: float | None = None
    ) -> EngineResult:
        if not self.available():
            raise EngineError(f"{self.binary!r} is not on PATH")
        effective_timeout = self.timeout if timeout is None else max(1, int(timeout))
        try:
            completed = rp.subprocess_run(
                [self.binary, *args], cwd=None, check=False, timeout=effective_timeout
            )
        except RuntimeError as exc:
            # `rp.subprocess_run` collapses two very different failures into the same
            # `RuntimeError`: a command that actually ran for `effective_timeout` seconds
            # and was killed, and a command that never started running at all (missing
            # binary, a PATH entry pointing at something not executable, a broken exec
            # environment -- anything the underlying spawn can raise before there is a
            # process to wait on). A captured traceback for issue #101 showed the second
            # case reported as the first: the real cause was
            # `RuntimeError: failed to spawn process: program not found`, raised
            # immediately, but this code used to describe every `RuntimeError` here as
            # "exceeded its N-second deadline" -- sending whoever read that message looking
            # for a slow Docker daemon instead of a missing binary.
            #
            # `subprocess_run` only distinguishes the two internally: it catches
            # `rp.TimeoutExpired` specifically and re-raises it as `RuntimeError(...) from
            # exc`, chaining the original in. Any other failure propagates as whatever it
            # already was (typically a plain `RuntimeError`) with no such cause. Reading
            # `exc.__cause__`'s type recovers that distinction without depending on message
            # wording -- string-matching `subprocess_run`'s message text was considered and
            # rejected as brittle, since a wording change in that dependency would silently
            # break the distinction with no test pinning it here.
            #
            # `available()` above already rules out the binary being absent from PATH at
            # call time, but that check and the actual spawn are not atomic (the binary can
            # disappear, or a PATH entry can point at something non-executable, in between),
            # so this branch still matters even with that guard in place.
            if isinstance(exc.__cause__, rp.TimeoutExpired):
                reachable, detail = self._probe_server(ENGINE_HEALTH_PROBE_TIMEOUT_SECONDS)
                if not reachable:
                    raise EngineError(self._unresponsive_message(detail)) from exc
                raise EngineError(
                    f"{self.binary} {' '.join(args)} exceeded its "
                    f"{effective_timeout}-second deadline"
                ) from exc
            raise EngineError(f"{self.binary} {' '.join(args)} could not start: {exc}") from exc
        result = EngineResult(
            returncode=completed.returncode,
            stdout=(completed.stdout or "").strip(),
            stderr=(getattr(completed, "stderr", "") or "").strip(),
        )
        if check and not result.ok:
            raise EngineError(
                f"{self.binary} {' '.join(args)} failed ({result.returncode}): "
                f"{result.stderr or result.stdout}"
            )
        return result

    def stream(
        self,
        args: list[str],
        *,
        on_line: Callable[[str], None] | None = None,
        cancelled: threading.Event | None = None,
    ) -> EngineResult:
        """Run a command, publishing output line by line, killing it on cancellation.

        Builds go through here rather than `run`. Two reasons, both about the 20-minute
        cold build: its output has to reach a watching client while it happens rather than
        in one lump at the end, and it has to be killable, since `bosn cancel`, daemon
        shutdown, and the job TTL all need a way to stop a build that is already going.

        Deliberately no timeout: the owning job's TTL is the one deadline. `run`'s 60s
        default is right for `docker inspect` and catastrophic for `docker build`, and two
        competing deadlines would just mean the tighter one silently wins.
        """
        if not self.available():
            raise EngineError(f"{self.binary!r} is not on PATH")
        lines: list[str] = []

        def publish(text: str) -> None:
            lines.append(text)
            if on_line is not None:
                on_line(text)

        process = rp.RunningProcess([self.binary, *args], check=False, timeout=None)
        monitor = self._monitor(process)
        monitor.start()
        killed = False
        try:
            while True:
                if cancelled is not None and cancelled.is_set() and not killed:
                    process.kill()
                    killed = True
                line = process.get_next_line_non_blocking()
                if isinstance(line, rp.EndOfStream):
                    break
                if line is None:
                    # `finished` is a property but `has_pending_output` is a method -- calling
                    # one and not the other silently turns this into `not <bound method>`,
                    # i.e. never true, and the drain guard stops guarding anything.
                    if process.finished and not process.has_pending_output():
                        break
                    time.sleep(STREAM_POLL_SECONDS)
                    continue
                publish(str(line))
                monitor.activity()
        finally:
            try:
                # wait() is typed as int | IdleWaitResult because it doubles as an idle
                # detector; with no idle_detector passed it always returns the exit code.
                waited = process.wait(timeout=30)
                returncode = waited if isinstance(waited, int) else -1
            except KeyboardInterrupt:
                raise
            except Exception:  # noqa: BLE001 - a wait that fails must not mask the real outcome
                process.kill()
                returncode = -1
            monitor.stop()

        output = "\n".join(lines)
        if monitor.error is not None:
            raise EngineError(monitor.error)
        if killed:
            return EngineResult(returncode=returncode or -1, stdout=output, stderr="cancelled")
        return EngineResult(returncode=returncode, stdout=output, stderr="")

    def execute(
        self,
        args: list[str],
        *,
        timeout: float | None = None,
        abort_container: str | None = None,
    ) -> int:
        """Run a user command with its stdout and stderr attached to the caller.

        Container commands are different from engine control calls: their output is the
        product the caller asked for, and a long test must stay observable while it runs.
        Tagged capture plus continuous echo preserves the two streams and makes them live.
        If a deadline or Ctrl-C kills the local ``docker exec`` client, removing the
        already-proven immutable container id also kills the remote process; cache volumes
        survive and the next converge recreates the cheap container.
        """
        if not self.available():
            raise EngineError(f"{self.binary!r} is not on PATH")
        effective_timeout = self.timeout if timeout is None else max(1.0, float(timeout))
        # running-process's native `capture=False` mode discards output rather than
        # inheriting the parent's handles. Capture and continuously drain both tagged
        # streams instead: `echo=True` relays each line while the process is still alive,
        # preserving stdout/stderr routing without waiting for command completion.
        process = rp.RunningProcess([self.binary, *args], check=False, capture=True, stderr=rp.PIPE)
        # A child choosing exit 130 is still an ordinary command result. Disable
        # running-process's exit-code-to-KeyboardInterrupt translation for this instance;
        # Python will continue to raise a real KeyboardInterrupt when the parent receives
        # Ctrl-C, giving this method an unambiguous cleanup boundary.
        process.KEYBOARD_INTERRUPT_EXIT_CODES = set()  # pyright: ignore[reportAttributeAccessIssue]
        monitor = self._monitor(process)
        monitor.start()
        try:
            waited = process.wait(timeout=effective_timeout, echo=True)
        except KeyboardInterrupt:
            if not process.finished:
                process.kill()
            self._abort_container(abort_container)
            raise
        except TimeoutError as exc:
            cleanup_error = self._abort_container(abort_container)
            cleanup_suffix = f"; {cleanup_error}" if cleanup_error else ""
            raise EngineError(
                f"{self.binary} {' '.join(args)} exceeded its "
                f"{effective_timeout:g}-second deadline{cleanup_suffix}"
            ) from exc
        finally:
            monitor.stop()
        if monitor.error is not None:
            cleanup_error = self._abort_container(abort_container)
            cleanup_suffix = f"; {cleanup_error}" if cleanup_error else ""
            raise EngineError(f"{monitor.error}{cleanup_suffix}")
        if not isinstance(waited, int):
            raise EngineError(f"{self.binary} {' '.join(args)} ended without an exit status")
        return waited

    def execute_capture(
        self,
        args: list[str],
        *,
        timeout: float | None = None,
    ) -> EngineResult:
        """Run a foreground command with separated capture and runtime health monitoring."""
        if not self.available():
            raise EngineError(f"{self.binary!r} is not on PATH")
        effective_timeout = self.timeout if timeout is None else max(1.0, float(timeout))
        process = rp.RunningProcess([self.binary, *args], check=False, capture=True, stderr=rp.PIPE)
        process.KEYBOARD_INTERRUPT_EXIT_CODES = set()  # pyright: ignore[reportAttributeAccessIssue]
        monitor = self._monitor(process)
        monitor.start()
        try:
            waited = process.wait(timeout=effective_timeout)
        except KeyboardInterrupt:
            if not process.finished:
                process.kill()
            raise
        except TimeoutError as exc:
            raise EngineError(
                f"{self.binary} {' '.join(args)} exceeded its {effective_timeout:g}-second deadline"
            ) from exc
        finally:
            monitor.stop()
        if monitor.error is not None:
            raise EngineError(monitor.error)
        if not isinstance(waited, int):
            raise EngineError(f"{self.binary} {' '.join(args)} ended without an exit status")
        return EngineResult(waited, str(process.stdout).strip(), str(process.stderr).strip())

    def _abort_container(self, container_id: str | None) -> str | None:
        if not container_id:
            return None
        try:
            removed = self.run(
                ["container", "rm", "--force", container_id],
                timeout=10,
            )
        except KeyboardInterrupt:
            raise
        except Exception as exc:  # noqa: BLE001 - the original abort stays authoritative
            return f"failed to stop timed-out container {container_id}: {exc}"
        if removed.ok:
            return None
        detail = removed.stderr or removed.stdout
        if "no such container" in detail.lower():
            return None
        return f"failed to stop timed-out container {container_id}: {detail}"

    def interactive(self, args: list[str]) -> int:
        """Run an engine command attached to this process's terminal.

        Unlike :meth:`run`, stdio is inherited, so Docker receives the caller's stdin,
        terminal resize/signals, and can allocate a real TTY for ``exec -it``.
        """
        if not self.available():
            raise EngineError(f"{self.binary!r} is not on PATH")
        # An inherited TTY provides no portable output-activity signal. Applying the quiet
        # subprocess monitor here would kill a healthy, actively used shell after two
        # transient health-probe failures. Runtime wedge detection therefore covers builds,
        # foreground non-interactive commands, JSON capture, and bounded control calls;
        # interactive recovery remains user-directed via Ctrl-C and `bosn doctor`.
        return subprocess.run([self.binary, *args], check=False).returncode

    def info(self) -> EngineInfo:
        """Probe the engine. Never raises — the result carries the diagnosis."""
        if not self.available():
            return EngineInfo(
                binary=self.binary,
                reachable=False,
                detail=f"{self.binary!r} is not on PATH",
                failure_category="binary_unavailable",
            )
        try:
            client = self.run(["version", "--format", "{{.Client.Version}}"])
            server = self.run(["version", "--format", "{{.Server.Version}}"])
        except KeyboardInterrupt:
            raise
        except Exception as exc:  # noqa: BLE001 - doctor must never crash
            detail = str(exc)
            return EngineInfo(
                binary=self.binary,
                reachable=False,
                detail=detail,
                failure_category="deadline" if "deadline" in detail else "probe_error",
            )
        if not server.ok:
            detail = server.stderr or server.stdout or "engine daemon unreachable"
            evidence = docker_desktop_evidence()
            return EngineInfo(
                binary=self.binary,
                reachable=False,
                client_version=client.stdout or None,
                detail=detail,
                failure_category=_server_failure_category(detail, evidence),
                desktop_evidence=evidence,
            )
        # ``docker info`` exposes the daemon's own nanosecond wall clock. Sampling the
        # client clock on both sides and comparing with the midpoint removes ordinary CLI
        # round-trip time from the estimate. Unlike launching a probe container, this is
        # read-only, creates no resource bosn would then need to govern, and does not round
        # away the sub-second precision that decides whether an incremental builder is safe.
        try:
            started_at = time.time()
            system_time = self.run(["info", "--format", "{{.SystemTime}}"])
            finished_at = time.time()
            engine_time = _engine_timestamp(system_time.stdout) if system_time.ok else None
            clock_skew = (
                engine_time - ((started_at + finished_at) / 2) if engine_time is not None else None
            )
        except KeyboardInterrupt:
            raise
        except Exception:  # noqa: BLE001 - optional diagnostics must not hide reachability
            clock_skew = None
        return EngineInfo(
            binary=self.binary,
            reachable=True,
            client_version=client.stdout or None,
            server_version=server.stdout or None,
            clock_skew_seconds=clock_skew,
        )


def engine_reachable(binary: str = "docker") -> bool:
    """Convenience probe used to skip Docker-backed tests when no engine is present."""
    return Engine(binary, timeout=20).info().reachable
