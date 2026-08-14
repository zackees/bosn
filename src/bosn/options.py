"""Fully typed CLI options.

`argparse.Namespace` is a bag of dynamically attached attributes: every read is
`Any`, a typo is an `AttributeError` at runtime rather than a type error, and whether a
field exists at all depends on which subparser ran. That gets worse as verbs accumulate
their own flags.

So argparse is confined to one function. It parses, and its Namespace is immediately
converted into a frozen dataclass with concrete types and real defaults. Everything
downstream takes `Options` and is fully checkable by pyright.
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass, field
from pathlib import Path


@dataclass(frozen=True)
class Options:
    """One fully resolved, fully typed value per CLI input."""

    verb: str | None = None

    # global
    engine: str = "docker"
    state_dir: Path | None = None
    manifest: Path | None = None

    # stack-facing verbs
    stack: str | None = None
    task: str | None = None
    args: tuple[str, ...] = ()

    # gc
    dry_run: bool = True

    # daemon
    port: int | None = None
    idle_retire_seconds: float | None = None
    max_builds: int | None = None
    build_ttl_seconds: float | None = None
    stop: bool = False
    autostart: bool | None = None

    extras: tuple[str, ...] = field(default=())

    @property
    def command(self) -> list[str]:
        """The ad-hoc command, with any argparse `--` separator stripped."""
        return [arg for arg in self.args if arg != "--"]

    def with_command(self, command: list[str]) -> Options:
        from dataclasses import replace

        return replace(self, args=tuple(command))


def _as_path(value: object) -> Path | None:
    if value is None or value == "":
        return None
    return Path(str(value))


def _as_str(value: object) -> str | None:
    return None if value is None else str(value)


def from_namespace(ns: argparse.Namespace) -> Options:
    """Collapse argparse's dynamic Namespace into a typed Options.

    This is the only place that reads attributes off a Namespace, and it uses getattr with
    explicit defaults because which attributes exist depends on the subparser that ran.
    """
    raw: dict[str, object] = dict(vars(ns))

    def get(name: str, default: object = None) -> object:
        return raw.get(name, default)

    def get_list(name: str) -> tuple[str, ...]:
        value = raw.get(name)
        if not isinstance(value, (list, tuple)):
            return ()
        return tuple(str(item) for item in value)

    # --state-dir and --manifest are accepted both globally and after some verbs; the
    # verb-local spelling wins when present.
    state_dir = get("daemon_state_dir") or get("state_dir")
    manifest = get("sub_manifest") or get("manifest")

    idle = get("idle_retire_seconds")
    port = get("port")
    max_builds = get("max_builds")
    build_ttl = get("build_ttl_seconds")

    return Options(
        verb=_as_str(get("verb")),
        engine=str(get("engine") or "docker"),
        state_dir=_as_path(state_dir),
        manifest=_as_path(manifest),
        stack=_as_str(get("stack")),
        task=_as_str(get("task")),
        args=get_list("args"),
        dry_run=bool(get("dry_run", True)),
        port=None if port is None else int(str(port)),
        idle_retire_seconds=None if idle is None else float(str(idle)),
        max_builds=None if max_builds is None else int(str(max_builds)),
        build_ttl_seconds=None if build_ttl is None else float(str(build_ttl)),
        stop=bool(get("stop", False)),
        autostart=get("autostart"),
    )
