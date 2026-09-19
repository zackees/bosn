from __future__ import annotations

import importlib.util
from pathlib import Path

import pytest

SPEC = importlib.util.spec_from_file_location("verify_release", Path("ci/verify_release.py"))
assert SPEC is not None and SPEC.loader is not None
verify_release = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(verify_release)

EXPECTED_WHEELS = (
    "bosn-0.1.4-cp310-abi3-manylinux_2_39_x86_64.whl",
    "bosn-0.1.4-cp310-abi3-win_amd64.whl",
    "bosn-0.1.4-cp310-abi3-macosx_10_12_x86_64.whl",
    "bosn-0.1.4-cp310-abi3-macosx_11_0_arm64.whl",
)


def write_tree(root: Path, version: str = "0.1.4") -> None:
    """A single-source tree: the version lives only in [workspace.package]."""
    (root / "src" / "bosn").mkdir(parents=True)
    (root / "crates" / "bosn-python" / "src").mkdir(parents=True)
    (root / "crates" / "bosn").mkdir()
    (root / "Cargo.toml").write_text(
        f'[workspace]\nmembers = []\n\n[workspace.package]\nversion = "{version}"\n',
        encoding="utf-8",
    )
    for crate in ("bosn", "bosn-python"):
        (root / "crates" / crate / "Cargo.toml").write_text(
            f'[package]\nname = "{crate}"\nversion.workspace = true\n', encoding="utf-8"
        )
    (root / "pyproject.toml").write_text(
        '[project]\nname = "bosn"\ndynamic = ["version"]\n', encoding="utf-8"
    )
    (root / "src" / "bosn" / "__init__.py").write_text(
        "from ._native import native_version\n\n__version__: str = native_version()\n",
        encoding="utf-8",
    )
    (root / "crates" / "bosn-python" / "src" / "lib.rs").write_text(
        'fn native_version() -> &\'static str { env!("CARGO_PKG_VERSION") }\n',
        encoding="utf-8",
    )


def write_wheels(directory: Path, names: tuple[str, ...]) -> None:
    directory.mkdir(exist_ok=True)
    for name in names:
        (directory / name).write_bytes(b"wheel")


def test_the_real_tree_has_one_version_and_it_matches() -> None:
    root = Path(".")
    assert verify_release.single_source_errors(root) == []
    version = verify_release.release_version(root)
    assert verify_release.source_errors(root, f"v{version}") == []


def test_matching_tag_is_accepted(tmp_path: Path) -> None:
    write_tree(tmp_path)
    assert verify_release.source_errors(tmp_path, "v0.1.4") == []


def test_tag_that_disagrees_with_the_version_is_refused(tmp_path: Path) -> None:
    write_tree(tmp_path)
    errors = verify_release.source_errors(tmp_path, "v0.1.5")
    assert errors == ["tag v0.1.5 does not match the release version 0.1.4"]


@pytest.mark.parametrize("tag", ["0.1.4", "release-0.1.4", "v0.1.4-rc1", "v0.1"])
def test_malformed_tags_are_refused(tmp_path: Path, tag: str) -> None:
    write_tree(tmp_path)
    assert verify_release.source_errors(tmp_path, tag) == [
        f"tag {tag!r} is not of the form vMAJOR.MINOR.PATCH"
    ]


# Each second copy of the version is what a bump would forget, so each is refused.


@pytest.mark.parametrize("crate", ["bosn", "bosn-python"])
def test_a_crate_that_writes_its_own_version_is_refused(tmp_path: Path, crate: str) -> None:
    write_tree(tmp_path)
    (tmp_path / "crates" / crate / "Cargo.toml").write_text(
        f'[package]\nname = "{crate}"\nversion = "0.1.4"\n', encoding="utf-8"
    )
    assert verify_release.source_errors(tmp_path, "v0.1.4") == [
        f"crates/{crate}/Cargo.toml must use `version.workspace = true`"
    ]


def test_a_literal_pyproject_version_is_refused(tmp_path: Path) -> None:
    write_tree(tmp_path)
    (tmp_path / "pyproject.toml").write_text(
        '[project]\nname = "bosn"\nversion = "0.1.4"\n', encoding="utf-8"
    )
    assert verify_release.source_errors(tmp_path, "v0.1.4") == [
        'pyproject.toml must declare `dynamic = ["version"]` and no [project].version'
    ]


@pytest.mark.parametrize("line", ['__version__ = "0.1.4"', '__version__: str = "0.1.4"'])
def test_a_literal_python_version_is_refused(tmp_path: Path, line: str) -> None:
    write_tree(tmp_path)
    (tmp_path / "src" / "bosn" / "__init__.py").write_text(line + "\n", encoding="utf-8")
    assert verify_release.source_errors(tmp_path, "v0.1.4") == [
        "src/bosn/__init__.py must derive __version__, not write a literal"
    ]


def test_a_rust_test_that_pins_the_version_is_refused(tmp_path: Path) -> None:
    write_tree(tmp_path)
    (tmp_path / "crates" / "bosn-python" / "src" / "lib.rs").write_text(
        'assert_eq!(native_version(), "0.1.4");\n', encoding="utf-8"
    )
    assert verify_release.source_errors(tmp_path, "v0.1.4") == [
        "crates/bosn-python/src/lib.rs must not pin native_version() to a literal"
    ]


def test_the_four_platform_wheels_are_accepted(tmp_path: Path) -> None:
    write_wheels(tmp_path / "dist", EXPECTED_WHEELS)
    assert verify_release.wheel_errors(tmp_path / "dist", "v0.1.4") == []


def test_a_missing_platform_is_refused(tmp_path: Path) -> None:
    write_wheels(tmp_path / "dist", EXPECTED_WHEELS[:3])
    assert verify_release.wheel_errors(tmp_path / "dist", "v0.1.4") == [
        "no wheel for macOS arm64 (macosx_11_0_arm64)"
    ]


def test_a_bare_linux_tag_is_refused_because_pypi_rejects_it(tmp_path: Path) -> None:
    names = (*EXPECTED_WHEELS[1:], "bosn-0.1.4-cp310-abi3-linux_x86_64.whl")
    write_wheels(tmp_path / "dist", names)
    errors = verify_release.wheel_errors(tmp_path / "dist", "v0.1.4")
    assert "unexpected wheel bosn-0.1.4-cp310-abi3-linux_x86_64.whl" in errors
    assert "no wheel for Linux x86_64 (manylinux_*_x86_64)" in errors


def test_a_non_abi3_or_wrong_version_wheel_is_refused(tmp_path: Path) -> None:
    names = (
        *EXPECTED_WHEELS[:3],
        "bosn-0.1.4-cp313-cp313-macosx_11_0_arm64.whl",
        "bosn-0.1.3-cp310-abi3-macosx_11_0_arm64.whl",
    )
    write_wheels(tmp_path / "dist", names)
    errors = verify_release.wheel_errors(tmp_path / "dist", "v0.1.4")
    assert "unexpected wheel bosn-0.1.4-cp313-cp313-macosx_11_0_arm64.whl" in errors
    assert "unexpected wheel bosn-0.1.3-cp310-abi3-macosx_11_0_arm64.whl" in errors
    assert "no wheel for macOS arm64 (macosx_11_0_arm64)" in errors


def test_two_wheels_for_one_platform_are_refused(tmp_path: Path) -> None:
    names = (*EXPECTED_WHEELS, "bosn-0.1.4-cp310-abi3-manylinux_2_28_x86_64.whl")
    write_wheels(tmp_path / "dist", names)
    assert verify_release.wheel_errors(tmp_path / "dist", "v0.1.4") == [
        "2 wheels for Linux x86_64 (manylinux_*_x86_64); expected exactly one"
    ]


def test_an_sdist_or_stray_file_is_refused(tmp_path: Path) -> None:
    write_wheels(tmp_path / "dist", (*EXPECTED_WHEELS, "bosn-0.1.4.tar.gz"))
    assert verify_release.wheel_errors(tmp_path / "dist", "v0.1.4") == [
        "unexpected file bosn-0.1.4.tar.gz"
    ]
