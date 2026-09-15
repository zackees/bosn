"""Locate the installed `bosn` CLI binary for tests.

The wheel ships the native binary as the `bosn` command in the environment's
scripts directory (there is no Python launcher). Tests that drive the real CLI
resolve it here rather than searching an arbitrary PATH entry, so a source
checkout or another Bosn install cannot be selected by accident.
"""

from __future__ import annotations

import os
import shutil
import sysconfig
from pathlib import Path


def native_binary() -> Path:
    name = "bosn.exe" if os.name == "nt" else "bosn"
    scripts = sysconfig.get_path("scripts")
    if scripts:
        candidate = Path(scripts) / name
        if candidate.is_file():
            return candidate
    found = shutil.which(name)
    if found is not None:
        return Path(found)
    raise RuntimeError(
        "the installed Bosn CLI was not found; install a platform wheel matching "
        "this Python environment"
    )
