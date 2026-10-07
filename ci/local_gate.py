"""Legacy diagnostic commands, excluded from the attestation protocol.

Use the pinned shared `ci-lint local-gate run` in local-gate.toml to create
attestations from source-bound Bosn workflow receipts. This helper only runs
individual diagnostic commands and never proves or stamps a gate pass.

- py-static: ruff and pyright over the Python sources.
- guards: the repository lints (CI policy, Ctrl-C handlers, vcpkg) and the
  local-gate definition itself.
- rust: rustfmt, Clippy, the kernal-api boundary and the locked resolution.
- tests: the Python and Rust suites, isolated in bosn (GATE-005): bosn's
  tests start bosn daemons and touch bosn state roots, so they never run on
  the developer's host.
"""

from __future__ import annotations

import argparse
import os
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# The zackees/ci.yml commit whose ci-lint this repository's gate is checked by.
CI_LINT = "git+https://github.com/zackees/ci.yml@96ba9f2df5b6c16d4fc933ee9dc2dbab7c47b46e"

# The locked environment without bosn itself (installing bosn is a full Rust
# extension build the linters do not need), then the tools from it.
SYNC = ["uv", "sync", "--frozen", "--all-groups", "--no-install-project"]
TOOL = ["uv", "run", "--no-sync"]
PY = ["uv", "run", "--no-project", "--python", "3.13", "--with", "pyyaml==6.0.2", "python"]

LANES: dict[str, list[list[str]]] = {
    "py-static": [
        SYNC,
        [*TOOL, "ruff", "format", "--check", "."],
        [*TOOL, "ruff", "check", "."],
        [*TOOL, "pyright"],
    ],
    "guards": [
        [*PY, "ci/lint_kbi.py"],
        [*PY, "ci/lint_no_macos_runners.py"],
        [*PY, "ci/lint_no_vcpkg_bootstrap.py"],
        ["uvx", "--from", CI_LINT, "--with", "pyyaml==6.0.2", "ci-lint", "local-gate", "lint"],
    ],
    "rust": [
        ["soldr", "cargo", "fmt", "--all", "--check"],
        ["soldr", "cargo", "fmt", "--check", "--manifest-path", "crates/bosn-widget/Cargo.toml"],
        [
            "soldr",
            "cargo",
            "clippy",
            "--workspace",
            "--exclude",
            "bosn-python",
            "--all-targets",
            "--locked",
            "--",
            "-D",
            "warnings",
        ],
        ["python3", "ci/verify_kernel_boundary.py"],
        ["soldr", "cargo", "metadata", "--locked", "--format-version", "1", "--no-deps"],
    ],
    "tests": [
        ["bosn", "run", "--task", "test"],
        ["bosn", "run", "--task", "rust-test"],
    ],
}


def run_lane(name: str) -> int:
    commands = LANES[name]
    if name in {"rust", "tests"} and not (
        os.environ.get("CI", "").lower() == "true" or os.environ.get("BOSN_TEST_ISOLATED") == "1"
    ):
        print("Rust/test diagnostics require an isolated Bosn runner", file=sys.stderr)
        return 1
    for command in commands:
        print(f"\n>>> [{name}] {' '.join(command)}", flush=True)
        started = time.monotonic()
        code = subprocess.run(command, cwd=ROOT).returncode
        print(f"<<< [{name}] exit {code} in {time.monotonic() - started:.1f}s", flush=True)
        if code != 0:
            return code
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--lane", choices=sorted(LANES), help="run one lane (default: every lane)")
    args = parser.parse_args(argv)
    for name in [args.lane] if args.lane else list(LANES):
        code = run_lane(name)
        if code != 0:
            print(f"\nLOCAL GATE FAILED: lane {name}", file=sys.stderr)
            return code
    print("\nDIAGNOSTIC COMMANDS OK (no attestation)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
