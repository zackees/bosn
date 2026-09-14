#!/usr/bin/env python3
"""Decide the Recovery-guest wheel smoke from what the guest sent back.

The guest runs ``ci/verify_installed_wheel.py`` and writes its exit code to
``wheel.rc`` and its output to ``wheel.log``; this runs on the Linux runner
afterwards.  The verifier prints one ``[wheel-smoke] <phase>`` line as it
enters each phase, so the last such line names where a failure happened.

Waivers are an EXACT set (issue #252, after mimalloc-pprof's
``recovery_expected_failures.py``): a phase may be listed in
``WAIVED_PHASES`` only with a documented environment reason, and then

  * a failure in a waived phase        -> green, reported as waived;
  * a failure in any other phase       -> red (a real defect);
  * a pass while a waiver is on file   -> red (the waiver is stale, drop it).

The set is empty until a Recovery run proves a phase cannot run there; a
guess would be a silent skip with extra steps.

    python3 ci/recovery_wheel_result.py --collected <dir> [--summary-file FILE]
"""

from __future__ import annotations

import argparse
import re
import sys
from dataclasses import dataclass
from pathlib import Path

# The guest script writes this when the relocatable interpreter itself will
# not start, before the verifier could print anything.
PYTHON_MISSING_RC = 91
PHASE = re.compile(r"^\[wheel-smoke\] (.+)$", re.M)
ESCALATION_DOC = "docs/macos-guest.md"

# Phases that cannot run inside macOS Recovery, with the environment reason.
# Empty on purpose: see the module docstring.
WAIVED_PHASES: frozenset[str] = frozenset()
WAIVER_REASONS: dict[str, str] = {}


@dataclass(frozen=True)
class Verdict:
    ok: bool
    reason: str
    last_phase: str | None
    reached: int


def phases(log: str) -> list[str]:
    return PHASE.findall(log)


def evaluate(*, rc: int, log: str, waived: frozenset[str]) -> Verdict:
    seen = phases(log)
    last = seen[-1] if seen else None
    if rc == PYTHON_MISSING_RC:
        return Verdict(
            False, "the relocatable CPython interpreter did not start in Recovery", last, 0
        )
    if rc == 0:
        if last != "complete":
            return Verdict(
                False,
                f"exit 0 but the verifier never reported 'complete' (last: {last})",
                last,
                len(seen),
            )
        if waived:
            return Verdict(
                False,
                f"stale waiver: {sorted(waived)} now pass in Recovery; "
                "remove them from WAIVED_PHASES",
                last,
                len(seen),
            )
        return Verdict(True, f"all {len(seen)} verifier phases passed in Recovery", last, len(seen))
    tail = "\n".join(line for line in log.splitlines()[-12:] if line.strip())
    if last in waived:
        return Verdict(
            True,
            f"failed in waived phase {last!r} ({WAIVER_REASONS.get(last, 'documented')}); "
            f"{len(seen) - 1} earlier phases passed",
            last,
            len(seen),
        )
    return Verdict(
        False,
        f"verifier exited {rc} in phase {last!r} after {max(len(seen) - 1, 0)} "
        f"passing phases:\n{tail}",
        last,
        len(seen),
    )


def render(verdict: Verdict, python_banner: str) -> str:
    status = "PASS" if verdict.ok else "FAIL"
    return (
        "### macOS x86_64 Recovery wheel smoke (advisory)\n\n"
        f"- Guest interpreter: `{python_banner.strip() or 'unknown'}`\n"
        f"- Phases reached: {verdict.reached} (last: `{verdict.last_phase}`)\n"
        f"- Verdict: **{status}** — {verdict.reason}\n\n"
        f"This lane is advisory (see {ESCALATION_DOC}); it never gates a merge or release.\n"
    )


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--collected", required=True, help="the action's collected/ directory")
    parser.add_argument("--summary-file", help="append the markdown verdict here")
    args = parser.parse_args(argv)
    collected = Path(args.collected)
    rc_file = collected / "wheel.rc"
    if not rc_file.is_file():
        raise SystemExit(
            f"no wheel.rc in {collected}: the guest never reported; check the action's "
            "results/stdout and screenshots"
        )
    rc = int(rc_file.read_text().strip() or "1")
    log = (collected / "wheel.log").read_text() if (collected / "wheel.log").is_file() else ""
    banner = (collected / "python.txt").read_text() if (collected / "python.txt").is_file() else ""
    verdict = evaluate(rc=rc, log=log, waived=WAIVED_PHASES)
    text = render(verdict, banner)
    print(text)
    if args.summary_file:
        with Path(args.summary_file).open("a") as handle:
            handle.write(text)
    if not verdict.ok:
        print(f"::error::Recovery wheel smoke failed: {verdict.reason.splitlines()[0]}")
    return 0 if verdict.ok else 1


if __name__ == "__main__":
    sys.exit(main())
