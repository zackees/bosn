"""Test driver for bosn. Invoked by ./test via `uv run python ci/test.py`.

Docker-backed tests carry the `docker` marker. They are skipped automatically when no
engine is reachable, and are additionally excluded outright on non-Linux CI runners so
the unit suite stands alone without an engine.
"""

from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def docker_tests_enabled() -> bool:
    """Docker tests run on Linux, and in CI only on Linux runners."""
    if os.environ.get("BOSN_SKIP_DOCKER_TESTS"):
        return False
    if os.environ.get("CI") and not sys.platform.startswith("linux"):
        return False
    return True


def main(argv: list[str] | None = None) -> int:
    argv = sys.argv[1:] if argv is None else list(argv)
    cmd = ["pytest", "-v"]
    if not docker_tests_enabled():
        print("Docker tests disabled for this platform; running unit tests only.")
        cmd += ["-m", "not docker"]
    cmd += argv
    print(f">>> {' '.join(cmd)}", flush=True)
    return subprocess.run(cmd, cwd=ROOT).returncode


if __name__ == "__main__":
    raise SystemExit(main())
