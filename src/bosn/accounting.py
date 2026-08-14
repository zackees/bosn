"""Byte accounting and backing-store pressure probes.

Docker's human-facing size strings are deliberately not parsed: inspect emits byte counts,
which keeps the supervisor's decisions independent of locale and display rounding.
"""

from __future__ import annotations

import shutil
from dataclasses import dataclass
from pathlib import Path

from bosn.engine import Engine
from bosn.registry import Resource

_SIZE_COMMANDS = {
    "container": ["container", "inspect", "--size", "--format", "{{.SizeRw}}"],
    "image": ["image", "inspect", "--format", "{{.Size}}"],
    # Docker has no per-volume inspect size; `system df -v` is the supported source.
    "volume": ["system", "df", "-v", "--format", "{{json .}}"],
}


@dataclass(frozen=True)
class StorageProbe:
    free_bytes: int
    total_bytes: int
    vhdx_slack_bytes: int | None = None


def resource_bytes(engine: Engine, resource: Resource) -> int | None:
    """Return a resource's managed bytes, or None when the engine cannot attribute it."""
    args = _SIZE_COMMANDS.get(resource.kind)
    if args is None:
        return None
    result = engine.run([*args, resource.name])
    if not result.ok:
        return None
    if resource.kind != "volume":
        try:
            return max(0, int(result.stdout.strip()))
        except ValueError:
            return None
    # `docker system df -v` describes local volumes in a table.  Docker does not expose a
    # stable machine-readable per-volume size, so only accept an exact byte field when an
    # engine implementation provides one; otherwise report the attribution as unavailable.
    import json

    for line in result.stdout.splitlines():
        try:
            row = json.loads(line)
        except json.JSONDecodeError:
            continue
        if str(row.get("Name", "")) == resource.name:
            for key in ("SizeBytes", "Size"):
                try:
                    return max(0, int(row[key]))
                except (KeyError, TypeError, ValueError):
                    pass
    return None


def probe(path: Path) -> StorageProbe:
    """Probe the filesystem carrying bosn state; VHDX slack remains a distinct field."""
    usage = shutil.disk_usage(path)
    return StorageProbe(free_bytes=usage.free, total_bytes=usage.total)
