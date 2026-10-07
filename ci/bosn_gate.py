"""Compatibility entry point; the shared CI tool owns execution proof."""

from __future__ import annotations

import subprocess
import sys
from pathlib import Path

CI_LINT = "git+https://github.com/zackees/ci.yml@96ba9f2df5b6c16d4fc933ee9dc2dbab7c47b46e"
ROOT = Path(__file__).resolve().parent.parent


def main(argv: list[str] | None = None) -> int:
    arguments = sys.argv[1:] if argv is None else argv
    command = ["uvx", "--from", CI_LINT, "--with", "pyyaml==6.0.2", "ci-lint", "local-gate", "run"]
    return subprocess.run([*command, *arguments], cwd=ROOT).returncode


if __name__ == "__main__":
    raise SystemExit(main())
