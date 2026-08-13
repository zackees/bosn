"""Container engine access.

Engine access goes through the `docker` CLI rather than a Docker-specific API binding,
which is what keeps podman cheap as a second target. Commands are always argument lists,
never shell strings.
"""

from __future__ import annotations

import shutil
from dataclasses import dataclass

import running_process as rp

DEFAULT_TIMEOUT = 60


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
