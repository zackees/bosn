"""Installable, reversible `docker` / `docker-compose` shims (#46).

A shim is a small launcher placed earlier on PATH than the real Docker engine that
routes to `bosn-docker` / `bosn-compose`, so scripts nobody has taught about bosn still
get governed containers and volumes instead of unmanaged ones. That is #46's "drop-in"
promise, and it is also the reason this module is careful: installing a `docker` shim
redirects *every* Docker invocation on the machine, not just bosn's own.

Where shims live
-----------------
Shims are written into a bosn-owned directory (`~/.bosn/shims` on POSIX,
`%LOCALAPPDATA%\\bosn\\shims` on Windows; override with `BOSN_SHIM_DIR`), never into an
existing PATH directory such as `/usr/local/bin` or `System32`. Writing into a directory
bosn already owns means install can never collide with an unrelated system binary, and
never needs elevation -- a write into `/usr/bin` or `System32` would require root/admin
on most machines, which the rest of bosn deliberately avoids. The tradeoff is that
activation is not automatic: **the user must prepend this directory to PATH themselves**
(shell profile / `setx PATH` / etc.) for the shims to actually intercept `docker`
invocations. `status()` reports the directory so `doctor`/the CLI can tell the user
exactly what to add.

What a shim file is
--------------------
Following `autostart.py`'s per-platform split: a POSIX shim is a `sh` wrapper (`docker`,
`docker-compose`, executable bit set) that `exec`s the bosn front door with the original
argv; a Windows shim is a `.cmd` (`docker.cmd`, `docker-compose.cmd`) that calls the
front door and propagates `%ERRORLEVEL%`. `docker` routes to `bosn-docker`;
`docker-compose` routes to `bosn-compose` (#46's new standalone compose front door).

How a bosn shim is identified
------------------------------
Every generated shim carries a `bosn-shim: <name>` marker line. Identification reads
that marker rather than trusting the filename alone -- a file named `docker` at the
shim path did not necessarily come from bosn (a prior manual install, a leftover from
some other tool). Only a file whose content carries the exact marker is treated as
bosn's own and is therefore safe to overwrite or delete. Anything else at that path --
including no-content-readable files -- is reported as a `conflict` and left completely
untouched by both install and uninstall: never overwritten, never deleted. This is what
makes uninstall exactly reversible, including the case where the user already had
something named `docker` at that location: since install refused to touch it, there is
nothing to restore.

Recursion
---------
This module never installs a shim into a directory where it would become the *only*
resolvable `docker` -- it never touches PATH itself, only writes files into its own
directory, so `bosn-docker`'s own PATH resolution in `docker_cli.py`
(`_resolve_real_engine` / `_is_this_program`) still has the rest of PATH to find the
real engine on. Whether that resolution should additionally *skip* the shim directory
when looking for "the real docker" is `docker_cli.py`'s call (out of scope here); this
module's own `status()` does exactly that when reporting `real_engine`, so `doctor` can
show the user a real path rather than pointing back at the shim it just installed.
"""

from __future__ import annotations

import os
import shutil
import sys
from dataclasses import dataclass
from pathlib import Path

#: Logical shim name -> the bosn front-door command it routes to.
_ROUTES: dict[str, str] = {
    "docker": "bosn-docker",
    "docker-compose": "bosn-compose",
}

_MARKER = "bosn-shim:"


def _is_windows() -> bool:
    return sys.platform.startswith("win")


def default_directory() -> Path:
    """Where shims are installed absent an explicit `directory=`.

    `BOSN_SHIM_DIR` overrides for tests and for users who want a non-default location,
    the same shape `bosn.config.default_path` uses for `BOSN_CONFIG`.
    """
    override = os.environ.get("BOSN_SHIM_DIR")
    if override:
        return Path(override)
    if _is_windows():
        base = Path(os.environ.get("LOCALAPPDATA", str(Path.home() / "AppData" / "Local")))
        return base / "bosn" / "shims"
    return Path.home() / ".bosn" / "shims"


def _filename(name: str) -> str:
    return f"{name}.cmd" if _is_windows() else name


def _shim_content(name: str, target: str) -> str:
    if _is_windows():
        return (
            "@echo off\r\n"
            f"rem {_MARKER} {name}\r\n"
            "rem Installed by bosn (see `bosn doctor`); reinstall to regenerate, do not\r\n"
            "rem hand-edit -- hand edits are indistinguishable from a foreign file and\r\n"
            "rem will be left alone (and reported as a conflict) by future installs.\r\n"
            f"rem Routes {name!s} to {target} so unmodified tooling gets bosn-governed\r\n"
            "rem resources instead of unmanaged ones.\r\n"
            f"{target} %*\r\n"
            "exit /b %ERRORLEVEL%\r\n"
        )
    return (
        "#!/bin/sh\n"
        f"# {_MARKER} {name}\n"
        "# Installed by bosn (see `bosn doctor`); reinstall to regenerate, do not\n"
        "# hand-edit -- hand edits are indistinguishable from a foreign file and will\n"
        "# be left alone (and reported as a conflict) by future installs.\n"
        f"# Routes {name} to {target} so unmodified tooling gets bosn-governed\n"
        "# resources instead of unmanaged ones.\n"
        f'exec {target} "$@"\n'
    )


def _is_bosn_shim(path: Path, name: str) -> bool:
    """Whether `path` is a shim bosn itself generated for `name`.

    Anything unreadable is treated as *not* ours -- an unreadable file cannot be
    confirmed safe to overwrite/delete, and the safe default is to leave it alone and
    report it as a conflict rather than guess.
    """
    try:
        content = path.read_text(encoding="utf-8")
    except OSError:
        return False
    except UnicodeDecodeError:
        return False
    return f"{_MARKER} {name}" in content


def _resolve_real_engine(directory: Path) -> Path | None:
    """Resolve `docker` off PATH with `directory` excluded from the search.

    Plain `shutil.which("docker")` would find bosn's own shim once its directory is on
    PATH ahead of the real engine -- exactly the case a shim exists to create. This
    reports the *real* engine for `doctor`, so filters the shim directory out of the
    search first, by resolved identity rather than string equality (PATH entries are
    frequently spelled inconsistently -- trailing separators, `.` segments, case on
    Windows).
    """
    raw_path = os.environ.get("PATH", "")
    try:
        excluded = directory.resolve()
    except OSError:
        excluded = directory
    kept = []
    for entry in raw_path.split(os.pathsep):
        if not entry:
            continue
        try:
            resolved = Path(entry).resolve()
        except OSError:
            resolved = Path(entry)
        if resolved == excluded:
            continue
        kept.append(entry)
    filtered_path = os.pathsep.join(kept)
    found = shutil.which("docker", path=filtered_path)
    if found is None:
        return None
    return Path(found).resolve()


def _detail(
    directory: Path,
    shimmed: tuple[str, ...],
    conflicts: tuple[str, ...],
    real_engine: Path | None,
) -> str:
    if shimmed:
        parts = [f"shims installed: {', '.join(shimmed)} (in {directory})"]
    else:
        parts = [f"no bosn shims installed (directory: {directory})"]
    if conflicts:
        parts.append(f"conflicts left untouched: {', '.join(conflicts)}")
    parts.append(f"real docker: {real_engine}" if real_engine else "real docker: not found on PATH")
    return "; ".join(parts)


@dataclass(frozen=True)
class ShimStatus:
    installed: bool
    directory: Path | None
    shimmed: tuple[str, ...]
    conflicts: tuple[str, ...]
    real_engine: Path | None
    detail: str


def status(*, directory: Path | None = None) -> ShimStatus:
    """Report shim state. Always safe: never raises, never mutates anything.

    Going into `doctor`, which must be trustworthy to run at any time -- including
    against a directory that does not exist yet, one that exists but is unreadable, or
    a PATH with no docker anywhere on it. Every failure mode short-circuits to an empty,
    honestly-labeled status rather than propagating.
    """
    target_dir = directory or default_directory()
    shimmed: list[str] = []
    conflicts: list[str] = []
    try:
        listable = target_dir.is_dir()
    except OSError:
        listable = False
    if listable:
        for name in _ROUTES:
            path = target_dir / _filename(name)
            try:
                exists = path.is_file()
            except OSError:
                exists = False
            if not exists:
                continue
            if _is_bosn_shim(path, name):
                shimmed.append(name)
            else:
                conflicts.append(name)
    real_engine = _resolve_real_engine(target_dir)
    shimmed_t = tuple(shimmed)
    conflicts_t = tuple(conflicts)
    return ShimStatus(
        installed=bool(shimmed_t),
        directory=target_dir,
        shimmed=shimmed_t,
        conflicts=conflicts_t,
        real_engine=real_engine,
        detail=_detail(target_dir, shimmed_t, conflicts_t, real_engine),
    )


def install(*, directory: Path | None = None) -> ShimStatus:
    """Install (or refresh) shims for every name in `_ROUTES`.

    Idempotent: a name already carrying bosn's own marker is rewritten (picks up
    content changes across a bosn upgrade) rather than skipped, but this never touches
    a file count or stacks anything -- there is exactly one file per name either way.
    A name occupied by a foreign, non-bosn file is left completely alone and reported
    back as a conflict; it is never overwritten.
    """
    target_dir = directory or default_directory()
    target_dir.mkdir(parents=True, exist_ok=True)
    for name, target in _ROUTES.items():
        path = target_dir / _filename(name)
        if path.exists() and not _is_bosn_shim(path, name):
            continue  # foreign file: leave it, status() will report it as a conflict
        path.write_text(_shim_content(name, target), encoding="utf-8")
        if not _is_windows():
            path.chmod(0o755)
    return status(directory=target_dir)


def uninstall(*, directory: Path | None = None) -> ShimStatus:
    """Remove every bosn-installed shim, restoring the prior state exactly.

    A no-op, not an error, when the directory does not exist, when a name was never
    shimmed, or when uninstall is called twice in a row. A foreign file occupying a
    shim's name is left in place -- install never overwrote it, so there is nothing
    for uninstall to undo there either.
    """
    target_dir = directory or default_directory()
    try:
        exists = target_dir.is_dir()
    except OSError:
        exists = False
    if exists:
        for name in _ROUTES:
            path = target_dir / _filename(name)
            if path.exists() and _is_bosn_shim(path, name):
                path.unlink()
    return status(directory=target_dir)
