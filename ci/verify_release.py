#!/usr/bin/env python3
"""Refuse to release anything the tag, the tree, and the wheels disagree about.

``source --tag vX.Y.Z`` checks that the tag names exactly the version the tree
declares, and that every hand-written declaration of that version agrees.

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


def _toml_version(path: Path, table: str) -> str:
    version = tomllib.loads(path.read_text(encoding="utf-8"))[table]["version"]
    if not isinstance(version, str):
        raise ValueError(f"{path}: [{table}].version is not a literal string")
    return version


def _init_version(path: Path) -> str:
    for node in ast.parse(path.read_text(encoding="utf-8")).body:
        if (
            isinstance(node, ast.Assign)
            and len(node.targets) == 1
            and isinstance(node.targets[0], ast.Name)
            and node.targets[0].id == "__version__"
            and isinstance(node.value, ast.Constant)
            and isinstance(node.value.value, str)
        ):
            return node.value.value
    raise ValueError(f"{path}: no __version__ string literal")


def _native_version_assertion(path: Path) -> str:
    match = re.search(
        r"assert_eq!\(native_version\(\),\s*\"([^\"]+)\"\)", path.read_text(encoding="utf-8")
    )
    if match is None:
        raise ValueError(f"{path}: no native_version() assertion")
    return match.group(1)


def declared_versions(root: Path) -> dict[str, str]:
    """Every place the release version is written by hand."""
    return {
        "pyproject.toml": _toml_version(root / "pyproject.toml", "project"),
        "crates/bosn-python/Cargo.toml": _toml_version(
            root / "crates" / "bosn-python" / "Cargo.toml", "package"
        ),
        # The crates.io `bosn`, amalgamated from the internal crates at release.
        "crates/bosn/Cargo.toml": _toml_version(root / "crates" / "bosn" / "Cargo.toml", "package"),
        "src/bosn/__init__.py": _init_version(root / "src" / "bosn" / "__init__.py"),
        "crates/bosn-python/src/lib.rs": _native_version_assertion(
            root / "crates" / "bosn-python" / "src" / "lib.rs"
        ),
    }


def source_errors(root: Path, tag: str) -> list[str]:
    match = TAG.match(tag)
    if match is None:
        return [f"tag {tag!r} is not of the form vMAJOR.MINOR.PATCH"]
    versions = declared_versions(root)
    if len(set(versions.values())) != 1:
        found = ", ".join(f"{path}={version}" for path, version in versions.items())
        return [f"version declarations disagree: {found}"]
    declared = next(iter(versions.values()))
    if match.group(1) != declared:
        return [f"tag {tag} does not match the declared version {declared}"]
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
        success = f"{args.tag} matches every version declaration"
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
