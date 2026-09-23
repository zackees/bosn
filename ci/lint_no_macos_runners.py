#!/usr/bin/env python3
"""Allow hosted macOS only in explicitly full CI and release smoke jobs.

Darwin wheels stay Linux-built through Soldr. This parses YAML scheduling
positions so prose cannot accidentally change the runner policy.
"""

from __future__ import annotations

import re
import sys
from collections.abc import Iterator
from pathlib import Path
from typing import cast

import yaml

MACOS_LABEL = re.compile(r"\bmacos-(?:latest|\d+)[\w.-]*\b", re.IGNORECASE)
MACOS_BARE = re.compile(r"^macos$", re.IGNORECASE)
SCHEDULING_KEYS = frozenset({"runs-on", "matrix", "vmImage", "pool"})


def _walk(node: object, path: str = "") -> Iterator[tuple[str, object]]:
    yield path, node
    if isinstance(node, dict):
        for key, value in cast("dict[str, object]", node).items():
            yield from _walk(value, f"{path}.{key}" if path else str(key))
    elif isinstance(node, list):
        for index, value in enumerate(cast("list[object]", node)):
            yield from _walk(value, f"{path}[{index}]")


def _strings(node: object) -> Iterator[str]:
    if isinstance(node, str):
        yield node
    elif isinstance(node, dict):
        for value in cast("dict[str, object]", node).values():
            yield from _strings(value)
    elif isinstance(node, list):
        for value in cast("list[object]", node):
            yield from _strings(value)


def offenders(document: object) -> Iterator[tuple[str, str]]:
    """Yield labels only from runner-selection keys and matrix definitions."""

    claimed: list[str] = []
    for path, node in _walk(document):
        if any(
            path == item or path.startswith(f"{item}.") or path.startswith(f"{item}[")
            for item in claimed
        ):
            continue
        key = path.rsplit(".", 1)[-1].split("[", 1)[0]
        if key not in SCHEDULING_KEYS:
            continue
        claimed.append(path)
        for text in _strings(node):
            yield from ((path, label) for label in MACOS_LABEL.findall(text))
            if MACOS_BARE.fullmatch(text.strip()):
                yield path, text.strip()


def _allowed_job(file: Path, document: object, path: str) -> bool:
    """Accept only the named smoke job with its event and dependency gate."""
    if not path.startswith("jobs.darwin-hosted-smoke.") or not isinstance(document, dict):
        return False
    jobs = document.get("jobs", {})
    job = jobs.get("darwin-hosted-smoke", {}) if isinstance(jobs, dict) else {}
    if not isinstance(job, dict):
        return False
    condition = " ".join(str(job.get("if", "")).split())
    needs = job.get("needs", [])
    matrix = job.get("strategy", {}).get("matrix", {})
    targets = matrix.get("include", []) if isinstance(matrix, dict) else []
    runner_pair = {
        (entry.get("target"), entry.get("runner")) for entry in targets if isinstance(entry, dict)
    }
    if runner_pair != {
        ("x86_64-apple-darwin", "macos-15-intel"),
        ("aarch64-apple-darwin", "macos-15"),
    }:
        return False
    if file.name == "ci.yml":
        selector = jobs.get("select-tier", {})
        command = " ".join(
            str(step.get("run", "")) for step in selector.get("steps", []) if isinstance(step, dict)
        )
        return (
            condition == "needs.select-tier.outputs.full == 'true'"
            and set(needs) == {"select-tier", "darwin-cross-wheel"}
            and "ci/select_ci_tier.py" in command
        )
    if file.name == "auto-release.yml":
        return (
            condition == "needs.guard.outputs.release == 'true'"
            and isinstance(needs, list)
            and "guard" in needs
            and "darwin-wheel" in needs
        )
    return False


def check(*targets: Path) -> int:
    files = [
        file
        for target in targets
        for file in (
            sorted((*target.glob("*.yml"), *target.glob("*.yaml"))) if target.is_dir() else [target]
        )
        if file.is_file()
    ]
    if not files:
        print("lint_no_macos_runners: no workflow YAML scanned", file=sys.stderr)
        return 2
    failures = 0
    for file in files:
        try:
            document = yaml.safe_load(file.read_text(encoding="utf-8"))
        except yaml.YAMLError as error:
            print(f"lint_no_macos_runners: {file}: {error}", file=sys.stderr)
            return 2
        for path, label in offenders(document):
            if _allowed_job(file, document, path):
                continue
            print(f"{file}: {path}: native macOS runner label {label!r}", file=sys.stderr)
            failures += 1
    if failures:
        print(f"{failures} native macOS runner label(s) found (issue #252).", file=sys.stderr)
        return 1
    print(f"lint_no_macos_runners: {len(files)} workflow files clean")
    return 0


def main(argv: list[str] | None = None) -> int:
    root = Path(__file__).resolve().parents[1]
    arguments = sys.argv[1:] if argv is None else argv
    targets = [Path(item) for item in arguments] or [root / ".github/workflows"]
    return check(*targets)


if __name__ == "__main__":
    raise SystemExit(main())
