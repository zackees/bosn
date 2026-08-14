"""Container engine access.

Engine access goes through the `docker` CLI rather than a Docker-specific API binding,
which is what keeps podman cheap as a second target. Commands are always argument lists,
never shell strings.
"""

from __future__ import annotations

import shutil
import threading
import time
from collections.abc import Callable
from dataclasses import dataclass

import running_process as rp

DEFAULT_TIMEOUT = 60
# How often a streaming command checks whether it has been cancelled.
STREAM_POLL_SECONDS = 0.05


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
    detail: str | None = None


class Engine:
    """A thin wrapper over the `docker` (or `podman`) CLI."""

    def __init__(self, binary: str = "docker", timeout: int = DEFAULT_TIMEOUT) -> None:
        self.binary = binary
        self.timeout = timeout

    def available(self) -> bool:
        """True when the engine binary is on PATH. Does not prove the daemon is up."""
        return shutil.which(self.binary) is not None

    def run(self, args: list[str], *, check: bool = False) -> EngineResult:
        if not self.available():
            raise EngineError(f"{self.binary!r} is not on PATH")
        completed = rp.subprocess_run(
            [self.binary, *args],
            cwd=None,
            check=False,
            timeout=self.timeout,
        )
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

        output = "\n".join(lines)
        if killed:
            return EngineResult(returncode=returncode or -1, stdout=output, stderr="cancelled")
        return EngineResult(returncode=returncode, stdout=output, stderr="")

    def info(self) -> EngineInfo:
        """Probe the engine. Never raises — the result carries the diagnosis."""
        if not self.available():
            return EngineInfo(
                binary=self.binary,
                reachable=False,
                detail=f"{self.binary!r} is not on PATH",
            )
        try:
            client = self.run(["version", "--format", "{{.Client.Version}}"])
            server = self.run(["version", "--format", "{{.Server.Version}}"])
        except KeyboardInterrupt:
            raise
        except Exception as exc:  # noqa: BLE001 - doctor must never crash
            return EngineInfo(binary=self.binary, reachable=False, detail=str(exc))
        if not server.ok:
            return EngineInfo(
                binary=self.binary,
                reachable=False,
                client_version=client.stdout or None,
                detail=server.stderr or server.stdout or "engine daemon unreachable",
            )
        return EngineInfo(
            binary=self.binary,
            reachable=True,
            client_version=client.stdout or None,
            server_version=server.stdout or None,
        )


def engine_reachable(binary: str = "docker") -> bool:
    """Convenience probe used to skip Docker-backed tests when no engine is present."""
    return Engine(binary, timeout=20).info().reachable
