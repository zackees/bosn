"""Cross-language guard for an irreversible Python-to-Rust state cutover.

The lock file is deliberately separate from SQLite.  Every bridge-capable Python
writer holds a shared advisory lock for the lifetime of its Registry connection;
the Rust importer takes the same byte exclusively through kernal-api before it
backs up or imports.  Read-only Python access remains side-effect-free.
"""

from __future__ import annotations

import ctypes
import json
import os
import stat
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import BinaryIO

CUTOVER_MARKER = "rust-cutover-v1.json"
LOCK_FILE = "registry.migration.lock"
_PROTOCOL = 1
_WINDOWS_LOCK_OFFSET = 1 << 62


class CutoverError(RuntimeError):
    """The state directory is cut over, malformed, or cannot be safely guarded."""


def _marker_path(state_dir: Path) -> Path:
    return state_dir / CUTOVER_MARKER


def assert_python_writes_allowed(state_dir: Path) -> None:
    """Refuse every post-cutover legacy write, including a malformed marker."""
    marker = _marker_path(state_dir)
    try:
        mode = marker.lstat().st_mode
    except FileNotFoundError:
        return
    except OSError as exc:
        raise CutoverError(f"cannot inspect Rust migration cutover marker at {marker}") from exc
    if not stat.S_ISREG(mode):
        raise CutoverError(f"invalid Rust migration cutover marker entry at {marker}")
    try:
        value = json.loads(marker.read_text(encoding="utf-8"))
        if not isinstance(value, dict):
            raise ValueError("cutover marker is not an object")
        if value.get("protocol") != _PROTOCOL or not isinstance(value.get("registry_id"), str):
            raise ValueError("invalid cutover marker")
    except (AttributeError, OSError, ValueError, json.JSONDecodeError) as exc:
        raise CutoverError(f"invalid Rust migration cutover marker at {marker}") from exc
    raise CutoverError(
        "state directory has completed Rust migration cutover; legacy writes refused"
    )


def publish_cutover_marker(state_dir: Path, registry_id: str) -> Path:
    """Publish an owner-private, no-replace cutover marker after daemon admission closes."""
    if not registry_id:
        raise CutoverError("cannot publish a cutover marker without a registry id")
    state_dir.mkdir(parents=True, exist_ok=True)
    marker = _marker_path(state_dir)
    payload = (
        json.dumps({"protocol": _PROTOCOL, "registry_id": registry_id}, sort_keys=True) + "\n"
    ).encode()
    try:
        descriptor = os.open(marker, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    except FileExistsError as exc:
        raise CutoverError(f"cutover marker already exists at {marker}") from exc
    try:
        with os.fdopen(descriptor, "wb", closefd=True) as output:
            output.write(payload)
            output.flush()
            os.fsync(output.fileno())
    except KeyboardInterrupt:
        # A partial marker remains a fail-closed recovery condition even when the
        # operator interrupts publication.
        raise
    except Exception:
        # The marker name belongs to this attempt because O_EXCL created it. A partial
        # marker must remain: deleting it would permit a legacy restart after admission
        # was closed. It is intentionally a fail-closed recovery condition.
        raise
    if os.name != "nt":
        try:
            parent_fd = os.open(state_dir, os.O_RDONLY)
            try:
                os.fsync(parent_fd)
            finally:
                os.close(parent_fd)
        except OSError as exc:
            raise CutoverError(f"could not durably publish cutover marker at {marker}") from exc
    return marker


@dataclass
class SharedMigrationLock:
    _file: BinaryIO

    def close(self) -> None:
        if self._file.closed:
            return
        try:
            _unlock(self._file)
        finally:
            self._file.close()


def acquire_shared(state_dir: Path) -> SharedMigrationLock:
    """Take the bridge's shared guard, then recheck the marker under that guard."""
    state_dir.mkdir(parents=True, exist_ok=True)
    path = state_dir / LOCK_FILE
    descriptor = os.open(path, os.O_RDWR | os.O_CREAT, 0o600)
    output = os.fdopen(descriptor, "r+b", buffering=0)
    try:
        _lock_shared(output)
        # The ordering is essential: an importer can take EX and publish the marker only
        # after existing SH holders leave; checking before SH would race that publication.
        assert_python_writes_allowed(state_dir)
    except KeyboardInterrupt:
        try:
            _unlock(output)
        finally:
            output.close()
        raise
    except Exception:
        try:
            _unlock(output)
        finally:
            output.close()
        raise
    return SharedMigrationLock(output)


def _lock_shared(output: BinaryIO) -> None:
    if os.name != "nt":
        import fcntl

        fcntl.flock(output.fileno(), fcntl.LOCK_SH)
        return
    _windows_lock(output, exclusive=False)


def _unlock(output: BinaryIO) -> None:
    if os.name != "nt":
        import fcntl

        fcntl.flock(output.fileno(), fcntl.LOCK_UN)
        return
    _windows_unlock(output)


if os.name == "nt":
    import msvcrt
    from ctypes import wintypes

    class _Overlapped(ctypes.Structure):
        _fields_ = [
            ("Internal", ctypes.c_size_t),
            ("InternalHigh", ctypes.c_size_t),
            ("Offset", wintypes.DWORD),
            ("OffsetHigh", wintypes.DWORD),
            ("hEvent", wintypes.HANDLE),
        ]

    _kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    _lock_file_ex = _kernel32.LockFileEx
    _lock_file_ex.argtypes = [
        wintypes.HANDLE,
        wintypes.DWORD,
        wintypes.DWORD,
        wintypes.DWORD,
        wintypes.DWORD,
        ctypes.POINTER(_Overlapped),
    ]
    _lock_file_ex.restype = wintypes.BOOL
    _unlock_file_ex = _kernel32.UnlockFileEx
    _unlock_file_ex.argtypes = [
        wintypes.HANDLE,
        wintypes.DWORD,
        wintypes.DWORD,
        wintypes.DWORD,
        ctypes.POINTER(_Overlapped),
    ]
    _unlock_file_ex.restype = wintypes.BOOL
    _LOCKFILE_EXCLUSIVE_LOCK = 0x00000002

    def _overlapped() -> _Overlapped:
        return _Overlapped(
            0,
            0,
            _WINDOWS_LOCK_OFFSET & 0xFFFFFFFF,
            _WINDOWS_LOCK_OFFSET >> 32,
            wintypes.HANDLE(),
        )

    def _windows_lock(output: BinaryIO, *, exclusive: bool) -> None:
        flags = _LOCKFILE_EXCLUSIVE_LOCK if exclusive else 0
        overlapped = _overlapped()
        handle = wintypes.HANDLE(msvcrt.get_osfhandle(output.fileno()))
        if not _lock_file_ex(handle, flags, 0, 1, 0, ctypes.byref(overlapped)):
            raise OSError(ctypes.get_last_error(), "LockFileEx failed")

    def _windows_unlock(output: BinaryIO) -> None:
        overlapped = _overlapped()
        handle = wintypes.HANDLE(msvcrt.get_osfhandle(output.fileno()))
        if not _unlock_file_ex(handle, 0, 1, 0, ctypes.byref(overlapped)):
            raise OSError(ctypes.get_last_error(), "UnlockFileEx failed")

else:

    def _windows_lock(_output: BinaryIO, *, exclusive: bool) -> None:
        raise AssertionError(f"Windows lock requested on {sys.platform}: exclusive={exclusive}")

    def _windows_unlock(_output: BinaryIO) -> None:
        raise AssertionError(f"Windows unlock requested on {sys.platform}")
