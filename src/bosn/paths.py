"""Canonical path identities across the supported shells' path spellings.

Issue #1 names cmd.exe, PowerShell, MSYS Git Bash, macOS, and Linux as the supported
shells, so a native Windows path, a `/c/...` MSYS spelling, a `//?/` UNC-prefixed path,
and a plain POSIX path must all resolve to one identity. WSL itself is rejected outright
(see `in_wsl`) rather than supported, but its `/mnt/c/...` spelling is recognized anyway,
defensively, the same way Cygwin's `/cygdrive/c/...` spelling is: neither shell is
supported, but the cost of recognizing either prefix is one alternation, and the
alternative -- silently rooting the identity at a nonexistent path like
`c:\\cygdrive\\c\\...` -- is a workspace identity that never matches, which is exactly the
silent split this module exists to prevent.
"""

from __future__ import annotations

import ntpath
import os
import re
from pathlib import Path


def to_host_path(path: str | Path) -> Path:
    """A shell's spelling of a path, as this OS's filesystem wants it.

    `normalize_workspace_path` answers "are these the same workspace?" and normcases to
    get there, which makes its result an identity, not a usable path. This answers the
    other question -- "what do I open?" -- and preserves case, because a mount source or a
    manifest location is handed to the engine and to the filesystem.

    Only drive-rooted spellings are rewritten. A plain POSIX path is returned untouched,
    so this is a no-op on Linux and macOS rather than something that has to be guarded at
    every call site.
    """
    raw = str(path)
    forward = raw.replace("\\", "/")
    forward = re.sub(r"^//\?/", "", forward)
    match = re.match(r"^/(?:mnt/|cygdrive/)?([a-zA-Z])/(.*)$", forward)
    if match is None:
        return Path(raw)
    if os.name != "nt":
        # `/c/x` on a POSIX host is a real path that happens to look like an MSYS
        # spelling. Rewriting it to `C:/x` there would invent a path that cannot exist.
        return Path(raw)
    return Path(f"{match.group(1).upper()}:/{match.group(2)}")


def normalize_workspace_path(path: str | Path) -> str:
    raw = str(path).replace("\\", "/")
    raw = re.sub(r"^//\?/", "", raw)
    match = re.match(r"^/(?:mnt/|cygdrive/)?([a-zA-Z])/(.*)$", raw)
    if match:
        raw = f"{match.group(1).upper()}:/{match.group(2)}"
    if re.match(r"^[a-zA-Z]:/", raw) or raw.startswith("//"):
        # ntpath is deliberately used even on non-Windows hosts so persisted identities
        # remain stable when a registry is inspected from another supported shell.
        return ntpath.normcase(ntpath.normpath(raw.replace("/", "\\")))
    return os.path.normcase(str(Path(raw).resolve()))


def in_wsl() -> bool:
    if os.environ.get("WSL_DISTRO_NAME") or os.environ.get("WSL_INTEROP"):
        return True
    try:
        return "microsoft" in Path("/proc/sys/kernel/osrelease").read_text().lower()
    except OSError:
        return False
