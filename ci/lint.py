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
            failed.append(cmd[0] + " " + (cmd[1] if len(cmd) > 1 else ""))
    if failed:
        print(f"\nLINT FAILED: {', '.join(failed)}", file=sys.stderr)
        return 1
    print("\nLINT OK")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
