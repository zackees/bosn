"""Measure the native Bosn performance comparison surface.

This is a local comparison tool, not a CI gate.  It writes one stable JSON
document to stdout and never includes command lines, paths, environment values,
or child process output.  Docker work is deliberately opt-in: without
``--docker``, the setup-ensure-reuse metric is emitted as unsupported.

Build a native binary first, then run for example::

    soldr cargo build -p bosn-service --bin bosn --locked
    python ci/native_performance_baseline.py --binary target/debug/bosn
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import re
import secrets
import statistics
import subprocess
import sys
import tempfile
import time
from collections.abc import Callable, Sequence
from pathlib import Path
from typing import Protocol

DEFAULT_SAMPLES = 7
MAX_SAMPLES = 30
READY_TIMEOUT_SECONDS = 10.0
JOB_TIMEOUT_SECONDS = 90.0
COMMAND_TIMEOUT_SECONDS = 120.0
PINNED_ALPINE = "alpine@sha256:28bd5fe8b56d1bd048e5babf5b10710ebe0bae67db86916198a6eec434943f8b"


class MeasurementError(RuntimeError):
    """A deliberately redacted measurement failure."""


class MetricSource(Protocol):
    """Small seam used by hermetic tests; production uses NativeBenchmark."""

    def cli_startup(self, samples: int) -> dict[str, object]: ...

    def daemon_idle_rss(self) -> dict[str, object]: ...

    def daemon_status(self, samples: int) -> dict[str, object]: ...

    def setup_ensure_reuse(self, samples: int) -> dict[str, object]: ...


def _milliseconds(value: float) -> float:
    # A fixed precision keeps diffs useful while avoiding platform-specific
    # JSON representations of long floating-point tails.
    return round(value, 3)


def latency_metric(samples: Sequence[float]) -> dict[str, object]:
    """Return the stable form shared by all latency metrics."""

    if not samples or len(samples) > MAX_SAMPLES:
        raise ValueError("sample count is outside the supported bound")
    values = [_milliseconds(value) for value in samples]
    return {
        "status": "measured",
        "unit": "ms",
        "samples": values,
        "median": _milliseconds(statistics.median(values)),
    }


def unsupported_metric(reason: str) -> dict[str, object]:
    return {"status": "unsupported", "reason": reason}


def collect_metrics(source: MetricSource, samples: int, docker: bool) -> dict[str, object]:
    """Collect every required metric through a production or fixture source."""

    if not 1 <= samples <= MAX_SAMPLES:
        raise ValueError(f"samples must be between 1 and {MAX_SAMPLES}")
    return {
        "native_cli_startup": source.cli_startup(samples),
        "idle_daemon_rss": source.daemon_idle_rss(),
        "daemon_status_latency": source.daemon_status(samples),
        "setup_ensure_reuse_latency": (
            source.setup_ensure_reuse(samples)
            if docker
            else unsupported_metric("requires_docker_opt_in")
        ),
    }


def report(
    metrics: dict[str, object], samples: int, docker: bool, version: str
) -> dict[str, object]:
    """Build the public JSON schema.  Keep host identity intentionally coarse."""

    return {
        "schema_version": 1,
        "implementation": {"kind": "native", "version": version},
        "platform": {"system": platform.system().lower(), "machine": platform.machine()},
        "measurement": {
            "samples_requested": samples,
            "docker_opt_in": docker,
            "caveats": [
                "wall_clock_measurements; compare only on a similarly loaded host",
                "caches_are_not_cleared",
                "setup_ensure_reuse_includes_job_completion_not_only_submission",
            ],
        },
        "metrics": metrics,
    }


class NativeBenchmark:
    """A disposable native CLI/daemon measurement session."""

    def __init__(self, binary: Path, docker: bool) -> None:
        self.binary = binary
        self.docker = docker
        self._temporary: tempfile.TemporaryDirectory[str] | None = None
        self.root: Path | None = None
        self.state: Path | None = None
        self.workspace: Path | None = None
        self.config: Path | None = None
        self.daemon: subprocess.Popen[bytes] | None = None
        self.container_name: str | None = None
        self.container_content: str | None = None

    def __enter__(self) -> NativeBenchmark:
        if not self.binary.is_file():
            raise MeasurementError("native binary is unavailable")
        try:
            self._temporary = tempfile.TemporaryDirectory(prefix="bosn-native-performance-")
            self.root = Path(self._temporary.name)
            self.state = self.root / "state"
            self.workspace = self.root / "workspace"
            self.workspace.mkdir()
            self.daemon = subprocess.Popen(
                [self.binary, "daemon", "serve", "--state-dir", self.state],
                stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                env=self._environment(),
            )
            self._wait_until_ready()
            if self.docker:
                self._prepare_docker_fixture()
        except KeyboardInterrupt:
            self.__exit__(None, None, None)
            raise
        except BaseException:
            # A context manager whose enter step fails is never given
            # __exit__ by ``with``.  Keep its daemon/container lifetime just
            # as narrow on a failed readiness or Docker setup probe.
            self.__exit__(None, None, None)
            raise
        return self

    def __exit__(self, _type: object, _value: object, _traceback: object) -> None:
        try:
            self._cleanup_container()
        finally:
            self._stop_daemon()
            if self._temporary is not None:
                self._temporary.cleanup()

    def _environment(self) -> dict[str, str]:
        environment = dict(os.environ)
        if not self.docker:
            # Make an accidental engine call deterministic without changing a
            # user's real Docker configuration in opt-in mode.
            environment["DOCKER_HOST"] = "tcp://127.0.0.1:1"
        return environment

    def _require_paths(self) -> tuple[Path, Path]:
        if self.state is None or self.workspace is None:
            raise MeasurementError("measurement session is unavailable")
        return self.state, self.workspace

    def _run(self, arguments: Sequence[str], timeout: float = COMMAND_TIMEOUT_SECONDS) -> bytes:
        try:
            completed = subprocess.run(
                [self.binary, *arguments],
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL,
                check=False,
                timeout=timeout,
                env=self._environment(),
            )
        except (OSError, subprocess.TimeoutExpired) as error:
            raise MeasurementError("native command could not be measured") from error
        if completed.returncode != 0:
            raise MeasurementError("native command failed during measurement")
        return completed.stdout

    def _measure(self, action: Callable[[], object], samples: int) -> dict[str, object]:
        values: list[float] = []
        for _ in range(samples):
            started = time.perf_counter()
            action()
            values.append((time.perf_counter() - started) * 1000)
        return latency_metric(values)

    def _wait_until_ready(self) -> None:
        state, _workspace = self._require_paths()
        deadline = time.monotonic() + READY_TIMEOUT_SECONDS
        while time.monotonic() < deadline:
            if self.daemon is None or self.daemon.poll() is not None:
                raise MeasurementError("native daemon stopped before readiness")
            try:
                output = self._run(["daemon", "status", "--state-dir", str(state), "--json"])
                value = json.loads(output)
                if value.get("action") == "daemon_status" and value.get("daemon") == "online":
                    return
            except (MeasurementError, json.JSONDecodeError):
                pass
            time.sleep(0.02)
        raise MeasurementError("native daemon did not become ready")

    def _stop_daemon(self) -> None:
        if self.daemon is None:
            return
        if self.daemon.poll() is None and self.state is not None:
            try:
                self._run(["daemon", "stop", "--state-dir", str(self.state), "--json"], timeout=10)
            except MeasurementError:
                pass
        try:
            self.daemon.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.daemon.kill()
            self.daemon.wait(timeout=5)

    def _prepare_docker_fixture(self) -> None:
        state, workspace = self._require_paths()
        if self.root is None:
            raise MeasurementError("measurement session is unavailable")
        self.config = self.root / "setup.toml"
        nonce = secrets.token_hex(12)
        self.config.write_text(
            "version = 1\n[app]\n"
            f"image = '{PINNED_ALPINE}'\n"
            f"command = 'exec sleep 120 # bosn-performance-{nonce}'\n",
            encoding="utf-8",
        )
        plan = self._run(
            [
                "setup",
                "plan",
                "--state-dir",
                str(state),
                "--workspace",
                str(workspace),
                "--config",
                str(self.config),
                "--refresh",
                "--json",
            ]
        )
        try:
            content = json.loads(plan)["content_sha256"]
        except (KeyError, TypeError, json.JSONDecodeError) as error:
            raise MeasurementError("native setup plan returned an invalid receipt") from error
        if not isinstance(content, str) or not re.fullmatch(r"[0-9a-f]{64}", content):
            raise MeasurementError("native setup plan returned an invalid receipt")
        self.container_content = content
        self.container_name = f"bosn-setup-{content}"
        # The initial job creates the disposable app.  Only subsequent full
        # job completions are recorded as reuse samples.
        self._submit_and_wait_ensure()

    def _submit_and_wait_ensure(self) -> None:
        state, workspace = self._require_paths()
        if self.config is None:
            raise MeasurementError("docker fixture is unavailable")
        output = self._run(
            [
                "setup",
                "ensure",
                "--state-dir",
                str(state),
                "--workspace",
                str(workspace),
                "--config",
                str(self.config),
                "--refresh",
                "--deadline-ms",
                "90000",
                "--output-limit",
                "1048576",
                "--json",
            ]
        )
        try:
            job_id = json.loads(output)["job_id"]
        except (KeyError, TypeError, json.JSONDecodeError) as error:
            raise MeasurementError("native setup ensure returned an invalid receipt") from error
        if not isinstance(job_id, int) or job_id < 1:
            raise MeasurementError("native setup ensure returned an invalid receipt")
        deadline = time.monotonic() + JOB_TIMEOUT_SECONDS
        while time.monotonic() < deadline:
            status = self._run(
                ["job", "status", "--state-dir", str(state), "--job-id", str(job_id), "--json"]
            )
            try:
                state_name = json.loads(status)["job"]["state"]
            except (KeyError, TypeError, json.JSONDecodeError) as error:
                raise MeasurementError("native job returned an invalid receipt") from error
            if state_name == "Succeeded":
                return
            if state_name in {"Failed", "Cancelled", "Superseded"}:
                raise MeasurementError("native setup ensure did not succeed")
            time.sleep(0.025)
        raise MeasurementError("native setup ensure timed out")

    def _cleanup_container(self) -> None:
        if not self.docker or self.container_name is None or self.container_content is None:
            return
        # Exact name plus all ownership proof values; no selector, prune, or
        # result-derived target can remove a non-fixture container.
        try:
            inspected = subprocess.run(
                [
                    "docker",
                    "container",
                    "inspect",
                    "--format",
                    '{{index .Config.Labels "com.zackees.bosn.setup-managed"}}\t'
                    '{{index .Config.Labels "com.zackees.bosn.setup-content-sha256"}}\t'
                    '{{index .Config.Labels "com.zackees.bosn.setup-container"}}',
                    self.container_name,
                ],
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL,
                check=False,
                timeout=10,
                env=self._environment(),
            )
            expected = f"v1\t{self.container_content}\t{self.container_name}\n".encode()
            if inspected.returncode == 0 and inspected.stdout == expected:
                subprocess.run(
                    ["docker", "container", "rm", "--force", self.container_name],
                    stdin=subprocess.DEVNULL,
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                    check=False,
                    timeout=10,
                    env=self._environment(),
                )
        except (OSError, subprocess.TimeoutExpired):
            # Cleanup must not mask a completed benchmark or broaden its scope.
            pass

    def cli_startup(self, samples: int) -> dict[str, object]:
        return self._measure(lambda: self._run(["--version"]), samples)

    def daemon_idle_rss(self) -> dict[str, object]:
        if not sys.platform.startswith("linux"):
            return unsupported_metric("rss_proc_status_is_linux_only") | {"platform": sys.platform}
        if self.daemon is None:
            raise MeasurementError("native daemon is unavailable")
        status = Path(f"/proc/{self.daemon.pid}/status")
        try:
            for line in status.read_text(encoding="utf-8").splitlines():
                if line.startswith("VmRSS:"):
                    value = int(line.split()[1])
                    return {"status": "measured", "unit": "KiB", "value": value}
        except (OSError, ValueError, IndexError):
            pass
        return unsupported_metric("rss_proc_status_is_unavailable") | {"platform": "linux"}

    def daemon_status(self, samples: int) -> dict[str, object]:
        state, _workspace = self._require_paths()

        def status() -> None:
            value = json.loads(self._run(["daemon", "status", "--state-dir", str(state), "--json"]))
            if value.get("action") != "daemon_status" or value.get("daemon") != "online":
                raise MeasurementError("native daemon status returned an invalid receipt")

        return self._measure(status, samples)

    def setup_ensure_reuse(self, samples: int) -> dict[str, object]:
        if not self.docker:
            return unsupported_metric("requires_docker_opt_in")
        return self._measure(self._submit_and_wait_ensure, samples)


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path("target/debug/bosn"))
    parser.add_argument("--samples", type=int, default=DEFAULT_SAMPLES)
    parser.add_argument(
        "--docker", action="store_true", help="run the real Docker ensure-reuse probe"
    )
    return parser.parse_args()


def native_version(binary: Path) -> str:
    try:
        completed = subprocess.run(
            [binary, "--version"],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            check=False,
            timeout=COMMAND_TIMEOUT_SECONDS,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise MeasurementError("native binary is unavailable") from error
    if completed.returncode != 0:
        raise MeasurementError("native binary version probe failed")
    match = re.fullmatch(rb"bosn ([0-9][0-9A-Za-z.+-]*)\s*", completed.stdout)
    if match is None:
        raise MeasurementError("native binary returned an invalid version")
    return match.group(1).decode("ascii")


def main() -> int:
    args = parse_arguments()
    if not 1 <= args.samples <= MAX_SAMPLES:
        raise SystemExit(f"--samples must be between 1 and {MAX_SAMPLES}")
    try:
        version = native_version(args.binary)
        with NativeBenchmark(args.binary, args.docker) as benchmark:
            metrics = collect_metrics(benchmark, args.samples, args.docker)
    except MeasurementError:
        print(json.dumps({"schema_version": 1, "error": "measurement_failed"}, sort_keys=True))
        return 1
    print(json.dumps(report(metrics, args.samples, args.docker, version), sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
