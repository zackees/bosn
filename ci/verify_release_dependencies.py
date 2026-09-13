#!/usr/bin/env python3
"""Reject unpublished/local kernel sources from a release manifest.

Development deliberately uses the reviewed git revision until kernal-api 0.1.0
is published.  This check is for release automation only; it must fail now.
"""

from __future__ import annotations

import argparse
import tomllib
from pathlib import Path


def verify(manifest: Path, expected_version: str = "=0.1.0") -> list[str]:
    data = tomllib.loads(manifest.read_text(encoding="utf-8"))
    errors: list[str] = []
    dependency = data.get("dependencies", {}).get("kernal-api")
    if isinstance(dependency, str):
        version = dependency
    elif isinstance(dependency, dict):
        if "git" in dependency or "path" in dependency:
            errors.append("kernal-api must not use a git or path source in a release")
        version = dependency.get("version")
    else:
        version = None
    if version != expected_version:
        errors.append(f"kernal-api release dependency must be exactly {expected_version}")
    for owner in _workspace_manifests(manifest):
        content = tomllib.loads(owner.read_text(encoding="utf-8"))
        if _contains_kernel_override(content.get("patch", {})) or _contains_kernel_override(
            content.get("replace", {})
        ):
            errors.append("release manifest must not override kernal-api through patch or replace")
        if _contains_kernel_source(content):
            errors.append("release manifest contains a git/path/workspace kernal-api bypass")
    return errors


def _workspace_manifests(manifest: Path) -> list[Path]:
    manifests = [manifest]
    for parent in manifest.resolve().parents:
        candidate = parent / "Cargo.toml"
        if candidate != manifest and candidate.is_file():
            contents = tomllib.loads(candidate.read_text(encoding="utf-8"))
            if "workspace" in contents:
                manifests.append(candidate)
                break
    return manifests


def _is_kernel(key: object, value: object) -> bool:
    return (isinstance(key, str) and (key == "kernal-api" or key.startswith("kernal-api:"))) or (
        isinstance(value, dict) and value.get("package") == "kernal-api"
    )


def _contains_kernel_override(value: object) -> bool:
    if not isinstance(value, dict):
        return False
    return any(_is_kernel(key, item) for key, item in value.items()) or any(
        _contains_kernel_override(item) for item in value.values()
    )


def _contains_kernel_source(value: object) -> bool:
    if not isinstance(value, dict):
        return False
    for key, item in value.items():
        if (
            _is_kernel(key, item)
            and isinstance(item, dict)
            and any(name in item for name in ("git", "path", "workspace", "registry"))
        ):
            return True
        if _contains_kernel_source(item):
            return True
    return False


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("manifest", type=Path)
    parser.add_argument("--expected-version", default="=0.1.0")
    args = parser.parse_args()
    errors = verify(args.manifest, args.expected_version)
    if errors:
        raise SystemExit("\n".join(errors))
