#!/usr/bin/env python3
"""Check that Rust crates use kernal-api as their only systems facade.

This is intentionally a manifest/lockfile check rather than a source-text
check.  It protects the architectural boundary at the point where a new
backend or runtime would enter the dependency graph, while Cargo's locked
metadata check proves the checked-in resolution can be reproduced.
"""

from __future__ import annotations

import argparse
from collections.abc import Iterator
from pathlib import Path

import tomllib

EXPECTED_KERNEL_VERSION = "=0.1.0"
EXPECTED_KERNEL_REVISION = "fc634e507024d63ccaaf75fa564818b9dcfbff36"
# These effects belong behind kernal-api.  Bosn may depend on application
# libraries (for example prost and serde), but must not add a second OS,
# SQLite, HTTP, or async-runtime boundary.
FORBIDDEN_DIRECT_PACKAGES = frozenset(
    {"running-process", "rusqlite", "tokio", "reqwest", "ureq", "hyper"}
)


def _dependency_tables(manifest: dict[str, object]) -> Iterator[dict[str, object]]:
    for name in ("dependencies", "dev-dependencies", "build-dependencies"):
        table = manifest.get(name)
        if isinstance(table, dict):
            yield table
    targets = manifest.get("target")
    if not isinstance(targets, dict):
        return
    for target in targets.values():
        if not isinstance(target, dict):
            continue
        for name in ("dependencies", "dev-dependencies", "build-dependencies"):
            table = target.get(name)
            if isinstance(table, dict):
                yield table


def _package_name(name: str, specification: object) -> str:
    if isinstance(specification, dict) and isinstance(specification.get("package"), str):
        return specification["package"]
    return name


def _kernel_errors(path: Path, dependencies: dict[str, object]) -> list[str]:
    errors: list[str] = []
    for name, specification in dependencies.items():
        package = _package_name(name, specification)
        if package in FORBIDDEN_DIRECT_PACKAGES:
            errors.append(f"{path}: direct dependency on {package} bypasses kernal-api")
        if package != "kernal-api":
            continue
        if not isinstance(specification, dict):
            errors.append(f"{path}: kernal-api must pin the reviewed git revision")
            continue
        if specification.get("version") != EXPECTED_KERNEL_VERSION:
            errors.append(f"{path}: kernal-api version must be {EXPECTED_KERNEL_VERSION}")
        if specification.get("git") != "https://github.com/zackees/kernal-api":
            errors.append(f"{path}: kernal-api must use the reviewed upstream source")
        if specification.get("rev") != EXPECTED_KERNEL_REVISION:
            errors.append(f"{path}: kernal-api revision must be {EXPECTED_KERNEL_REVISION}")
        if "path" in specification:
            errors.append(f"{path}: kernal-api must not use a path override")
    return errors


def verify(root: Path) -> list[str]:
    workspace = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))["workspace"]
    members = workspace.get("members") if isinstance(workspace, dict) else None
    if not isinstance(members, list):
        return ["Cargo.toml: workspace members are missing"]

    errors: list[str] = []
    for member in members:
        if not isinstance(member, str):
            errors.append("Cargo.toml: workspace member must be a string")
            continue
        path = root / member / "Cargo.toml"
        manifest = tomllib.loads(path.read_text(encoding="utf-8"))
        for dependencies in _dependency_tables(manifest):
            errors.extend(_kernel_errors(path.relative_to(root), dependencies))

    lock = tomllib.loads((root / "Cargo.lock").read_text(encoding="utf-8"))
    packages = lock.get("package")
    kernel_packages = (
        [
            package
            for package in packages
            if isinstance(package, dict) and package.get("name") == "kernal-api"
        ]
        if isinstance(packages, list)
        else []
    )
    if len(kernel_packages) != 1:
        errors.append("Cargo.lock: expected exactly one kernal-api package")
    else:
        package = kernel_packages[0]
        if package.get("version") != "0.1.0":
            errors.append("Cargo.lock: kernal-api version must be 0.1.0")
        source = package.get("source")
        expected_source = (
            "git+https://github.com/zackees/kernal-api?rev="
            f"{EXPECTED_KERNEL_REVISION}#{EXPECTED_KERNEL_REVISION}"
        )
        if source != expected_source:
            errors.append("Cargo.lock: kernal-api source must match the reviewed revision")
    return errors


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parent.parent)
    args = parser.parse_args(argv)
    errors = verify(args.root.resolve())
    if errors:
        print("\n".join(errors))
        return 1
    print("kernal-api dependency and systems boundary verified")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
