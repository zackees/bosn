"""Installed launcher for the version-matched Rust Bosn executable."""

from __future__ import annotations

import os
import sys
import sysconfig
from pathlib import Path

_DLL_DIRECTORIES: list[object] = []


def native_executable() -> Path:
    """Return the Rust executable installed with this wheel.

    Resolve its package-local path instead of searching ``PATH`` so a source
    checkout or another Bosn installation can never be selected accidentally.
    """

    suffix = ".exe" if os.name == "nt" else ""
    filename = f"bosn-native{suffix}"
    candidates = [Path(__file__).resolve().parent / "_bin" / filename]
    platlib = sysconfig.get_path("platlib")
    if platlib is not None:
        candidates.append(Path(platlib) / "bosn" / "_bin" / filename)
    for executable in candidates:
        if executable.is_file():
            return executable
    raise RuntimeError(
        "the installed Bosn wheel does not contain its native CLI; "
        "reinstall a platform wheel matching this Python environment"
    )


def _configure_native_library_path(executable: Path) -> None:
    """Make the wheel's audited native libraries visible to the CLI process."""

    packaged = executable.parent
    audited = executable.parent.parent.parent / "bosn.libs"
    directories = [directory for directory in (packaged, audited) if directory.is_dir()]
    if not directories:
        return
    if os.name == "nt":
        for directory in directories:
            _DLL_DIRECTORIES.append(os.add_dll_directory(os.fspath(directory)))
        return
    if sys.platform == "darwin":
        variable = "DYLD_FALLBACK_LIBRARY_PATH"
    else:
        variable = "LD_LIBRARY_PATH"
    previous = os.environ.get(variable)
    paths = [os.fspath(directory) for directory in directories]
    if previous:
        paths.append(previous)
    os.environ[variable] = os.pathsep.join(paths)


def main() -> None:
    try:
        executable = native_executable()
    except RuntimeError as error:
        raise SystemExit(f"bosn: {error}") from error
    _configure_native_library_path(executable)
    os.execv(executable, [os.fspath(executable), *sys.argv[1:]])
