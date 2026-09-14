"""Small parser fixtures for the Linux-side Darwin wheel verifier."""

from __future__ import annotations

import struct
import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "ci"))
import verify_cross_wheel as verifier  # noqa: E402


def macho(
    target: verifier.Target, filetype: int, *, dylib: str = "/usr/lib/libSystem.B.dylib"
) -> bytes:
    """A minimal Mach-O 64 fixture with one system dylib and a 10.12 min OS."""

    dylib_bytes = dylib.encode() + b"\0"
    dylib_size = 24 + len(dylib_bytes)
    dylib_size += (-dylib_size) % 8
    dylib_command = struct.pack("<IIIIII", verifier.LC_LOAD_DYLIB, dylib_size, 24, 0, 0, 0)
    dylib_command += dylib_bytes + b"\0" * (dylib_size - 24 - len(dylib_bytes))
    min_command = struct.pack("<IIII", verifier.LC_VERSION_MIN_MACOSX, 16, 0x000A0C00, 0)
    commands = dylib_command + min_command
    return (
        struct.pack(
            "<IiiIIIII",
            verifier.MH_MAGIC_64,
            target.cputype,
            3,
            filetype,
            2,
            len(commands),
            0,
            0,
        )
        + commands
    )


def test_accepts_matching_macho() -> None:
    target = verifier.TARGETS["x86_64-apple-darwin"]
    verifier.inspect_macho(
        macho(target, verifier.MH_EXECUTE),
        target=target,
        expected_filetype=verifier.MH_EXECUTE,
        name="cli",
    )


def test_rejects_non_system_dylib() -> None:
    target = verifier.TARGETS["x86_64-apple-darwin"]
    with pytest.raises(AssertionError, match="non-system dylib"):
        verifier.inspect_macho(
            macho(target, verifier.MH_EXECUTE, dylib="/tmp/libssl.dylib"),
            target=target,
            expected_filetype=verifier.MH_EXECUTE,
            name="cli",
        )


def test_rejects_wrong_filetype() -> None:
    target = verifier.TARGETS["aarch64-apple-darwin"]
    with pytest.raises(AssertionError, match="filetype"):
        verifier.inspect_macho(
            macho(target, verifier.MH_DYLIB),
            target=target,
            expected_filetype=verifier.MH_EXECUTE,
            name="cli",
        )
