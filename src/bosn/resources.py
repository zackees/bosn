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

import json
from dataclasses import dataclass, field

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


def process_alive(pid: int, proc_start: float | None = None) -> bool:
    """Liveness probe for a lease holder.

    A lease expires only when its TTL elapses *and* this probe fails, so a killed client
    releases within one TTL while a live 40-minute build is never collected out from
    under itself.
    """
    if pid <= 0:
        return False
    try:
        import os

        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    except OSError:
        return False
    return True


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
