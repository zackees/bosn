"""The one-dylib rewrite that lets python-build-standalone load in macOS Recovery."""

from __future__ import annotations

import struct
import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "ci"))
import patch_recovery_python as patcher  # noqa: E402

LC_LOAD_DYLIB = 0x0C


def dylib_command(path: bytes) -> bytes:
    # cmd, cmdsize, name offset, timestamp, current, compat, then the NUL-terminated
    # name padded to an 8-byte boundary, exactly as ld64 lays it out.
    name = path + b"\x00"
    name += b"\x00" * (-len(name) % 8)
    size = 24 + len(name)
    return struct.pack("<IIIIII", LC_LOAD_DYLIB, size, 24, 2, 0x10000, 0x10000) + name


def macho(commands: list[bytes]) -> bytes:
    body = b"".join(commands)
    header = struct.pack("<8I", 0xFEEDFACF, 0x01000007, 3, 6, len(commands), len(body), 0, 0)
    return header + body


def test_patch_rewrites_exactly_the_libpanel_load_command(tmp_path: Path) -> None:
    library = tmp_path / "libpython3.10.dylib"
    library.write_bytes(
        macho(
            [
                dylib_command(b"/usr/lib/libncurses.5.4.dylib"),
                dylib_command(b"/usr/lib/libpanel.5.4.dylib"),
                dylib_command(b"/usr/lib/libSystem.B.dylib"),
            ]
        )
    )
    before = library.read_bytes()

    patcher.patch(str(library))

    after = library.read_bytes()
    assert len(after) == len(before)
    assert b"/usr/lib/libpanel.5.4.dylib" not in after
    assert after.count(b"/usr/lib/libSystem.B.dylib\x00") == 2
    assert b"/usr/lib/libncurses.5.4.dylib\x00" in after
    # The tail of the old, longer path must not survive past the new NUL.
    assert b"5.4.dylib" not in after.split(b"/usr/lib/libSystem.B.dylib\x00", 1)[1][:8]


def test_patch_refuses_when_the_reference_is_absent(tmp_path: Path) -> None:
    library = tmp_path / "libpython3.10.dylib"
    library.write_bytes(macho([dylib_command(b"/usr/lib/libSystem.B.dylib")]))
    with pytest.raises(SystemExit, match="expected exactly 1"):
        patcher.patch(str(library))


def test_patch_refuses_a_non_macho_file(tmp_path: Path) -> None:
    library = tmp_path / "libpython3.10.dylib"
    library.write_bytes(b"\x7fELF" + b"\x00" * 60)
    with pytest.raises(SystemExit, match="not a 64-bit little-endian Mach-O"):
        patcher.patch(str(library))


def test_replacement_fits_the_fixed_size_load_command() -> None:
    assert len(patcher.NEW) <= len(patcher.OLD)
