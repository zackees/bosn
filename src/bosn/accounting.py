"""Byte accounting and backing-store pressure probes.

The Docker CLI exposes its verbose inventory as JSON but renders sizes with SI suffixes.
Those values are parsed once per pass; unknown formats remain unknown rather than zero.
"""

from __future__ import annotations

import json
import os
import re
import shutil
from dataclasses import dataclass
from pathlib import Path

from bosn.engine import Engine
from bosn.registry import Resource

_INVENTORY_COMMAND = ["system", "df", "-v", "--format", "{{json .}}"]
_SIZE = re.compile(r"^([0-9]+(?:\.[0-9]+)?)\s*([kmgtpe]?b)$", re.I)


@dataclass(frozen=True)
class StorageProbe:
    free_bytes: int
    total_bytes: int
    vhdx_slack_bytes: int | None = None


def _bytes(value: object) -> int | None:
    """Parse Docker's JSON size field; absent/unrecognised remains explicitly unknown."""
    if isinstance(value, int):
        return max(0, value)
    match = _SIZE.match(str(value).strip())
    if not match:
        return None
    units = {"b": 0, "kb": 1, "mb": 2, "gb": 3, "tb": 4, "pb": 5, "eb": 6}
    return int(float(match.group(1)) * 1000 ** units[match.group(2).lower()])


@dataclass(frozen=True)
class StorageInventory:
    """One `system df -v` snapshot, avoiding per-resource commands and layer double counting."""

    sizes: dict[tuple[str, str], int]

    @classmethod
    def collect(cls, engine: Engine) -> StorageInventory:
        try:
            result = engine.run(_INVENTORY_COMMAND)
        except KeyboardInterrupt:
            raise
        except Exception:  # noqa: BLE001 - status must still expose unknown measurements offline
            return cls({})
        if not result.ok:
            return cls({})
        sizes: dict[tuple[str, str], int] = {}
        try:
            rows = [json.loads(line) for line in result.stdout.splitlines() if line.strip()]
        except json.JSONDecodeError:
            return cls({})
        for row in rows:
            if not isinstance(row, dict):
                continue
            for kind, key, field in (
                ("volume", "Volumes", "Size"),
                ("container", "Containers", "Size"),
            ):
                for item in row.get(key, []):
                    if isinstance(item, dict) and (size := _bytes(item.get(field))) is not None:
                        sizes[(kind, str(item.get("Name") or item.get("ID") or ""))] = size
            # UniqueSize is the only image attribution that cannot charge a shared layer twice.
            for item in row.get("Images", []):
                if isinstance(item, dict) and (size := _bytes(item.get("UniqueSize"))) is not None:
                    sizes[("image", str(item.get("ID", "")))] = size
        return cls(sizes)


def resource_bytes(
    engine: Engine, resource: Resource, inventory: StorageInventory | None = None
) -> int | None:
    """Return a resource's managed bytes, or None when the engine cannot attribute it."""
    inventory = inventory or StorageInventory.collect(engine)
    return inventory.sizes.get((resource.kind, resource.name))


def engine_storage_path(engine: Engine, fallback: Path) -> Path:
    """Find the host path which actually holds engine data, never assuming registry storage."""
    try:
        root = engine.run(["info", "--format", "{{.DockerRootDir}}"]).stdout.strip()
    except KeyboardInterrupt:
        raise
    except Exception:  # noqa: BLE001 - an unavailable engine leaves the value explicitly fallback
        root = ""
    candidate = Path(root)
    if root and candidate.exists():
        return candidate
    if os.name == "nt":
        settings = Path(os.environ.get("APPDATA", "")) / "Docker" / "settings-store.json"
        try:
            raw = json.loads(settings.read_text(encoding="utf-8"))
            for key in ("CustomWslDistroDir", "DataFolder"):
                value = raw.get(key)
                if value and (vhdx := desktop_vhdx(Path(str(value)))) is not None:
                    return vhdx
        except (OSError, json.JSONDecodeError):
            pass
    return fallback


def desktop_vhdx(directory: Path) -> Path | None:
    """Return Docker Desktop's allocated data disk below a configured host directory."""
    if not directory.is_dir():
        return None
    candidates = [path for path in directory.rglob("*.vhdx") if path.is_file()]
    if not candidates:
        return None
    # Docker's data disk is normally docker_data.vhdx; prefer it, otherwise use the largest
    # virtual disk rather than accidentally accounting a small ancillary disk.
    return max(
        candidates, key=lambda path: (path.name.lower() == "docker_data.vhdx", path.stat().st_size)
    )


def probe(engine: Engine, fallback: Path) -> StorageProbe:
    """Probe engine storage (or a documented fallback when the engine cannot reveal it)."""
    path = engine_storage_path(engine, fallback)
    usage = shutil.disk_usage(path)
    # Docker Desktop does not expose a reliable guest-used / host-allocated pair through the
    # CLI.  In particular, host-volume capacity minus the VHDX's logical length is *not* VHDX
    # slack. Keep it unknown rather than emitting a dangerous false compaction recommendation.
    return StorageProbe(free_bytes=usage.free, total_bytes=usage.total, vhdx_slack_bytes=None)
