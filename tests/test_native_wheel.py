"""End-to-end proof that a Bosn wheel carries its Rust CLI and daemon."""

from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parent.parent


@pytest.mark.slow
def test_clean_wheel_install_has_native_cli_without_source_checkout(tmp_path: Path) -> None:
    wheels = tmp_path / "wheels"
    wheels.mkdir()
    subprocess.run(
        [
            "uv",
            "build",
            "--wheel",
            "--no-build-isolation",
            "--python",
            sys.executable,
            "--out-dir",
            str(wheels),
            str(ROOT),
        ],
        check=True,
        cwd=ROOT,
    )
    (wheel,) = wheels.glob("bosn-*.whl")

    environment = tmp_path / "environment"
    subprocess.run([sys.executable, "-m", "venv", str(environment)], check=True)
    python = environment / ("Scripts/python.exe" if os.name == "nt" else "bin/python")
    scripts = environment / ("Scripts" if os.name == "nt" else "bin")
    subprocess.run(
        ["uv", "pip", "install", "--python", str(python), "--no-deps", str(wheel)],
        check=True,
        cwd=tmp_path,
    )

    # Deliberately omit both the checkout and host library paths.  The installed
    # package must resolve its own extension, binary, and bundled native libs.
    clean_environment = {"PATH": str(scripts)}
    imported = subprocess.run(
        [
            str(python),
            "-c",
            "import bosn; from bosn.native_cli import native_executable; "
            "print(bosn.__version__, bosn.native_version()); print(native_executable())",
        ],
        check=True,
        cwd=tmp_path,
        env=clean_environment,
        text=True,
        capture_output=True,
    )
    lines = imported.stdout.splitlines()
    package_version, native_version = lines[0].split()
    assert package_version == native_version
    assert Path(lines[1]).is_file()
    assert not lines[1].startswith(str(ROOT))

    version = subprocess.run(
        [str(scripts / ("bosn.exe" if os.name == "nt" else "bosn")), "--version"],
        check=True,
        cwd=tmp_path,
        env=clean_environment,
        text=True,
        capture_output=True,
    )
    assert version.stdout == f"bosn {package_version}\n"

    daemon_help = subprocess.run(
        [str(scripts / ("bosn.exe" if os.name == "nt" else "bosn")), "daemon", "--help"],
        cwd=tmp_path,
        env=clean_environment,
        text=True,
        capture_output=True,
    )
    assert daemon_help.returncode == 2
    assert "bosn daemon serve" in daemon_help.stderr
