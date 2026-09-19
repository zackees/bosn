from __future__ import annotations

import importlib.util
import sys
from pathlib import Path

import pytest

SPEC = importlib.util.spec_from_file_location("bump", Path("ci/bump.py"))
assert SPEC is not None and SPEC.loader is not None
bump = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = bump
SPEC.loader.exec_module(bump)

WORKSPACE = """[workspace]
members = ["crates/bosn", "crates/bosn-core"]
resolver = "3"

[workspace.package]
version = "0.1.4"

[workspace.dependencies]
serde = { version = "=1.0.229" }
"""


@pytest.mark.parametrize(
    ("part", "expected"),
    [("patch", "0.1.5"), ("minor", "0.2.0"), ("major", "1.0.0"), ("0.3.7", "0.3.7")],
)
def test_next_version(part: str, expected: str) -> None:
    assert bump.next_version("0.1.4", part) == expected


@pytest.mark.parametrize("part", ["0.1.4", "0.1.3", "0.0.9"])
def test_an_explicit_version_must_move_forward(part: str) -> None:
    # PyPI and crates.io never accept a version twice; going back cannot release.
    with pytest.raises(bump.BumpError, match="must be greater than 0.1.4"):
        bump.next_version("0.1.4", part)


@pytest.mark.parametrize("part", ["", "1.2", "v0.2.0", "0.2.0-rc1", "next", "1.2.3.4"])
def test_nonsense_is_refused(part: str) -> None:
    with pytest.raises(bump.BumpError):
        bump.next_version("0.1.4", part)


def test_only_the_workspace_version_is_rewritten() -> None:
    out = bump.write_version(WORKSPACE, "0.1.5")
    assert '[workspace.package]\nversion = "0.1.5"\n' in out
    # A dependency's version requirement is not the release version.
    assert 'serde = { version = "=1.0.229" }' in out
    assert out.count("0.1.5") == 1


def test_a_manifest_without_the_workspace_version_is_refused() -> None:
    with pytest.raises(bump.BumpError, match=r"\[workspace\.package\] version"):
        bump.read_version("[workspace]\nmembers = []\n")


def test_bump_rewrites_the_manifest_and_nothing_else(tmp_path: Path) -> None:
    (tmp_path / "Cargo.toml").write_text(WORKSPACE, encoding="utf-8")
    (tmp_path / "pyproject.toml").write_text('[project]\ndynamic = ["version"]\n', encoding="utf-8")
    before = (tmp_path / "pyproject.toml").read_bytes()
    assert bump.main(["patch"], root=tmp_path, refresh_lock=False) == 0
    assert bump.read_version((tmp_path / "Cargo.toml").read_text(encoding="utf-8")) == "0.1.5"
    assert (tmp_path / "pyproject.toml").read_bytes() == before


def test_the_real_workspace_has_a_bumpable_version() -> None:
    version = bump.read_version(Path("Cargo.toml").read_text(encoding="utf-8"))
    assert bump.next_version(version, "patch") != version
