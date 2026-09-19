#!/usr/bin/env python3
"""Statically verify a Linux-built Darwin Bosn wheel without executing it.

Linux cannot import a Mach-O extension.  This verifier deliberately reads the
wheel and Mach-O load commands itself, so the release gate is independent of
the builder's LLVM installation and catches a stale Linux CLI sidecar.
"""

from __future__ import annotations

import argparse
import zipfile
from dataclasses import dataclass
from pathlib import Path
from struct import unpack_from
from typing import NoReturn

import tomllib

RETIRED_LIFECYCLE_MODULES = (
    "daemon",
    "engine",
    "registry",
    "converge",
    "gc",
    "resources",
    "recovery",
    "docker_cli",
    "compose",
    "manifest",
    "guest",
)
MH_MAGIC_64 = 0xFEEDFACF
MH_DYLIB = 6
MH_EXECUTE = 2
LC_LOAD_DYLIB = 0xC
LC_LOAD_WEAK_DYLIB = 0x18
LC_REEXPORT_DYLIB = 0x1F
LC_LOAD_UPWARD_DYLIB = 0x23
LC_LAZY_LOAD_DYLIB = 0x20
LC_VERSION_MIN_MACOSX = 0x24
LC_BUILD_VERSION = 0x32
DYLIB_COMMANDS = frozenset(
    {
        LC_LOAD_DYLIB,
        LC_LOAD_WEAK_DYLIB,
        LC_REEXPORT_DYLIB,
        LC_LOAD_UPWARD_DYLIB,
        LC_LAZY_LOAD_DYLIB,
    }
)


@dataclass(frozen=True)
class Target:
    triple: str
    tag: str
    cputype: int
    floor: tuple[int, int, int]


TARGETS = {
    "x86_64-apple-darwin": Target(
        "x86_64-apple-darwin", "macosx_10_12_x86_64", 0x01000007, (10, 12, 0)
    ),
    "aarch64-apple-darwin": Target(
        "aarch64-apple-darwin", "macosx_11_0_arm64", 0x0100000C, (11, 0, 0)
    ),
}


def fail(message: str) -> NoReturn:
    raise AssertionError(message)


def _version(raw: int) -> tuple[int, int, int]:
    return (raw >> 16, (raw >> 8) & 0xFF, raw & 0xFF)


def _nul(data: bytes, offset: int) -> str:
    end = data.find(b"\0", offset)
    if end < 0:
        end = len(data)
    return data[offset:end].decode("utf-8", errors="strict")


def inspect_macho(data: bytes, *, target: Target, expected_filetype: int, name: str) -> None:
    if len(data) < 32:
        fail(f"{name}: too short to be a Mach-O 64 file")
    magic, cputype, _subtype, filetype, command_count, command_size, _flags, _reserved = (
        unpack_from("<IiiIIIII", data)
    )
    if magic != MH_MAGIC_64:
        fail(f"{name}: expected Mach-O 64 magic, got {magic:#x}")
    if cputype != target.cputype:
        fail(f"{name}: cputype {cputype:#x}, expected {target.cputype:#x} for {target.triple}")
    if filetype != expected_filetype:
        fail(f"{name}: Mach-O filetype {filetype}, expected {expected_filetype}")
    end = 32 + command_size
    if end > len(data):
        fail(f"{name}: load commands run past end of file")
    offset = 32
    deployment_versions: list[tuple[int, int, int]] = []
    for _ in range(command_count):
        if offset + 8 > end:
            fail(f"{name}: truncated Mach-O load command")
        command, size = unpack_from("<II", data, offset)
        if size < 8 or offset + size > end:
            fail(f"{name}: malformed Mach-O load command size {size}")
        base_command = command & ~0x80000000
        if base_command in DYLIB_COMMANDS:
            if size < 24:
                fail(f"{name}: truncated dylib load command")
            name_offset = unpack_from("<I", data, offset + 8)[0]
            if name_offset >= size:
                fail(f"{name}: invalid dylib path offset")
            dylib = _nul(data, offset + name_offset)
            if not dylib.startswith(("/usr/lib/", "/System/Library/Frameworks/")):
                fail(f"{name}: non-system dylib dependency {dylib!r}")
        elif base_command == LC_VERSION_MIN_MACOSX:
            if size < 16:
                fail(f"{name}: truncated LC_VERSION_MIN_MACOSX")
            deployment_versions.append(_version(unpack_from("<I", data, offset + 8)[0]))
        elif base_command == LC_BUILD_VERSION:
            if size < 24:
                fail(f"{name}: truncated LC_BUILD_VERSION")
            deployment_versions.append(_version(unpack_from("<I", data, offset + 12)[0]))
        offset += size
    if not deployment_versions:
        fail(f"{name}: no macOS deployment-minimum load command")
    if any(version > target.floor for version in deployment_versions):
        fail(f"{name}: deployment minimum {deployment_versions} exceeds tag floor {target.floor}")


def project_version(root: Path) -> str:
    """`[workspace.package].version`: the only place the release version is written.

    `bosn-python` inherits it and maturin reads it for the wheel (`dynamic = ["version"]`),
    so the wheel filename checked below is the real cross-check.
    """
    workspace = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))["workspace"]
    version = workspace.get("package", {}).get("version")
    if not isinstance(version, str):
        fail("Cargo.toml: [workspace.package] version is missing")
    return version


def wheel_from_argument(argument: str) -> Path:
    candidates = (
        sorted(Path().glob(argument))
        if any(char in argument for char in "*?[")
        else [Path(argument)]
    )
    if len(candidates) != 1 or not candidates[0].is_file():
        fail(f"expected exactly one wheel, got {candidates}")
    return candidates[0].resolve()


def verify(wheel: Path, target: Target, root: Path) -> None:
    version = project_version(root)
    required_tag = f"cp310-abi3-{target.tag}"
    if f"-{required_tag}.whl" not in wheel.name or not wheel.name.startswith(f"bosn-{version}-"):
        fail(f"wheel filename {wheel.name!r} must contain {required_tag!r} and version {version!r}")
    with zipfile.ZipFile(wheel) as archive:
        names = archive.namelist()
        wheel_metadata = next((name for name in names if name.endswith(".dist-info/WHEEL")), None)
        package_metadata = next(
            (name for name in names if name.endswith(".dist-info/METADATA")), None
        )
        if wheel_metadata is None or package_metadata is None:
            fail("wheel lacks .dist-info WHEEL or METADATA")
        if f"Tag: {required_tag}" not in archive.read(wheel_metadata).decode("utf-8"):
            fail(f"WHEEL does not declare {required_tag}")
        if f"Version: {version}\n" not in archive.read(package_metadata).decode("utf-8"):
            fail(f"METADATA does not declare version {version}")
        retired = [
            f"bosn/{module}.py"
            for module in RETIRED_LIFECYCLE_MODULES
            if f"bosn/{module}.py" in names
        ]
        if retired:
            fail(f"wheel includes retired lifecycle modules: {retired}")
        forbidden = [
            name
            for name in names
            if name.endswith(".exe")
            or name.endswith(".so.3")
            or archive.read(name).startswith(b"\x7fELF")
        ]
        if forbidden:
            fail(f"Darwin wheel contains non-Darwin/sidecar artifacts: {forbidden}")
        extension = "bosn/_native.abi3.so"
        cli = next((name for name in names if name.endswith(".data/scripts/bosn")), None)
        if extension not in names or cli is None:
            fail(
                "wheel misses Darwin extension or packaged CLI: "
                f"extension={extension in names}, cli={cli}"
            )
        inspect_macho(
            archive.read(extension), target=target, expected_filetype=MH_DYLIB, name=extension
        )
        cli_data = archive.read(cli)
        inspect_macho(cli_data, target=target, expected_filetype=MH_EXECUTE, name=cli)
        if f"bosn {version}".encode() not in cli_data:
            fail(f"{cli}: missing embedded version string bosn {version}")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("wheel")
    parser.add_argument("--target", required=True, choices=sorted(TARGETS))
    arguments = parser.parse_args(argv)
    verify(
        wheel_from_argument(arguments.wheel),
        TARGETS[arguments.target],
        Path(__file__).resolve().parents[1],
    )
    print(f"cross-wheel verifier: {arguments.target} wheel is staticly valid")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
