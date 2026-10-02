"""Hard gate: every tracked Rust, Python and shell source file stays under 1,000 lines.

A file that grows past the limit is a signal to split it by responsibility (one module per
concern; large inline test modules move to sibling test files), not to raise the limit.

Counted: tracked `*.rs`, `*.py`, `*.pyi`, `*.sh`, `*.bash` files, and extensionless tracked
files whose first line is a shell or Python shebang (`./lint`, `./test`, `./bump`, ...).
Vendored code under `_vender/` is not ours and is skipped. Lines are physical lines.
"""

from __future__ import annotations

import argparse
from pathlib import Path

from tracked_files import tracked_files as _tracked_files

ROOT = Path(__file__).resolve().parent.parent
MAX_LINES = 1000  # a file must have fewer lines than this
SUFFIXES = frozenset({".rs", ".py", ".pyi", ".sh", ".bash"})
SHEBANGS = (b"#!/bin/bash", b"#!/bin/sh", b"#!/usr/bin/env bash", b"#!/usr/bin/env sh")
SKIPPED_PREFIXES = ("_vender/",)


def tracked_files(root: Path) -> list[Path]:
    return _tracked_files(root)


def is_source(path: Path, root: Path) -> bool:
    relative = path.relative_to(root).as_posix()
    if relative.startswith(SKIPPED_PREFIXES) or not path.is_file():
        return False
    if path.suffix in SUFFIXES:
        return True
    if path.suffix:
        return False
    with path.open("rb") as handle:
        first = handle.readline()
    return first.startswith(SHEBANGS) or (first.startswith(b"#!") and b"python" in first)


def line_count(path: Path) -> int:
    with path.open("rb") as handle:
        return sum(1 for _ in handle)


def violations(paths: list[Path], root: Path) -> list[tuple[str, int]]:
    found = []
    for path in paths:
        if is_source(path, root):
            lines = line_count(path)
            if lines >= MAX_LINES:
                found.append((path.relative_to(root).as_posix(), lines))
    return sorted(found, key=lambda item: -item[1])


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT)
    args, _ = parser.parse_known_args(argv)
    root = args.root.resolve()
    found = violations(tracked_files(root), root)
    for relative, lines in found:
        print(
            f"{relative}: {lines} lines (limit: fewer than {MAX_LINES}); split it by responsibility"
        )
    if found:
        return 1
    print(f"lint_file_length: every source file is under {MAX_LINES} lines")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
