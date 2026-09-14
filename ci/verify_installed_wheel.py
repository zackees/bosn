#!/usr/bin/env python3
"""Install one Bosn wheel in an isolated venv and exercise its native surface.

This deliberately runs the installed entry point from outside the checkout.
It is suitable for a locally built wheel as well as the Linux, macOS, and
Windows native-wheel CI lanes:

    python ci/verify_installed_wheel.py dist/bosn-*.whl
"""

from __future__ import annotations

import argparse
import importlib.machinery
import json
import os
import shutil
import subprocess
import sys
import sysconfig
import tempfile
import textwrap
import time
import venv
import zipfile
from pathlib import Path
from typing import NoReturn

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


def fail(message: str) -> NoReturn:
    raise AssertionError(message)


def platform_executable_suffix() -> str:
    return ".exe" if os.name == "nt" else ""


def platform_extension_suffix() -> str:
    suffix = sysconfig.get_config_var("EXT_SUFFIX")
    if not isinstance(suffix, str) or suffix not in importlib.machinery.EXTENSION_SUFFIXES:
        fail(f"Python has no usable extension suffix: {suffix!r}")
    return suffix


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


def run(command: list[str], *, cwd: Path, env: dict[str, str]) -> subprocess.CompletedProcess[str]:
    return subprocess.run(command, cwd=cwd, env=env, check=True, text=True, capture_output=True)


def json_output(command: list[str], *, cwd: Path, env: dict[str, str]) -> dict[str, object]:
    result = run(command, cwd=cwd, env=env)
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
    daemon: subprocess.Popen[str],
) -> None:
    deadline = time.monotonic() + 10
    last_status = "no status request was attempted"
    while time.monotonic() < deadline:
        if daemon.poll() is not None:
            stdout, stderr = daemon.communicate()
            fail(f"installed daemon exited early ({daemon.returncode}): {stdout}\n{stderr}")
        status = subprocess.run(
            [str(cli), "daemon", "status", "--state-dir", str(state), "--json"],
            cwd=cwd,
            env=env,
            text=True,
            capture_output=True,
        )
        last_status = (
            f"exit={status.returncode}, stdout={status.stdout[-512:]!r}, "
            f"stderr={status.stderr[-512:]!r}"
        )
        if status.returncode == 0:
            value = json.loads(status.stdout)
            if value.get("action") == "daemon_status" and value.get("daemon") == "online":
                return
        time.sleep(0.05)
    if daemon.poll() is None:
        daemon_detail = "still running"
    else:
        stdout, stderr = daemon.communicate()
        daemon_detail = (
            f"exited={daemon.returncode}, stdout={stdout[-2048:]!r}, "
            f"stderr={stderr[-2048:]!r}"
        )
    socket_candidate = state / "bosn-rs.sock"
    fail(
        "installed daemon did not become ready; "
        f"daemon={daemon_detail}; "
        f"socket_candidate_length={len(os.fsencode(socket_candidate))}; "
        f"last_status={last_status}"
    )


def verify_installed_wheel(wheel: Path) -> None:
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
        venv.EnvBuilder(with_pip=True).create(environment)
        python = environment / ("Scripts/python.exe" if os.name == "nt" else "bin/python")
        scripts = environment / ("Scripts" if os.name == "nt" else "bin")
        subprocess.run(
            [uv, "pip", "install", "--python", str(python), "--no-deps", str(wheel)], check=True
        )

        env = child_environment(scripts)
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
                    import sysconfig

                    import bosn
                    from bosn.native_cli import native_executable

                    origin = pathlib.Path(importlib.util.find_spec("bosn._native").origin)
                    executable = native_executable()
                    assert pathlib.Path(bosn.__file__).resolve().is_relative_to(
                        pathlib.Path(sys.prefix).resolve()
                    )
                    assert origin.name == "_native" + sysconfig.get_config_var("EXT_SUFFIX")
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
        version = run([str(cli), "--version"], cwd=workdir, env=env)
        if version.stdout != f"bosn {versions['package_version']}\n":
            fail(f"installed CLI reported an unexpected version: {version.stdout!r}")

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

        daemon = subprocess.Popen(
            [str(cli), "daemon", "serve", "--state-dir", str(state)],
            cwd=workdir,
            env=env,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        try:
            wait_for_daemon(cli, state, cwd=workdir, env=env, daemon=daemon)
            doctor = json_output(
                [str(cli), "doctor", "--state-dir", str(state), "--json"], cwd=workdir, env=env
            )
            if doctor.get("action") != "doctor" or doctor.get("daemon") != "ready":
                fail(f"doctor did not inspect the installed daemon: {doctor}")
            stopped = json_output(
                [str(cli), "daemon", "stop", "--state-dir", str(state), "--json"],
                cwd=workdir,
                env=env,
            )
            if stopped != {"action": "daemon_stop", "stopped": True}:
                fail(f"installed daemon did not stop cleanly: {stopped}")
            if daemon.wait(timeout=10) != 0:
                stdout, stderr = daemon.communicate()
                fail(f"installed daemon failed while stopping: {stdout}\n{stderr}")
        finally:
            if daemon.poll() is None:
                daemon.kill()
                daemon.wait(timeout=10)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "wheel", help="wheel path, or a shell-style path relative to the current directory"
    )
    arguments = parser.parse_args()
    verify_installed_wheel(wheel_from_argument(arguments.wheel))


if __name__ == "__main__":
    main()
