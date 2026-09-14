"""The installed Python package is a native client boundary, not a second daemon."""

from __future__ import annotations

import importlib.util
from pathlib import Path

import tomllib

import bosn

ROOT = Path(__file__).resolve().parent.parent
PACKAGE = ROOT / "src" / "bosn"
RETIRED_MODULES = (
    "accounting",
    "autostart",
    "cli",
    "clock",
    "compose",
    "config",
    "converge",
    "daemon",
    "docker_cli",
    "engine",
    "frontdoor",
    "gc",
    "gitstate",
    "guest",
    "ipc",
    "jobs",
    "labels",
    "legacy",
    "manifest",
    "migration_lock",
    "options",
    "paths",
    "recovery",
    "registry",
    "resources",
    "retention",
    "shims",
)


def test_python_distribution_contains_only_native_boundary_modules() -> None:
    assert {path.name for path in PACKAGE.glob("*.py")} == {
        "__init__.py",
        "__main__.py",
        "native_cli.py",
    }

    # The retained package files must not gain a Python process, Docker, or
    # SQLite implementation. The native launcher hands off only to the bundled
    # executable; wheel building is checked separately in bosn_build_backend.
    source = "\n".join(path.read_text(encoding="utf-8") for path in PACKAGE.glob("*.py"))
    for forbidden in ("subprocess", "sqlite3", "running_process", "docker", "podman"):
        assert forbidden not in source.lower()


def test_retired_python_lifecycle_modules_are_not_importable() -> None:
    assert bosn.__version__
    for module in RETIRED_MODULES:
        assert importlib.util.find_spec(f"bosn.{module}") is None


def test_distribution_has_no_python_runtime_lifecycle_dependencies() -> None:
    project = tomllib.loads((ROOT / "pyproject.toml").read_text(encoding="utf-8"))
    assert project["project"]["dependencies"] == []
    assert project["project"]["scripts"] == {"bosn": "bosn.native_cli:main"}
    assert project["project"]["requires-python"] == ">=3.10"
