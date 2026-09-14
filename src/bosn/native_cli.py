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


def _execute_native(executable: Path, arguments: list[str]) -> None:
    """Replace this launcher on Unix and synchronously delegate on Windows.

    Windows does not have a process-replacement ``execve`` primitive.  Its
    C-runtime ``exec`` emulation can let the Python console-script launcher
    finish before the child has written output or released the package-local
    executable.  That breaks normal shell behavior (not only CI): a caller
    can observe a successful, empty ``bosn --version`` and immediately fail
    to update or uninstall the wheel because ``bosn-native.exe`` is still
    open.  Keep Unix's zero-overhead replacement, but wait for the Windows
    child and faithfully return its exit status.
    """

    command = [os.fspath(executable), *arguments]
    if os.name == "nt":
        raise SystemExit(os.spawnv(os.P_WAIT, os.fspath(executable), command))
    os.execv(executable, command)


def main() -> None:
    try:
        executable = native_executable()
    except RuntimeError as error:
        raise SystemExit(f"bosn: {error}") from error
    _configure_native_library_path(executable)
    _execute_native(executable, sys.argv[1:])
