"""PEP 517 wrapper that puts Bosn's Rust CLI in every platform wheel.

Maturin's PyO3 bridge builds the extension target but intentionally does not
also package a Cargo binary from the same mixed project.  Build the shared
native CLI first, then let maturin include that artifact in the wheel.
"""

from __future__ import annotations

from collections.abc import Iterator, Mapping
from contextlib import contextmanager
from dataclasses import dataclass
from os import chmod, environ
from os import name as os_name
from pathlib import Path
from re import compile as compile_regex
from shutil import copy2, rmtree
from struct import unpack_from
from subprocess import run
from sys import platform
from typing import Any

import maturin

_ROOT = Path(__file__).resolve().parent
_WHEEL_DATA = _ROOT / "target" / "bosn-wheel-data"
# The CLI is staged into the wheel's ``.data/scripts`` tree so pip installs it
# as the ``bosn`` command on PATH (like soldr ships its own binary), rather than
# behind a Python launcher.  On Linux its OpenSSL sidecars ride in the same
# directory and an ``$ORIGIN`` rpath finds them, so the binary is self-contained
# with no wrapper configuring ``LD_LIBRARY_PATH``.
_WHEEL_NATIVE_DIRECTORY = _WHEEL_DATA / "scripts"
_LINUX_OPENSSL = compile_regex(r"^(lib(?:ssl|crypto)\.so\.\d+) => (\S+)")
_MACHO_64_MAGIC = 0xFEEDFACF


@dataclass(frozen=True)
class _DarwinTarget:
    triple: str
    cputype: int
    deployment_target: str


# A target is an explicit release contract, not an arbitrary Cargo string.  In
# particular, accepting an unknown target would let the backend put a host ELF
# executable into a wheel with a Darwin filename (the false-green #252 exists
# to prevent).  Add a target here together with its static verifier contract.
_DARWIN_TARGETS = {
    "x86_64-apple-darwin": _DarwinTarget("x86_64-apple-darwin", 0x01000007, "10.12"),
    "aarch64-apple-darwin": _DarwinTarget("aarch64-apple-darwin", 0x0100000C, "11.0"),
}


def _wheel_target() -> _DarwinTarget | None:
    requested = environ.get("BOSN_WHEEL_TARGET")
    if not requested:
        return None
    try:
        return _DARWIN_TARGETS[requested]
    except KeyError as error:
        allowed = ", ".join(sorted(_DARWIN_TARGETS))
        raise RuntimeError(
            f"BOSN_WHEEL_TARGET={requested!r} is not a supported Bosn wheel target; "
            f"allowed values: {allowed}"
        ) from error


# The command installed on PATH.  The Cargo bin stays `bosn-native` (the
# workspace already has a distinct `bosn` bin in bosn-service; a second one
# would collide on `target/<profile>/bosn`), so the backend renames it to the
# command name while staging it into the wheel's scripts tree.
def _command_name() -> str:
    return "bosn.exe" if os_name == "nt" else "bosn"


def _native_cli(target: _DarwinTarget | None) -> Path:
    name = "bosn-native.exe" if target is None and os_name == "nt" else "bosn-native"
    directory = _ROOT / "target"
    if target is not None:
        directory /= target.triple
    return directory / "release" / name


def _assert_target_magic(binary: Path, target: _DarwinTarget | None) -> None:
    """Refuse a wheel whose staged CLI is not the requested native format."""

    if target is None:
        return
    data = binary.read_bytes()
    if len(data) < 16:
        raise RuntimeError(f"cross-built CLI is too short to be Mach-O: {binary}")
    magic, cputype, _cpusubtype, filetype = unpack_from("<IiiI", data)
    if magic != _MACHO_64_MAGIC or cputype != target.cputype or filetype != 2:
        raise RuntimeError(
            f"cross-built CLI does not match {target.triple}: "
            f"magic={magic:#x}, cputype={cputype:#x}, filetype={filetype}; "
            "refusing to stage a host or wrong-architecture executable"
        )


@contextmanager
def _cross_pyo3_environment(target: _DarwinTarget | None) -> Iterator[None]:
    """Make both the pre-built CLI and maturin's cdylib use the same target."""

    if target is None:
        yield
        return
    old_version = environ.get("PYO3_CROSS_PYTHON_VERSION")
    old_deployment = environ.get("MACOSX_DEPLOYMENT_TARGET")
    # Maturin sets this for its own cargo call in some modes, but the CLI is a
    # separate build.  Set it here for both calls and restore the caller's env.
    environ["PYO3_CROSS_PYTHON_VERSION"] = "3.10"
    environ.setdefault("MACOSX_DEPLOYMENT_TARGET", target.deployment_target)
    try:
        yield
    finally:
        if old_version is None:
            environ.pop("PYO3_CROSS_PYTHON_VERSION", None)
        else:
            environ["PYO3_CROSS_PYTHON_VERSION"] = old_version
        if old_deployment is None:
            environ.pop("MACOSX_DEPLOYMENT_TARGET", None)
        else:
            environ["MACOSX_DEPLOYMENT_TARGET"] = old_deployment


@contextmanager
def _linux_rpath_environment(target: _DarwinTarget | None) -> Iterator[None]:
    """Give the Linux CLI an ``$ORIGIN`` rpath so it finds co-located OpenSSL.

    Only the native Linux host build needs this: macOS links system frameworks
    and Windows uses SChannel, neither of which ships a sidecar.  A cross build
    (``target`` set) carries its own soldr-provided linker flags and is left
    alone.  The flag is a final-link argument, so appending it to ``RUSTFLAGS``
    is safe for dependency compiles.
    """

    if target is not None or not platform.startswith("linux"):
        yield
        return
    key = "RUSTFLAGS"
    previous = environ.get(key)
    flag = "-C link-arg=-Wl,-rpath,$ORIGIN"
    environ[key] = f"{previous} {flag}" if previous else flag
    try:
        yield
    finally:
        if previous is None:
            environ.pop(key, None)
        else:
            environ[key] = previous


def _build_native_cli() -> None:
    target = _wheel_target()
    command = [
        "cargo",
        "build",
        "--release",
        "--locked",
        "--package",
        "bosn-python",
        "--bin",
        "bosn-native",
    ]
    if target is not None:
        command.extend(["--target", target.triple])
    with _cross_pyo3_environment(target), _linux_rpath_environment(target):
        run(command, cwd=_ROOT, check=True)
    native_cli = _native_cli(target)
    _assert_target_magic(native_cli, target)
    rmtree(_WHEEL_DATA, ignore_errors=True)
    destination = _WHEEL_NATIVE_DIRECTORY / _command_name()
    destination.parent.mkdir(parents=True, exist_ok=True)
    copy2(native_cli, destination)
    chmod(destination, native_cli.stat().st_mode)
    _copy_linux_openssl(destination.parent, native_cli, target)


def _copy_linux_openssl(destination: Path, native_cli: Path, target: _DarwinTarget | None) -> None:
    """Keep the standalone Linux executable independent of host OpenSSL."""

    # Sidecars belong to the output target, never to the Linux builder.  The
    # Darwin branch must remain empty even though this backend runs on Linux.
    if target is not None or not platform.startswith("linux"):
        return
    output = run(["ldd", native_cli], capture_output=True, check=True, text=True).stdout
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
    target = _wheel_target()
    if target is not None:
        # Maturin receives an explicit target too; otherwise it may compile the
        # extension for the Linux builder while the backend stages a Mach-O CLI.
        if "--target" not in arguments:
            arguments.extend(["--target", target.triple])
        if "--interpreter" not in arguments and "-i" not in arguments:
            arguments.extend(["--interpreter", "python3.11"])
    settings["maturin.build-args"] = arguments
    return settings


def build_wheel(
    wheel_directory: str,
    config_settings: Mapping[str, Any] | None = None,
    metadata_directory: str | None = None,
) -> str:
    target = _wheel_target()
    with _cross_pyo3_environment(target):
        _build_native_cli()
        return maturin.build_wheel(
            wheel_directory, _wheel_config(config_settings), metadata_directory
        )


def build_editable(
    wheel_directory: str,
    config_settings: Mapping[str, Any] | None = None,
    metadata_directory: str | None = None,
) -> str:
    target = _wheel_target()
    if target is not None:
        raise RuntimeError("cross-target editable wheels are unsupported; build a wheel instead")
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
