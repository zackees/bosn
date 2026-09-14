#!/usr/bin/env python3
"""Fail CI when a workflow step installs native libraries through vcpkg.

soldr#3231 measured a per-job ``vcpkg install openssl`` bootstrap costing
7m47s-9m27s on Bosn's Windows native-wheel lane.  Bosn never needed it:
kernal-api's ``http-client`` reaches TLS through reqwest ``default-tls``, which
is ``native-tls`` and therefore SChannel on Windows MSVC, so ``openssl-sys`` is
not in the Windows dependency graph at all.  This parses workflow ``run``
scripts rather than grepping prose, so comments explaining the absence stay
allowed.
"""

from __future__ import annotations

import re
import sys
from collections.abc import Iterator
from pathlib import Path
from typing import cast

import yaml

VCPKG_INSTALL = re.compile(r"\bvcpkg(?:\.exe)?[\"']?\s+(?:[^\s|;&]+\s+)*?install\b", re.IGNORECASE)


def _walk(node: object, path: str = "") -> Iterator[tuple[str, object]]:
    yield path, node
    if isinstance(node, dict):
        for key, value in cast("dict[str, object]", node).items():
            yield from _walk(value, f"{path}.{key}" if path else str(key))
    elif isinstance(node, list):
        for index, value in enumerate(cast("list[object]", node)):
            yield from _walk(value, f"{path}[{index}]")


def offenders(document: object) -> Iterator[tuple[str, str]]:
    """Yield vcpkg install commands found in step ``run`` scripts."""

    for path, node in _walk(document):
        if path.rsplit(".", 1)[-1] != "run" or not isinstance(node, str):
            continue
        for line in node.splitlines():
            command = line.split("#", 1)[0].strip()
            if VCPKG_INSTALL.search(command):
                yield path, command


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
        print("lint_no_vcpkg_bootstrap: no workflow YAML scanned", file=sys.stderr)
        return 2
    failures = 0
    for file in files:
        try:
            document = yaml.safe_load(file.read_text(encoding="utf-8"))
        except yaml.YAMLError as error:
            print(f"lint_no_vcpkg_bootstrap: {file}: {error}", file=sys.stderr)
            return 2
        for path, command in offenders(document):
            print(f"{file}: {path}: vcpkg install in CI {command!r}", file=sys.stderr)
            failures += 1
    if failures:
        print(
            f"{failures} vcpkg install command(s) found. Windows MSVC TLS uses SChannel "
            "and needs no OpenSSL; fix the dependency, not the runner (soldr#3231).",
            file=sys.stderr,
        )
        return 1
    print(f"lint_no_vcpkg_bootstrap: {len(files)} workflow files clean")
    return 0


def main(argv: list[str] | None = None) -> int:
    root = Path(__file__).resolve().parents[1]
    arguments = sys.argv[1:] if argv is None else argv
    targets = [Path(item) for item in arguments] or [root / ".github/workflows"]
    return check(*targets)


if __name__ == "__main__":
    raise SystemExit(main())
