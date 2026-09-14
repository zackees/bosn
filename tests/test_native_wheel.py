"""End-to-end proof that a Bosn wheel works outside its source checkout."""

from __future__ import annotations

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
    subprocess.run(
        [sys.executable, str(ROOT / "ci" / "verify_installed_wheel.py"), str(wheel)],
        check=True,
        cwd=tmp_path,
    )
