"""bosn's local gate (zackees/ci.yml GATE-001..007; bosn#361).

`ci-lint local-gate run` (see local-gate.toml) runs every lane below and, on
success, stamps HEAD with a tree-bound `Local-Gate:` trailer and one
`Ci-Attestation:` per attested gate (ci-attestations.yml). Each lane is also
runnable alone: `python ci/local_gate.py --lane <name>`.

Lanes split along input boundaries so the lane cache (GATE-007) can reuse a
pass whose inputs did not change:

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
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# The zackees/ci.yml commit whose ci-lint this repository's gate is checked by.
CI_LINT = "git+https://github.com/zackees/ci.yml@527cc179200496bdc41445a2c5b91e6025b495d7"

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
    if name in {"rust", "tests"}:
        commands = [[sys.executable, "ci/bosn_gate.py", "--lane", name]]
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
    print("\nLOCAL GATE OK")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
