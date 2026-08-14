"""Canonical path identities across native Windows, MSYS, WSL and UNC spellings."""

from __future__ import annotations

import ntpath
import os
import re
from pathlib import Path


def normalize_workspace_path(path: str | Path) -> str:
    raw = str(path).replace("\\", "/")
    raw = re.sub(r"^//\?/", "", raw)
    match = re.match(r"^/(?:mnt/)?([a-zA-Z])/(.*)$", raw)
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
