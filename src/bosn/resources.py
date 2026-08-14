"""Resource enumeration and adoption.

Ownership lives in the Docker labels, so enumeration is the recovery path for a lost
registry as well as the input to every lifecycle decision. Three buckets come out of a
scan, and the distinction is the whole safety property:

- **owned**    complete label set carrying our registry id -- eligible for collection
- **foreign**  complete label set carrying someone else's registry id -- counted, never touched
- **unlabeled** incomplete or absent labels -- invisible to every decision, never touched

Automatic deletion requires complete ownership proof, which is why there is no `gc --force`.
"""

from __future__ import annotations

import datetime as dt
import json
import os
import subprocess
import sys
from dataclasses import dataclass, field
from pathlib import Path

from bosn import labels
from bosn.clock import Clock, SystemClock
from bosn.engine import Engine
from bosn.registry import Registry

# Docker CLI list commands per resource kind, formatted as one JSON object per line.
_LIST_COMMANDS: dict[str, list[str]] = {
    "container": ["ps", "--all", "--format", "{{json .}}"],
    "volume": ["volume", "ls", "--format", "{{json .}}"],
    # `docker images` shortens `.ID` to 12 characters unless told otherwise.  Adoption,
    # convergence, execution leases, and GC must all use the same immutable full ID.
    "image": ["images", "--no-trunc", "--format", "{{json .}}"],
}

_INSPECT_COMMANDS: dict[str, list[str]] = {
    "container": ["inspect", "--format", "{{json .Config.Labels}}"],
    "volume": ["volume", "inspect", "--format", "{{json .Labels}}"],
    "image": ["image", "inspect", "--format", "{{json .Config.Labels}}"],
}


@dataclass(frozen=True)
class DiscoveredResource:
    kind: str
    name: str
    raw_labels: dict[str, str]

    def owned_by(self, registry_id: str) -> bool:
        return labels.is_owned_by(self.raw_labels, registry_id)

    @property
    def complete(self) -> bool:
        return labels.is_complete(self.raw_labels)

    @property
    def registry(self) -> str | None:
        return self.raw_labels.get(labels.REGISTRY)

    def parsed(self) -> labels.ResourceLabels:
        return labels.ResourceLabels.from_dict(self.raw_labels)


@dataclass
class ScanResult:
    owned: list[DiscoveredResource] = field(default_factory=list)
    foreign: list[DiscoveredResource] = field(default_factory=list)
    unlabeled: list[DiscoveredResource] = field(default_factory=list)

    @property
    def foreign_registries(self) -> set[str]:
        return {r.registry for r in self.foreign if r.registry}

    def counts(self) -> dict[str, int]:
        return {
            "owned": len(self.owned),
            "foreign": len(self.foreign),
            "unlabeled": len(self.unlabeled),
        }


def _parse_labels(blob: str) -> dict[str, str]:
    """Docker renders labels either as a JSON object or as a comma-joined k=v string."""
    blob = (blob or "").strip()
    if not blob or blob in {"null", "<no value>", "map[]"}:
        return {}
    if blob.startswith("{"):
        try:
            parsed = json.loads(blob)
        except json.JSONDecodeError:
            return {}
        if not isinstance(parsed, dict):
            return {}
        return {str(k): str(v) for k, v in parsed.items() if v is not None}
    result: dict[str, str] = {}
    for pair in blob.split(","):
        key, sep, value = pair.partition("=")
        if sep:
            result[key.strip()] = value.strip()
    return result


def _name_of(kind: str, row: dict[str, object]) -> str:
    if kind == "volume":
        return str(row.get("Name", ""))
    if kind == "image":
        return str(row.get("ID", ""))
    return str(row.get("Names") or row.get("Name") or row.get("ID", ""))


class ResourceScanner:
    """Enumerates engine resources and sorts them by ownership proof."""

    def __init__(self, engine: Engine | None = None) -> None:
        self.engine = engine or Engine()

    def discover(self, kind: str) -> list[DiscoveredResource]:
        args = _LIST_COMMANDS.get(kind)
        if args is None:
            raise ValueError(f"cannot enumerate unknown kind {kind!r}")
        result = self.engine.run(args)
        if not result.ok:
            return []

        discovered: list[DiscoveredResource] = []
        for line in result.stdout.splitlines():
            line = line.strip()
            if not line:
                continue
            try:
                row = json.loads(line)
            except json.JSONDecodeError:
                continue
            if not isinstance(row, dict):
                continue
            name = _name_of(kind, row)
            if not name:
                continue
            raw = _parse_labels(str(row.get("Labels", "")))
            if not labels.is_complete(raw):
                # The list format truncates labels for some kinds; confirm via inspect
                # before concluding a resource is unlabeled.
                raw = self.inspect_labels(kind, name) or raw
            discovered.append(DiscoveredResource(kind=kind, name=name, raw_labels=raw))
        return discovered

    def inspect_labels(self, kind: str, name: str) -> dict[str, str]:
        args = _INSPECT_COMMANDS.get(kind)
        if args is None:
            return {}
        result = self.engine.run([*args, name])
        if not result.ok:
            return {}
        return _parse_labels(result.stdout)

    def scan(self, registry_id: str, kinds: list[str] | None = None) -> ScanResult:
        scan = ScanResult()
        for kind in kinds or list(_LIST_COMMANDS):
            for resource in self.discover(kind):
                if not resource.complete:
                    scan.unlabeled.append(resource)
                elif resource.owned_by(registry_id):
                    scan.owned.append(resource)
                else:
                    scan.foreign.append(resource)
        return scan


# -- leases ----------------------------------------------------------------


PROCESS_START_TOLERANCE_SECONDS = 2.0


def _parse_windows_process_start(ticks_text: str) -> float | None:
    """Parse .NET ``DateTime.Ticks`` (100ns units since 0001-01-01) into an epoch float."""
    try:
        return (int(ticks_text.strip()) / 10_000_000) - 62_135_596_800
    except ValueError:
        return None


def _parse_linux_process_start(
    stat_text: str, proc_stat_text: str, clock_ticks_per_sec: float
) -> float | None:
    """Parse ``/proc/<pid>/stat`` field 22 (starttime, in clock ticks since boot).

    ``stat_text`` is the raw content of ``/proc/<pid>/stat``; the comm field can itself
    contain spaces or parens, so the split happens after the last ``)``. ``proc_stat_text``
    is the raw content of ``/proc/stat``, which supplies the boot time (``btime``, seconds
    since epoch) that the tick count is relative to.
    """
    try:
        fields = stat_text.rsplit(")", 1)[1].split()
        ticks = int(fields[19])
        boot = next(
            int(line.split()[1])
            for line in proc_stat_text.splitlines()
            if line.startswith("btime ")
        )
        return boot + (ticks / clock_ticks_per_sec)
    except (IndexError, ValueError, StopIteration):
        return None


def _parse_darwin_process_start(lstart_text: str) -> float | None:
    """Parse ``ps -o lstart=`` output, e.g. ``Thu Aug 13 09:41:02 2026``."""
    try:
        return dt.datetime.strptime(lstart_text.strip(), "%a %b %d %H:%M:%S %Y").timestamp()
    except ValueError:
        return None


def process_start_time(pid: int) -> float | None:
    """Return an epoch creation time from an OS-owned process identity source.

    This is a thin per-OS dispatch: it gathers the raw platform data (a subprocess call on
    Windows/macOS, `/proc` reads on Linux) and hands it to a pure parsing helper so the
    parsing logic itself can be unit-tested on any OS with captured sample input.
    """
    if pid <= 0:
        return None
    if os.name == "nt":
        # WMIC is removed on current Windows; PowerShell's CIM provider is available on
        # every supported v1 host and returns the creation time without name guessing.
        # Note: this spawns a PowerShell process (~1s) per call. process_alive() only
        # invokes it once a lease's TTL has already elapsed, so it is not on the hot path
        # of a normal heartbeat -- but a maintenance sweep iterating many expired leases
        # pays this cost per lease. A cache is deliberately not added here: keying on pid
        # alone is unsafe across PID reuse in a long-lived daemon, and there is no cheap
        # cache key (pid, start) available before the very probe the cache would replace.
        result = subprocess.run(
            [
                "powershell",
                "-NoProfile",
                "-Command",
                f"(Get-Process -Id {pid}).StartTime.ToUniversalTime().Ticks",
            ],
            capture_output=True,
            text=True,
            check=False,
        )
        return _parse_windows_process_start(result.stdout)
    if sys.platform == "linux":
        try:
            stat_text = Path(f"/proc/{pid}/stat").read_text(encoding="utf-8")
            proc_stat_text = Path("/proc/stat").read_text(encoding="utf-8")
            clock_ticks_per_sec = os.sysconf("SC_CLK_TCK")
        except OSError:
            return None
        return _parse_linux_process_start(stat_text, proc_stat_text, clock_ticks_per_sec)
    if sys.platform == "darwin":
        result = subprocess.run(
            ["ps", "-p", str(pid), "-o", "lstart="], capture_output=True, text=True, check=False
        )
        return _parse_darwin_process_start(result.stdout)
    return None


def process_alive(pid: int, proc_start: float | None = None) -> bool:
    """Liveness probe for a lease holder.

    A lease expires only when its TTL elapses *and* this probe fails, so a killed client
    releases within one TTL while a live 40-minute build is never collected out from
    under itself.
    """
    if pid <= 0:
        return False
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    except OSError:
        return False
    if proc_start is None:
        return True
    actual = process_start_time(pid)
    # An unavailable identity probe fails conservatively: it may leak a lease but never
    # deletes resources belonging to a live process.
    return actual is None or abs(actual - proc_start) <= PROCESS_START_TOLERANCE_SECONDS


def lease_is_expired(
    lease,
    now: float,
    *,
    alive_probe=process_alive,
) -> bool:
    """True only when the TTL has elapsed AND the holder is gone."""
    if not lease.expired_by_time(now):
        return False
    return not alive_probe(lease.pid, lease.proc_start)


def resource_is_leased(registry: Registry, resource_id: str, *, alive_probe=process_alive) -> bool:
    now = registry.clock.now()
    return any(
        not lease_is_expired(lease, now, alive_probe=alive_probe)
        for lease in registry.leases_for(resource_id)
    )


def prune_dead_leases(registry: Registry, *, alive_probe=process_alive) -> list[str]:
    """Delete every lease that is both expired by time and confirmed dead.

    Reuses ``lease_is_expired`` so pruning applies exactly the same liveness proof as the
    rest of the lease lifecycle -- a lease is never deleted on TTL alone. Idempotent: a
    pruned lease's row is gone, so a second pass finds nothing left to prune and returns an
    empty list without error.
    """
    now = registry.clock.now()
    pruned: list[str] = []
    for lease in registry.all_leases():
        if lease_is_expired(lease, now, alive_probe=alive_probe):
            registry.prune_lease(lease.id)
            pruned.append(lease.id)
    return pruned


# -- adoption --------------------------------------------------------------

QUIET_PERIOD_SECONDS = 24 * 3600


def adopt(
    registry: Registry,
    scan: ScanResult,
    *,
    clock: Clock | None = None,
) -> list[str]:
    """Rebuild registry rows from labels after the database is lost.

    Adoption time becomes last-use, and the caller applies a 24 h quiet period on top, so
    recovery is never followed by a mass age-out of caches that were merely un-tracked.
    """
    clock = clock or SystemClock()
    now = clock.now()
    adopted: list[str] = []
    known = {(r.kind, r.name) for r in registry.list_resources()}

    for resource in scan.owned:
        if (resource.kind, resource.name) in known:
            continue
        parsed = resource.parsed()
        registry.register_resource(
            kind=resource.kind,
            name=resource.name,
            stack=parsed.stack,
            generation=parsed.generation,
            scope=parsed.scope,
            workspace=parsed.workspace,
            created_at=now,
        )
        registered = next(
            r
            for r in registry.list_resources()
            if r.kind == resource.kind and r.name == resource.name
        )
        registry.set_resource_state(registered.id, "adopted")
        registry.log_event("resource.adopted", f"{resource.kind}:{resource.name}")
        adopted.append(resource.name)
    return adopted


def within_quiet_period(adopted_at: float, now: float) -> bool:
    """Adopted resources are protected from age-out for the quiet period."""
    return (now - adopted_at) < QUIET_PERIOD_SECONDS
