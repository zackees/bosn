#!/usr/bin/env python3
"""Fail a full CI run unless every required cell succeeded on its candidate."""

from __future__ import annotations

import argparse
import os
import subprocess
from collections.abc import Iterable
from typing import Any

from ci_queue_timing import fetch_run

REQUIRED = (
    "CI policy (no hosted macOS runners)",
    "Rust workspace (locked tests)",
    "Linux (lint + unit + docker)",
    "Native wheel (ubuntu-latest)",
    "Native wheel (windows-latest)",
    "Darwin wheel (x86_64-apple-darwin, Linux-hosted Soldr)",
    "Darwin wheel (aarch64-apple-darwin, Linux-hosted Soldr)",
    "Hosted macOS wheel smoke (x86_64-apple-darwin)",
    "Hosted macOS wheel smoke (aarch64-apple-darwin)",
)


def failures(jobs: Iterable[dict[str, Any]]) -> list[str]:
    """Use the latest attempt of each named job; missing and skipped both fail."""
    latest: dict[str, dict[str, Any]] = {}
    for job in jobs:
        name = str(job.get("name", ""))
        if name in REQUIRED and int(job.get("id", 0)) > int(latest.get(name, {}).get("id", 0)):
            latest[name] = job
    return [
        f"{name}: {latest[name].get('conclusion') if name in latest else 'missing'}"
        for name in REQUIRED
        if name not in latest or latest[name].get("conclusion") != "success"
    ]


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo", default="zackees/bosn")
    parser.add_argument("--run-id", type=int, required=True)
    parser.add_argument("--candidate-sha", required=True)
    args = parser.parse_args()
    actual = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
    if actual.lower() != args.candidate_sha.lower():
        print(f"Full CI candidate mismatch: checkout {actual}, requested {args.candidate_sha}")
        return 1
    _, jobs = fetch_run(args.repo, args.run_id, os.environ.get("GITHUB_TOKEN"))
    bad = failures(jobs)
    if bad:
        for item in bad:
            print(f"Full CI coverage failure: {item}")
        return 1
    print(f"Full CI coverage: all {len(REQUIRED)} cells succeeded for {actual}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
