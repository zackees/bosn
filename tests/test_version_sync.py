"""Every hand-written version declaration in the tree must agree."""

from __future__ import annotations

import ast
import re
from pathlib import Path

import tomllib

ROOT = Path(__file__).resolve().parent.parent


def pyproject_version() -> str:
    project = tomllib.loads((ROOT / "pyproject.toml").read_text(encoding="utf-8"))
    version = project["project"]["version"]
    assert isinstance(version, str)
    return version


def cargo_version() -> str:
    path = ROOT / "crates" / "bosn-python" / "Cargo.toml"
    cargo = tomllib.loads(path.read_text(encoding="utf-8"))
    version = cargo["package"]["version"]
    assert isinstance(version, str), (
        "crates/bosn-python/Cargo.toml [package].version is not a literal string"
    )
    return version


def init_version() -> str:
    path = ROOT / "src" / "bosn" / "__init__.py"
    tree = ast.parse(path.read_text(encoding="utf-8"))
    for node in tree.body:
        if isinstance(node, ast.Assign):
            if len(node.targets) != 1:
                continue
            target = node.targets[0]
        elif isinstance(node, ast.AnnAssign):
            target = node.target
        else:
            continue
        if isinstance(target, ast.Name) and target.id == "__version__":
            value = node.value
            if isinstance(value, ast.Constant) and isinstance(value.value, str):
                return value.value
    raise AssertionError("__version__ string literal not found in src/bosn/__init__.py")


def test_version_declarations_match() -> None:
    versions = {
        "pyproject.toml": pyproject_version(),
        "crates/bosn-python/Cargo.toml": cargo_version(),
        "src/bosn/__init__.py": init_version(),
    }
    assert len(set(versions.values())) == 1, versions


def test_version_is_nonempty_semver_like() -> None:
    assert re.match(r"^\d+\.\d+\.\d+", pyproject_version())
