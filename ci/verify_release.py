#!/usr/bin/env python3
"""Refuse to release anything the tag, the tree, and the wheels disagree about.

``source --tag vX.Y.Z`` checks that the tag names exactly the release version,
``[workspace.package].version`` in Cargo.toml, and that nothing that ships writes
its own copy of it: both published crates inherit it, the wheel reads it through
maturin, and ``bosn.__version__`` is derived.

``wheels --tag vX.Y.Z DIR`` checks that DIR holds exactly the four platform
wheels a release ships: one ``cp310-abi3`` wheel each for Linux x86_64, Windows
x86_64, macOS x86_64, and macOS arm64, all at the tagged version, and nothing
else. A bare ``linux_x86_64`` tag is refused because PyPI rejects it; the build
backend asks maturin for ``--compatibility pypi`` so a real build never
produces one.
"""

from __future__ import annotations

import argparse
import ast
import re
import sys
from pathlib import Path

import tomllib

TAG = re.compile(r"^v(\d+\.\d+\.\d+)$")
# name, version, python tag, abi tag, platform tag. `bosn` has no build tag.
WHEEL = re.compile(r"^bosn-(?P<version>[^-]+)-cp310-abi3-(?P<platform>[^-]+)\.whl$")
PLATFORMS = (
    ("Linux x86_64 (manylinux_*_x86_64)", re.compile(r"^manylinux_\d+_\d+_x86_64$")),
    ("Windows x86_64 (win_amd64)", re.compile(r"^win_amd64$")),
    ("macOS x86_64 (macosx_10_12_x86_64)", re.compile(r"^macosx_10_12_x86_64$")),
    ("macOS arm64 (macosx_11_0_arm64)", re.compile(r"^macosx_11_0_arm64$")),
)


def release_version(root: Path) -> str:
    """`[workspace.package].version` in the root Cargo.toml: the only place it is written."""
    workspace = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))["workspace"]
    version = workspace.get("package", {}).get("version")
    if not isinstance(version, str):
        raise ValueError("Cargo.toml: [workspace.package] version is missing")
    return version


def single_source_errors(root: Path) -> list[str]:
    """Everything that ships must derive the version, never write its own copy.

    A second copy is what a bump forgets; `./bump` rewrites only the workspace line.
    """
    errors: list[str] = []
    for crate in ("bosn", "bosn-python"):
        path = root / "crates" / crate / "Cargo.toml"
        package = tomllib.loads(path.read_text(encoding="utf-8"))["package"]
        if package.get("version") != {"workspace": True}:
            errors.append(f"crates/{crate}/Cargo.toml must use `version.workspace = true`")
    project = tomllib.loads((root / "pyproject.toml").read_text(encoding="utf-8"))["project"]
    if "version" in project or "version" not in project.get("dynamic", []):
        errors.append(
            'pyproject.toml must declare `dynamic = ["version"]` and no [project].version'
        )
    init = root / "src" / "bosn" / "__init__.py"
    for node in ast.walk(ast.parse(init.read_text(encoding="utf-8"))):
        targets = (
            node.targets
            if isinstance(node, ast.Assign)
            else [node.target]
            if isinstance(node, ast.AnnAssign)
            else []
        )
        value = getattr(node, "value", None)
        if any(
            isinstance(target, ast.Name) and target.id == "__version__" for target in targets
        ) and isinstance(value, ast.Constant):
            errors.append("src/bosn/__init__.py must derive __version__, not write a literal")
    lib = (root / "crates" / "bosn-python" / "src" / "lib.rs").read_text(encoding="utf-8")
    if re.search(r'assert_eq!\(native_version\(\),\s*"', lib):
        errors.append("crates/bosn-python/src/lib.rs must not pin native_version() to a literal")
    return errors


def source_errors(root: Path, tag: str) -> list[str]:
    match = TAG.match(tag)
    if match is None:
        return [f"tag {tag!r} is not of the form vMAJOR.MINOR.PATCH"]
    errors = single_source_errors(root)
    if errors:
        return errors
    version = release_version(root)
    if match.group(1) != version:
        return [f"tag {tag} does not match the release version {version}"]
    return []


def wheel_errors(directory: Path, tag: str) -> list[str]:
    match = TAG.match(tag)
    if match is None:
        return [f"tag {tag!r} is not of the form vMAJOR.MINOR.PATCH"]
    version = match.group(1)

    errors: list[str] = []
    matched: dict[str, list[str]] = {label: [] for label, _ in PLATFORMS}
    for path in sorted(directory.iterdir()):
        if path.suffix != ".whl":
            errors.append(f"unexpected file {path.name}")
            continue
        wheel = WHEEL.match(path.name)
        platform = next(
            (
                label
                for label, pattern in PLATFORMS
                if wheel is not None and pattern.match(wheel.group("platform"))
            ),
            None,
        )
        if wheel is None or wheel.group("version") != version or platform is None:
            errors.append(f"unexpected wheel {path.name}")
            continue
        matched[platform].append(path.name)

    for label, names in matched.items():
        if not names:
            errors.append(f"no wheel for {label}")
        elif len(names) > 1:
            errors.append(f"{len(names)} wheels for {label}; expected exactly one")
    return errors


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    source = commands.add_parser("source", help="check the tag against the tree")
    source.add_argument("--tag", required=True)
    source.add_argument("--root", type=Path, default=Path(__file__).resolve().parent.parent)
    wheels = commands.add_parser("wheels", help="check a directory of built wheels")
    wheels.add_argument("--tag", required=True)
    wheels.add_argument("directory", type=Path)
    args = parser.parse_args(argv)

    if args.command == "source":
        errors = source_errors(args.root.resolve(), args.tag)
        success = f"{args.tag} matches the release version, and nothing else declares one"
    else:
        errors = wheel_errors(args.directory, args.tag)
        success = f"{args.directory} holds exactly the four {args.tag} platform wheels"
    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1
    print(success)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
