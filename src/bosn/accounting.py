"""Byte accounting and backing-store pressure probes.

The Docker CLI exposes its verbose inventory as JSON but renders sizes with SI suffixes.
Those values are parsed once per pass; unknown formats remain unknown rather than zero.
"""

from __future__ import annotations

import json
import os
import re
import shutil
from collections.abc import Iterable
from dataclasses import dataclass
from pathlib import Path
from typing import Protocol

from bosn.engine import Engine
from bosn.registry import Resource

_INVENTORY_COMMAND = ["system", "df", "-v", "--format", "{{json .}}"]
_SIZE = re.compile(r"^([0-9]+(?:\.[0-9]+)?)\s*([kmgtpe]?b)$", re.I)

# `StorageInventory.collect` deliberately does not pass a bespoke `timeout=` to
# `engine.run` here, unlike `compose-adopt` (#99), which needed one because its 45s figure
# was measured against a documented failure: the batched scan reliably landed past the
# shared 10s *IPC client* budget, so a shorter deadline was silently returning stale
# bookkeeping to the caller while `compose build` still reported success. That specific
# failure mode does not apply to this call in the same way: `engine.run` already has its
# own 60s default (`Engine.DEFAULT_TIMEOUT`), and after this issue's fix (#106) a command
# that runs past *that* deadline lands in `collect`'s `except Exception` branch below,
# which now returns `measured=False` and is logged by `gc.py` as `gc.inventory_unmeasured`
# rather than silently reading as "zero bytes, no pressure". The failure changed from
# silent-fail-open to loud-abstain, which was the actual problem -- a slow `system df -v`
# no longer needs a bigger deadline to be safe, it needs its slowness to stop lying.
#
# A real widening of `ipc.DEFAULT_TIMEOUT` for the `gc` verb specifically (the same
# category of problem #99 fixed for `compose-adopt`: the CLI's `bosn gc` waits only the
# shared 10s IPC budget while the daemon-side `Collector.collect` can legitimately run
# past it, including this call) is a separate, pre-existing gap that lives in
# `daemon.request`/`cli.py`'s `cmd_gc`, not in this module, and is out of scope here.


@dataclass(frozen=True)
class StorageProbe:
    free_bytes: int
    total_bytes: int
    vhdx_slack_bytes: int | None = None


@dataclass(frozen=True)
class VhdxAllocation:
    """Configured Docker Desktop data-disk allocation, never a reclaimability claim."""

    path: Path
    allocated_bytes: int


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
    """One `system df -v` snapshot, avoiding per-resource commands and layer double counting.

    ``measured`` is the whole point of this dataclass existing separately from a bare dict
    (issue #106). Before it existed, `collect` returned `cls({})` on every failure path --
    engine unreachable, non-zero exit, unparseable JSON -- and an *empty-because-nothing-to-
    measure* host produced exactly the same `sizes == {}` as an *empty-because-the-command-
    failed* host. Those two situations must never be conflated: an empty dict fed
    `Pressure.assess` as `managed_bytes=0`, which reads as "measured, and it is zero" --
    "no byte pressure" -- when what actually happened is "unmeasured, so silence about
    pressure, not a clean bill of health". The condition GC exists to relieve (a host with
    hundreds of Docker objects) is also the condition that makes `system df -v` slowest and
    likeliest to fail or time out, so this is a directional failure: exactly the hosts that
    most need reclamation are the ones most likely to look, incorrectly, like they don't.

    `measured=True` with `sizes=={}` is a legitimate, common state (a fresh host with no
    Docker objects at all) and must stay indistinguishable from any other successful-but-
    small measurement -- it is not itself evidence of a problem.
    """

    sizes: dict[tuple[str, str], int]
    measured: bool = True

    @classmethod
    def collect(cls, engine: Engine) -> StorageInventory:
        try:
            result = engine.run(_INVENTORY_COMMAND)
        except KeyboardInterrupt:
            raise
        except Exception:  # noqa: BLE001 - status must still expose unknown measurements offline
            return cls({}, measured=False)
        if not result.ok:
            return cls({}, measured=False)
        sizes: dict[tuple[str, str], int] = {}
        try:
            rows = [json.loads(line) for line in result.stdout.splitlines() if line.strip()]
        except json.JSONDecodeError:
            return cls({}, measured=False)
        if not rows:
            # A healthy `system df -v --format {{json .}}` emits exactly one JSON object per
            # invocation -- verified live against a real, busy Docker host (a single line,
            # keys `Images`/`Containers`/`Volumes`/`BuildCache` all present and non-empty
            # there). That host had objects in every category, so "keys present as empty
            # arrays when a category is genuinely empty" is inferred from the fixed format
            # struct rather than observed directly; `test_a_genuinely_empty_host_is_still_
            # measured` below pins that inferred contract as the assumption this code relies
            # on. Either way, zero parseable lines on a `returncode == 0` exit is not that
            # shape -- it is indistinguishable from a client that silently produced no
            # output -- so treat it as unmeasured rather than trusting an empty result we
            # cannot explain.
            return cls({}, measured=False)
        for row in rows:
            if not isinstance(row, dict):
                continue
            for kind, key, field in (
                ("volume", "Volumes", "Size"),
                ("container", "Containers", "Size"),
            ):
                for item in row.get(key, []):
                    if isinstance(item, dict) and (size := _bytes(item.get(field))) is not None:
                        # `df -v` names containers with `Names` (plural) and volumes with
                        # `Name`. Reading only `Name` silently fell through to `ID` for every
                        # container, while `resources._name_of` keys them by `Names` -- so the
                        # two never met and every container read as unmeasured. That is not a
                        # cosmetic gap: `gc` refuses to declare byte pressure resolved while
                        # any managed resource is unmeasured, so one managed container was
                        # enough to wedge pressure resolution permanently. Mirrors
                        # `resources._name_of`; keep the two in step.
                        name = item.get("Names") or item.get("Name") or item.get("ID") or ""
                        sizes[(kind, str(name))] = size
            # UniqueSize is the only image attribution that cannot charge a shared layer twice.
            for item in row.get("Images", []):
                if isinstance(item, dict) and (size := _bytes(item.get("UniqueSize"))) is not None:
                    sizes[("image", str(item.get("ID", "")))] = size
        return cls(sizes)


def resource_bytes(
    engine: Engine, resource: Resource, inventory: StorageInventory | None = None
) -> int | None:
    """Return a resource's managed bytes, or None when the engine cannot attribute it.

    `docker system df -v` has no row shape for networks -- they hold no data, so there is
    nothing to size. That absence must read as "known: zero bytes", not "unmeasured": `gc`
    refuses to declare byte pressure resolved while any managed resource is unmeasured, so
    treating every network as permanently unmeasured would wedge pressure resolution for
    good on any project with a Compose-created network.
    """
    if resource.kind == "network":
        return 0
    inventory = inventory or StorageInventory.collect(engine)
    return inventory.sizes.get((resource.kind, resource.name))


class SizedResource(Protocol):
    """The shape `bucket_totals` needs: anything the scanner reports with a kind and name."""

    @property
    def kind(self) -> str: ...

    @property
    def name(self) -> str: ...


def bucket_totals(items: Iterable[SizedResource], inventory: StorageInventory) -> dict[str, object]:
    """Size one ownership bucket from a scan, with a per-kind breakdown.

    `gc.status` reported byte totals for the `foreign` bucket but only a bare count for
    `unlabeled` (#147 G1), so a host could carry tens of gigabytes of artifacts bosn does
    not manage and say only how *many* there were. Both buckets go through here now, so
    neither can regain a size the other lacks.

    Networks are counted as a known zero rather than unmeasured, for the same reason
    `resource_bytes` does it: `docker system df -v` has no row shape for them because they
    hold no data. Treating that absence as "unknown" would make an honest report of a
    healthy host look permanently unmeasurable.
    """
    totals = {"count": 0, "bytes": 0, "unmeasured": 0}
    by_kind: dict[str, dict[str, int]] = {}
    for item in items:
        kind_bucket = by_kind.setdefault(item.kind, {"count": 0, "bytes": 0, "unmeasured": 0})
        totals["count"] += 1
        kind_bucket["count"] += 1
        size = 0 if item.kind == "network" else inventory.sizes.get((item.kind, item.name))
        if size is None:
            totals["unmeasured"] += 1
            kind_bucket["unmeasured"] += 1
            continue
        totals["bytes"] += size
        kind_bucket["bytes"] += size
    return {
        **totals,
        # Sorted so a report, a test, and a diff of two passes all read in the same order.
        "by_kind": dict(sorted(by_kind.items())),
        # A size the engine could not attribute makes `bytes` a floor, not a total. Callers
        # that print a number to a human need to know when to say "at least".
        "bytes_are_floor": totals["unmeasured"] > 0 or not inventory.measured,
    }


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


def configured_desktop_vhdx_allocation(
    settings_path: Path | None = None,
) -> VhdxAllocation | None:
    """Read configured Desktop allocation without consulting the unavailable engine."""
    settings_path = settings_path or (
        Path(os.environ.get("APPDATA", "")) / "Docker" / "settings-store.json"
    )
    try:
        raw = json.loads(settings_path.read_text(encoding="utf-8"))
        for key in ("CustomWslDistroDir", "DataFolder"):
            value = raw.get(key)
            if value and (vhdx := desktop_vhdx(Path(str(value)))) is not None:
                return VhdxAllocation(vhdx, vhdx.stat().st_size)
    except (OSError, json.JSONDecodeError):
        return None
    return None


def probe(engine: Engine, fallback: Path) -> StorageProbe:
    """Probe engine storage (or a documented fallback when the engine cannot reveal it)."""
    path = engine_storage_path(engine, fallback)
    usage = shutil.disk_usage(path)
    # Docker Desktop does not expose a reliable guest-used / host-allocated pair through the
    # CLI.  In particular, host-volume capacity minus the VHDX's logical length is *not* VHDX
    # slack. Keep it unknown rather than emitting a dangerous false compaction recommendation.
    return StorageProbe(free_bytes=usage.free, total_bytes=usage.total, vhdx_slack_bytes=None)
