"""Staging the guest payload: pinned CPython, patched libpython, wheel, verifier."""

from __future__ import annotations

import hashlib
import io
import struct
import sys
import tarfile
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "ci"))
import recovery_payload as payload  # noqa: E402

LC_LOAD_DYLIB = 0x0C


def fake_libpython() -> bytes:
    name = b"/usr/lib/libpanel.5.4.dylib\x00"
    name += b"\x00" * (-len(name) % 8)
    command = struct.pack("<IIIIII", LC_LOAD_DYLIB, 24 + len(name), 24, 2, 0, 0) + name
    return struct.pack("<8I", 0xFEEDFACF, 0x01000007, 3, 6, 1, len(command), 0, 0) + command


def fake_cpython_archive(path: Path) -> str:
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w:gz") as archive:
        for member, data in (
            ("python/bin/python3", b"#!/bin/sh\n"),
            ("python/lib/libpython3.10.dylib", fake_libpython()),
        ):
            info = tarfile.TarInfo(member)
            info.size = len(data)
            info.mode = 0o755
            archive.addfile(info, io.BytesIO(data))
    path.write_bytes(buffer.getvalue())
    return hashlib.sha256(buffer.getvalue()).hexdigest()


def test_stage_builds_a_complete_patched_payload(tmp_path: Path) -> None:
    archive = tmp_path / "cpython.tar.gz"
    digest = fake_cpython_archive(archive)
    wheel = tmp_path / "bosn-0.1.3-cp310-abi3-macosx_10_12_x86_64.whl"
    wheel.write_bytes(b"PK\x03\x04")
    verifier = tmp_path / "verify_installed_wheel.py"
    verifier.write_text("print('verifier')\n")
    out = tmp_path / "payload"

    manifest = payload.stage(
        out, wheel=wheel, verifier=verifier, cpython_archive=archive, sha256=digest
    )

    assert (out / "python/bin/python3").exists()
    assert (out / wheel.name).read_bytes() == b"PK\x03\x04"
    assert (out / "verify_installed_wheel.py").read_text() == "print('verifier')\n"
    patched = (out / "python/lib/libpython3.10.dylib").read_bytes()
    assert b"libpanel" not in patched and b"/usr/lib/libSystem.B.dylib" in patched
    assert manifest["wheel"] == wheel.name
    assert manifest["cpython_sha256"] == digest
    assert manifest["libpython_patched"] == "python/lib/libpython3.10.dylib"


def test_stage_refuses_a_cpython_archive_with_the_wrong_digest(tmp_path: Path) -> None:
    archive = tmp_path / "cpython.tar.gz"
    fake_cpython_archive(archive)
    wheel = tmp_path / "bosn-0.1.3-cp310-abi3-macosx_10_12_x86_64.whl"
    wheel.write_bytes(b"PK")
    verifier = tmp_path / "verify_installed_wheel.py"
    verifier.write_text("")
    with pytest.raises(SystemExit, match="sha256"):
        payload.stage(
            tmp_path / "payload",
            wheel=wheel,
            verifier=verifier,
            cpython_archive=archive,
            sha256="0" * 64,
        )
    assert not (tmp_path / "payload" / "python").exists()


def test_stage_refuses_a_wheel_that_is_not_the_x86_64_darwin_abi3_artifact(tmp_path: Path) -> None:
    archive = tmp_path / "cpython.tar.gz"
    digest = fake_cpython_archive(archive)
    wheel = tmp_path / "bosn-0.1.3-cp310-abi3-macosx_11_0_arm64.whl"
    wheel.write_bytes(b"PK")
    verifier = tmp_path / "verify_installed_wheel.py"
    verifier.write_text("")
    with pytest.raises(SystemExit, match="x86_64"):
        payload.stage(
            tmp_path / "payload",
            wheel=wheel,
            verifier=verifier,
            cpython_archive=archive,
            sha256=digest,
        )


def test_pinned_cpython_is_the_abi3_floor_interpreter_for_intel_macos() -> None:
    assert "cpython-3.10." in payload.CPYTHON_URL
    assert "x86_64-apple-darwin-install_only" in payload.CPYTHON_URL
    assert len(payload.CPYTHON_SHA256) == 64
