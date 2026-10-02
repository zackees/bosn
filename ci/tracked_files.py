"""The repository's source files, for the lints that scan them.

`git ls-files` when git can read the checkout. Inside the isolated test
container a git *worktree* cannot be read: its `.git` file points at a
gitdir outside the mount, so git refuses. The lints then walk the tree
instead, skipping what git never tracks here (build outputs, virtual
environments, tool caches).
"""

from __future__ import annotations

import fnmatch
import os
import subprocess
from pathlib import Path

# Directories that hold nothing tracked: build outputs, environments, caches.
UNTRACKED_DIRS = frozenset(
    {
        ".git",
        "target",
        ".venv",
        "node_modules",
        "__pycache__",
        ".cargo",
        ".pytest_cache",
        ".ruff_cache",
        ".mypy_cache",
        "dist",
        "build",
    }
)


def tracked_files(root: Path, pattern: str | None = None) -> list[Path]:
    """Files under `root` (optionally only those matching `pattern`)."""
    listed = _git_listing(root, pattern)
    return listed if listed is not None else _walk(root, pattern)


def _git_listing(root: Path, pattern: str | None) -> list[Path] | None:
    command = [
        "git",
        # The isolated test container mounts a checkout owned by another
        # user; a read-only listing names it safe for this one command.
        "-c",
        f"safe.directory={root}",
        "ls-files",
        "-z",
    ]
    if pattern:
        command += ["--", pattern]
    result = subprocess.run(command, cwd=root, capture_output=True)
    if result.returncode != 0:
        return None
    return [root / name for name in result.stdout.decode().split("\0") if name]


def _walk(root: Path, pattern: str | None) -> list[Path]:
    found = []
    for directory, subdirectories, files in os.walk(root):
        subdirectories[:] = sorted(d for d in subdirectories if d not in UNTRACKED_DIRS)
        for name in sorted(files):
            if name in UNTRACKED_DIRS:
                continue  # a worktree's `.git` file is not source
            if pattern is None or fnmatch.fnmatch(name, pattern):
                found.append(Path(directory) / name)
    return found
