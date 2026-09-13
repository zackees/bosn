"""PEP 517 wrapper that puts Bosn's Rust CLI in every platform wheel.

Maturin's PyO3 bridge builds the extension target but intentionally does not
also package a Cargo binary from the same mixed project.  Build the shared
native CLI first, then let maturin include that artifact in the wheel.
"""

from __future__ import annotations

from collections.abc import Mapping
from os import chmod
from os import name as os_name
from pathlib import Path
from re import compile as compile_regex
from shutil import copy2, rmtree
from subprocess import run
from sys import platform
from typing import Any

import maturin

_ROOT = Path(__file__).resolve().parent
_NATIVE_CLI = (
    _ROOT / "target" / "release" / ("bosn-native.exe" if os_name == "nt" else "bosn-native")
)
_WHEEL_NATIVE_DIRECTORY = _ROOT / "target" / "bosn-wheel-data" / "platlib" / "bosn" / "_bin"
_WHEEL_DATA = _ROOT / "target" / "bosn-wheel-data"
_LINUX_OPENSSL = compile_regex(r"^(lib(?:ssl|crypto)\.so\.\d+) => (\S+)")


def _build_native_cli() -> None:
    run(
        [
            "cargo",
            "build",
            "--release",
            "--locked",
            "--package",
            "bosn",
            "--bin",
            "bosn-native",
        ],
        cwd=_ROOT,
        check=True,
    )
    rmtree(_WHEEL_DATA, ignore_errors=True)
    destination = _WHEEL_NATIVE_DIRECTORY / _NATIVE_CLI.name
    destination.parent.mkdir(parents=True, exist_ok=True)
    copy2(_NATIVE_CLI, destination)
    chmod(destination, _NATIVE_CLI.stat().st_mode)
    _copy_linux_openssl(destination.parent)


def _copy_linux_openssl(destination: Path) -> None:
    """Keep the standalone Linux executable independent of host OpenSSL."""

    if not platform.startswith("linux"):
        return
    output = run(["ldd", _NATIVE_CLI], capture_output=True, check=True, text=True).stdout
    copied = set()
    for line in output.splitlines():
        match = _LINUX_OPENSSL.match(line.strip())
        if match is None:
            continue
        name, source = match.groups()
        copy2(source, destination / name)
        copied.add(name)
    if copied != {"libssl.so.3", "libcrypto.so.3"}:
        raise RuntimeError("native CLI was not linked to the expected OpenSSL 3 runtime libraries")


def _wheel_config(config_settings: Mapping[str, Any] | None) -> dict[str, Any]:
    """Request an audited, platform-tagged wheel from maturin's PEP 517 path."""

    settings = dict(config_settings or {})
    arguments = settings.get("maturin.build-args", settings.get("build-args", []))
    if isinstance(arguments, str):
        arguments = arguments.split()
    else:
        arguments = list(arguments)
    if "--compatibility" not in arguments and "--manylinux" not in arguments:
        arguments.extend(["--compatibility", "pypi"])
    settings["maturin.build-args"] = arguments
    return settings


def build_wheel(
    wheel_directory: str,
    config_settings: Mapping[str, Any] | None = None,
    metadata_directory: str | None = None,
) -> str:
    _build_native_cli()
    return maturin.build_wheel(wheel_directory, _wheel_config(config_settings), metadata_directory)


def build_editable(
    wheel_directory: str,
    config_settings: Mapping[str, Any] | None = None,
    metadata_directory: str | None = None,
) -> str:
    _build_native_cli()
    return maturin.build_editable(
        wheel_directory, _wheel_config(config_settings), metadata_directory
    )


get_requires_for_build_wheel = maturin.get_requires_for_build_wheel
get_requires_for_build_editable = maturin.get_requires_for_build_editable
get_requires_for_build_sdist = maturin.get_requires_for_build_sdist
prepare_metadata_for_build_wheel = maturin.prepare_metadata_for_build_wheel
prepare_metadata_for_build_editable = maturin.prepare_metadata_for_build_editable
build_sdist = maturin.build_sdist
