"""Lint driver for bosn. Invoked by ./lint via `uv run python ci/lint.py`."""

from __future__ import annotations

import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

CHECKS: list[list[str]] = [
    ["ruff", "format", "--check", "."],
    ["ruff", "check", "."],
    ["pyright"],
    # Ctrl-C correctness: ruff's BLE001 flags blind excepts but cannot see the *missing*
    # KeyboardInterrupt sibling handler, which is the actual defect.
    [sys.executable, str(ROOT / "ci" / "lint_kbi.py")],
]


def run(cmd: list[str]) -> int:
    print(f"\n>>> {' '.join(cmd)}", flush=True)
    return subprocess.run(cmd, cwd=ROOT).returncode


def main(argv: list[str] | None = None) -> int:
    argv = sys.argv[1:] if argv is None else list(argv)
    failed: list[str] = []
    for check in CHECKS:
        cmd = check + argv if argv else check
        if run(cmd) != 0:
            failed.append(" ".join(Path(part).name for part in cmd[:2]))
    if failed:
        print(f"\nLINT FAILED: {', '.join(failed)}", file=sys.stderr)
        return 1
    print("\nLINT OK")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
