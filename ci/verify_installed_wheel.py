#!/usr/bin/env python3
"""Install one Bosn wheel in an isolated venv and exercise its native surface.

This deliberately runs the installed entry point from outside the checkout.
It is suitable for a locally built wheel as well as the Linux, macOS, and
Windows native-wheel CI lanes:

    python ci/verify_installed_wheel.py dist/bosn-*.whl
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
import textwrap
import time
import zipfile
from pathlib import Path
from typing import BinaryIO, NoReturn

RETIRED_LIFECYCLE_MODULES = (
    "daemon",
    "engine",
    "registry",
    "converge",
    "gc",
    "resources",
    "recovery",
    "docker_cli",
    "compose",
    "manifest",
    "guest",
)

# The smoke lane must fail diagnostically rather than leave a platform runner
# occupied forever when a child process or its local IPC transport wedges.
CLI_TIMEOUT_SECONDS = 10
INSTALL_TIMEOUT_SECONDS = 120
REAP_TIMEOUT_SECONDS = 10


def fail(message: str) -> NoReturn:
    raise AssertionError(message)


def phase(name: str) -> None:
    """Emit an immediate, compact CI progress marker."""

    print(f"[wheel-smoke] {name}", flush=True)


def tail(value: str | bytes | None, limit: int = 2048) -> str:
    if value is None:
        return ""
    if isinstance(value, bytes):
        value = value.decode(errors="replace")
    return value[-limit:]


def read_output(stream: BinaryIO | None) -> str:
    """Read a temporary binary stream without relying on a child pipe closing."""

    # ``TemporaryFile`` is intentionally passed directly to child processes.
    # On Windows a spawned descendant may retain a pipe handle after the
    # daemon parent exits, which makes ``communicate()`` wait forever.  A
    # regular file lets us inspect diagnostics without that lifecycle edge.
    if stream is None:
        return ""
    stream.seek(0)
    value = stream.read()
    return value.decode(errors="replace") if isinstance(value, bytes) else str(value)


def daemon_detail(
    daemon: subprocess.Popen[bytes] | None,
    daemon_log: BinaryIO | None = None,
) -> str:
    if daemon is None:
        return "not started"
    if daemon.poll() is None:
        return f"still running; log={tail(read_output(daemon_log))!r}"
    return f"exited={daemon.returncode}; log={tail(read_output(daemon_log))!r}"


def timeout_detail(
    error: subprocess.TimeoutExpired,
    *,
    state: Path | None = None,
    daemon: subprocess.Popen[bytes] | None = None,
    daemon_log: BinaryIO | None = None,
) -> str:
    detail = (
        f"command={error.cmd!r}; timeout={error.timeout}s; "
        f"stdout={tail(error.output)!r}; stderr={tail(error.stderr)!r}"
    )
    if state is not None:
        socket_candidate = state / "bosn-rs.sock"
        detail += (
            f"; state={state}; state_exists={state.exists()}; "
            f"socket_candidate_length={len(os.fsencode(socket_candidate))}"
        )
    if daemon is not None:
        detail += f"; daemon={daemon_detail(daemon, daemon_log)}"
    return detail


def platform_executable_suffix() -> str:
    return ".exe" if os.name == "nt" else ""


def platform_extension_suffix() -> str:
    """The one extension suffix ABI3 promises across supported Python hosts."""

    return ".abi3.pyd" if os.name == "nt" else ".abi3.so"


def smoke_temporary_directory() -> str | None:
    """Keep macOS Unix-domain socket names within its small path budget.

    The daemon's portable IPC endpoint is derived beneath the supplied state
    directory.  GitHub's macOS temporary root is already long enough that a
    normal ``TemporaryDirectory`` name makes ``state/bosn-rs.sock`` exceed the
    platform's Unix-domain socket limit before the daemon can bind it.
    """

    return "/tmp" if sys.platform == "darwin" else None


def smoke_temporary_prefix() -> str:
    """Reserve the shortest practical state-root spelling on macOS.

    Darwin's filesystem local-socket transport has a much smaller endpoint
    budget than Linux.  The installed-wheel test intentionally keeps its
    working directory outside the checkout, but the daemon state need not be
    nested there.  A short private temporary root leaves room for the socket
    file and for platform/library path normalization.
    """

    return "b-" if sys.platform == "darwin" else "bosn-wheel-smoke-"


def wheel_from_argument(argument: str) -> Path:
    candidates = (
        sorted(Path().glob(argument))
        if any(char in argument for char in "*?[")
        else [Path(argument)]
    )
    if len(candidates) != 1 or not candidates[0].is_file():
        fail(f"expected exactly one wheel, got: {candidates}")
    return candidates[0].resolve()


def assert_platform_wheel_contents(wheel: Path) -> None:
    extension = f"bosn/_native{platform_extension_suffix()}"
    executable = f".data/platlib/bosn/_bin/bosn-native{platform_executable_suffix()}"
    with zipfile.ZipFile(wheel) as archive:
        names = archive.namelist()
    retired = [
        f"bosn/{module}.py" for module in RETIRED_LIFECYCLE_MODULES if f"bosn/{module}.py" in names
    ]
    if retired:
        fail(f"wheel includes retired Python lifecycle modules: {retired}")
    if extension not in names:
        fail(f"wheel is missing this platform extension {extension!r}: {names}")
    if not any(name.endswith(executable) for name in names):
        fail(f"wheel is missing this platform executable {executable!r}: {names}")


def child_environment(scripts: Path) -> dict[str, str]:
    """Remove checkout/toolchain imports while retaining OS runtime programs."""

    environment = dict(os.environ)
    for key in (
        "PYTHONHOME",
        "PYTHONPATH",
        "VIRTUAL_ENV",
        "CONDA_PREFIX",
        "CARGO_HOME",
        "RUSTUP_HOME",
    ):
        environment.pop(key, None)
    environment["PYTHONNOUSERSITE"] = "1"
    if os.name == "nt":
        system_root = environment.get("SystemRoot", r"C:\\Windows")
        runtime_paths = [str(scripts), str(Path(system_root) / "System32"), system_root]
    else:
        runtime_paths = [str(scripts), "/usr/local/bin", "/usr/bin", "/bin"]
    environment["PATH"] = os.pathsep.join(runtime_paths)
    # A reachable Docker engine is neither required nor permitted by this
    # smoke test.  Doctor reports the resulting unavailable engine state.
    environment["DOCKER_HOST"] = "tcp://127.0.0.1:1"
    return environment


def bootstrap_environment() -> dict[str, str]:
    """Keep venv and uv isolated from a caller's Python environment too."""

    environment = dict(os.environ)
    for key in ("PYTHONHOME", "PYTHONPATH", "VIRTUAL_ENV", "CONDA_PREFIX"):
        environment.pop(key, None)
    environment["PYTHONNOUSERSITE"] = "1"
    return environment


def run(
    command: list[str],
    *,
    cwd: Path,
    env: dict[str, str],
    check: bool = True,
    timeout: int = CLI_TIMEOUT_SECONDS,
    state: Path | None = None,
    daemon: subprocess.Popen[bytes] | None = None,
    daemon_log: BinaryIO | None = None,
) -> subprocess.CompletedProcess[str]:
    # Do not use PIPE here.  A process which launches another process can
    # leave a pipe write handle alive on Windows after its own exit, defeating
    # subprocess.run(timeout=...) during its implicit communicate() cleanup.
    # File-backed output gives the same diagnostics while wait() remains
    # bounded by the direct child alone.
    with tempfile.TemporaryFile(mode="w+b") as stdout, tempfile.TemporaryFile(mode="w+b") as stderr:
        process = subprocess.Popen(
            command,
            cwd=cwd,
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=stdout,
            stderr=stderr,
        )
        try:
            returncode = process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            try:
                process.kill()
            except ProcessLookupError:
                # A process may win the race between wait's timeout and kill.
                pass
            try:
                process.wait(timeout=REAP_TIMEOUT_SECONDS)
            except subprocess.TimeoutExpired:
                fail(
                    "installed-wheel command could not be reaped: "
                    f"command={command!r}; timeout={timeout}s; "
                    f"stdout={tail(read_output(stdout))!r}; stderr={tail(read_output(stderr))!r}; "
                    f"daemon={daemon_detail(daemon, daemon_log)}"
                )
            error = subprocess.TimeoutExpired(
                command, timeout, read_output(stdout), read_output(stderr)
            )
            fail(
                "installed-wheel command timed out: "
                f"{timeout_detail(error, state=state, daemon=daemon, daemon_log=daemon_log)}"
            )
        output = read_output(stdout)
        error_output = read_output(stderr)
    completed = subprocess.CompletedProcess(command, returncode, output, error_output)
    if check and returncode != 0:
        fail(
            "installed-wheel command failed: "
            f"command={command!r}; exit={returncode}; "
            f"stdout={tail(output)!r}; stderr={tail(error_output)!r}"
        )
    return completed


def json_output(
    command: list[str],
    *,
    cwd: Path,
    env: dict[str, str],
    state: Path | None = None,
    daemon: subprocess.Popen[bytes] | None = None,
    daemon_log: BinaryIO | None = None,
) -> dict[str, object]:
    result = run(
        command,
        cwd=cwd,
        env=env,
        state=state,
        daemon=daemon,
        daemon_log=daemon_log,
    )
    try:
        value = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        fail(f"{command!r} did not emit JSON: {result.stdout!r} ({error})")
    if not isinstance(value, dict):
        fail(f"{command!r} emitted a non-object JSON value: {value!r}")
    return value


def wait_for_daemon(
    cli: Path,
    state: Path,
    *,
    cwd: Path,
    env: dict[str, str],
    daemon: subprocess.Popen[bytes],
    daemon_log: BinaryIO,
) -> None:
    deadline = time.monotonic() + 10
    last_status = "no status request was attempted"
    while time.monotonic() < deadline:
        if daemon.poll() is not None:
            fail(
                f"installed daemon exited early ({daemon.returncode}): "
                f"{daemon_detail(daemon, daemon_log)}"
            )
        try:
            status = run(
                [str(cli), "daemon", "status", "--state-dir", str(state), "--json"],
                cwd=cwd,
                env=env,
                check=False,
                state=state,
                daemon=daemon,
                daemon_log=daemon_log,
            )
        except AssertionError as error:
            fail(f"installed daemon readiness probe failed: {error}")
        last_status = (
            f"exit={status.returncode}, stdout={status.stdout[-512:]!r}, "
            f"stderr={status.stderr[-512:]!r}"
        )
        if status.returncode == 0:
            value = json.loads(status.stdout)
            if value.get("action") == "daemon_status" and value.get("daemon") == "online":
                return
        time.sleep(0.05)
    detail = daemon_detail(daemon, daemon_log)
    socket_candidate = state / "bosn-rs.sock"
    fail(
        "installed daemon did not become ready; "
        f"daemon={detail}; "
        f"socket_candidate_length={len(os.fsencode(socket_candidate))}; "
        f"last_status={last_status}"
    )


def verify_installed_wheel(wheel: Path) -> None:
    phase("inspect wheel archive")
    assert_platform_wheel_contents(wheel)
    uv = shutil.which("uv")
    if uv is None:
        fail("uv is required to install the wheel")

    with tempfile.TemporaryDirectory(
        prefix=smoke_temporary_prefix(), dir=smoke_temporary_directory()
    ) as temporary:
        root = Path(temporary)
        environment = root / "environment"
        workdir = root / "outside-checkout"
        # Keep the daemon state in the temporary root itself.  ``workdir`` is
        # still outside the checkout, so this remains an installed-wheel test
        # rather than a source-tree import, while macOS gets a safely short
        # filesystem socket endpoint.
        state = root / "state"
        workdir.mkdir()
        phase("create isolated virtual environment")
        run(
            [sys.executable, "-m", "venv", str(environment)],
            cwd=root,
            env=bootstrap_environment(),
            timeout=INSTALL_TIMEOUT_SECONDS,
        )
        python = environment / ("Scripts/python.exe" if os.name == "nt" else "bin/python")
        scripts = environment / ("Scripts" if os.name == "nt" else "bin")
        env = child_environment(scripts)
        phase("install wheel")
        run(
            [uv, "pip", "install", "--python", str(python), "--no-deps", str(wheel)],
            cwd=root,
            env=env,
            timeout=INSTALL_TIMEOUT_SECONDS,
        )
        phase("import installed extension")
        imported = run(
            [
                str(python),
                "-c",
                textwrap.dedent(
                    """
                    import importlib.util
                    import json
                    import os
                    import pathlib
                    import sys

                    import bosn
                    from bosn.native_cli import native_executable

                    origin = pathlib.Path(importlib.util.find_spec("bosn._native").origin)
                    executable = native_executable()
                    assert pathlib.Path(bosn.__file__).resolve().is_relative_to(
                        pathlib.Path(sys.prefix).resolve()
                    )
                    assert origin.name == "_native.abi3" + (".pyd" if os.name == "nt" else ".so")
                    assert executable.is_file()
                    assert executable.name == "bosn-native" + (".exe" if os.name == "nt" else "")
                    versions = {
                        "package_version": bosn.__version__,
                        "native_version": bosn.native_version(),
                    }
                    print(json.dumps(versions))
                    """
                ),
            ],
            cwd=workdir,
            env=env,
        )
        versions = json.loads(imported.stdout)
        if versions["package_version"] != versions["native_version"]:
            fail(f"Python and native versions differ: {versions}")

        cli = scripts / f"bosn{platform_executable_suffix()}"
        phase("verify installed CLI version")
        version = run([str(cli), "--version"], cwd=workdir, env=env)
        if version.stdout != f"bosn {versions['package_version']}\n":
            fail(f"installed CLI reported an unexpected version: {version.stdout!r}")

        phase("verify offline doctor")
        offline_doctor = json_output(
            [str(cli), "doctor", "--state-dir", str(state), "--json"], cwd=workdir, env=env
        )
        if (
            offline_doctor.get("action") != "doctor"
            or offline_doctor.get("daemon") != "unavailable"
        ):
            fail(f"doctor did not report a missing daemon: {offline_doctor}")
        if state.exists():
            fail("doctor initialized daemon state")

        phase("start daemon")
        # Keep daemon diagnostics in a real file rather than a PIPE.  See
        # ``run``: inherited Windows pipe handles can otherwise make the
        # verifier's final communicate() wait unbounded.
        with tempfile.TemporaryFile(mode="w+b") as daemon_log:
            daemon = subprocess.Popen(
                [str(cli), "daemon", "serve", "--state-dir", str(state)],
                cwd=workdir,
                env=env,
                stdin=subprocess.DEVNULL,
                stdout=daemon_log,
                stderr=subprocess.STDOUT,
            )
            try:
                phase("wait for daemon readiness")
                wait_for_daemon(
                    cli,
                    state,
                    cwd=workdir,
                    env=env,
                    daemon=daemon,
                    daemon_log=daemon_log,
                )
                phase("verify online doctor")
                doctor = json_output(
                    [str(cli), "doctor", "--state-dir", str(state), "--json"],
                    cwd=workdir,
                    env=env,
                    state=state,
                    daemon=daemon,
                    daemon_log=daemon_log,
                )
                if doctor.get("action") != "doctor" or doctor.get("daemon") != "ready":
                    fail(f"doctor did not inspect the installed daemon: {doctor}")
                phase("stop daemon")
                stopped = json_output(
                    [str(cli), "daemon", "stop", "--state-dir", str(state), "--json"],
                    cwd=workdir,
                    env=env,
                    state=state,
                    daemon=daemon,
                    daemon_log=daemon_log,
                )
                if stopped != {"action": "daemon_stop", "stopped": True}:
                    fail(f"installed daemon did not stop cleanly: {stopped}")
                try:
                    daemon_exit = daemon.wait(timeout=REAP_TIMEOUT_SECONDS)
                except subprocess.TimeoutExpired:
                    fail(
                        "installed daemon did not exit after a successful stop request: "
                        f"{daemon_detail(daemon, daemon_log)}"
                    )
                if daemon_exit != 0:
                    fail(
                        "installed daemon failed while stopping: "
                        f"{daemon_detail(daemon, daemon_log)}"
                    )
            finally:
                if daemon.poll() is None:
                    phase("force-reap daemon")
                    try:
                        daemon.kill()
                    except ProcessLookupError:
                        pass
                    try:
                        daemon.wait(timeout=REAP_TIMEOUT_SECONDS)
                    except subprocess.TimeoutExpired:
                        fail(
                            "installed daemon could not be reaped: "
                            f"{daemon_detail(daemon, daemon_log)}"
                        )
        phase("complete")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "wheel", help="wheel path, or a shell-style path relative to the current directory"
    )
    arguments = parser.parse_args()
    verify_installed_wheel(wheel_from_argument(arguments.wheel))


if __name__ == "__main__":
    main()
