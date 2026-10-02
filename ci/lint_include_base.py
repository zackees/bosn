"""Hard gate: an embedded file's directory is written once per Rust file.

`include_str!`/`include_bytes!` paths are literals, so a directory spelled in
several of them (`"assets/a.js"`, `"assets/b.js"`, ...) has to be edited in
every place when it moves. Instead, within a file:

- reach a directory's files through one base macro, e.g.
  `macro_rules! asset { ($n:literal) => { include_str!(concat!("assets/", $n)) }; }`;
- include one file once and bind it to a `const`.

A base macro's directory must stay inside the crate's `src/`:
`ci/publish_amalgamate.py` relocates only literal includes, so a `concat!`
base outside `src/` would not reach the published crate. A single literal
include of a fixture outside `src/` is fine. Vendored code under `_vender/`
is skipped.
"""

from __future__ import annotations

import argparse
import re
from collections import Counter
from pathlib import Path

from tracked_files import tracked_files as _tracked_files

ROOT = Path(__file__).resolve().parent.parent
SKIPPED_PREFIXES = ("_vender/",)
LITERAL = re.compile(r'include_(?:str|bytes)!\(\s*"([^"]+)"\s*\)')
BASE = re.compile(r'include_(?:str|bytes)!\(\s*concat!\(\s*"([^"]+)"')


def tracked_files(root: Path) -> list[Path]:
    return _tracked_files(root, "*.rs")


def directory(path: str) -> str:
    return path.rsplit("/", 1)[0] + "/" if "/" in path else ""


def problems(text: str) -> list[str]:
    found = []
    repeated = Counter(directory(path) for path in LITERAL.findall(text))
    for base, count in sorted(repeated.items()):
        if base and count > 1:
            found.append(
                f"`{base}` is written in {count} includes; reach it through one base macro"
            )
    for base in BASE.findall(text):
        if base.startswith("../") or base.startswith("/"):
            found.append(
                f"base `{base}` leaves src/; publish_amalgamate only relocates literal includes"
            )
    return found


def violations(paths: list[Path], root: Path) -> list[tuple[str, str]]:
    found = []
    for path in paths:
        relative = path.relative_to(root).as_posix()
        if relative.startswith(SKIPPED_PREFIXES) or not path.is_file():
            continue
        for problem in problems(path.read_text(encoding="utf-8", errors="replace")):
            found.append((relative, problem))
    return found


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT)
    args, _ = parser.parse_known_args(argv)
    root = args.root.resolve()
    found = violations(tracked_files(root), root)
    for relative, problem in found:
        print(f"{relative}: {problem}")
    if found:
        return 1
    print("lint_include_base: every embedded directory is written once per file")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
