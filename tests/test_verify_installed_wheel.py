"""Regression coverage for the native wheel ABI3 archive contract."""

from __future__ import annotations

import sys
import zipfile
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "ci"))
import verify_installed_wheel as verifier  # noqa: E402


def write_windows_wheel(path: Path, *, tag: str) -> None:
    with zipfile.ZipFile(path, "w") as wheel:
        wheel.writestr("bosn/_native.pyd", b"native-dll")
        wheel.writestr("bosn-0.1.3.data/platlib/bosn/_bin/bosn-native.exe", b"native-cli")
        wheel.writestr("bosn-0.1.3.dist-info/WHEEL", f"Wheel-Version: 1.0\nTag: {tag}\n")


def test_windows_pyd_is_accepted_only_when_the_wheel_tag_is_abi3(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    wheel = tmp_path / "bosn-0.1.3-cp310-abi3-win_amd64.whl"
    write_windows_wheel(wheel, tag="cp310-abi3-win_amd64")
    monkeypatch.setattr(verifier.os, "name", "nt")

    verifier.assert_platform_wheel_contents(wheel)


def test_windows_pyd_rejects_non_abi3_wheel_tag(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    wheel = tmp_path / "bosn-0.1.3-cp311-cp311-win_amd64.whl"
    write_windows_wheel(wheel, tag="cp311-cp311-win_amd64")
    monkeypatch.setattr(verifier.os, "name", "nt")

    with pytest.raises(AssertionError, match="cp310-abi3"):
        verifier.assert_platform_wheel_contents(wheel)


@pytest.mark.parametrize(
    ("os_name", "expected_extension"),
    [("nt", "_native.pyd"), ("posix", "_native.abi3.so")],
)
def test_inline_smoke_script_uses_the_platform_extension_contract(
    monkeypatch: pytest.MonkeyPatch, os_name: str, expected_extension: str
) -> None:
    monkeypatch.setattr(verifier.os, "name", os_name)

    script = verifier.installed_extension_smoke_script()

    assert f'assert origin.name == "{expected_extension}"' in script
