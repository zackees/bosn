"""Garbage collection and status.

Design commitments enforced here:

- **Never `docker system prune`.** Never prune the default builder. Resources are removed
  one at a time, by name, and only after re-confirming complete ownership proof from the
  engine's own labels -- the registry is a hint, the labels are the authority.
- **No `gc --force`.** Automatic deletion requires complete ownership proof, so there is no
  flag that skips it.
- **Every failure is observable.** A removal error lands in the event log and in the result
  counters. A discarded prune error is how you come to believe storage is bounded when it
  is not.
"""

from __future__ import annotations

from collections import defaultdict
from dataclasses import dataclass, field

from bosn import labels
from bosn.accounting import probe, resource_bytes
from bosn.engine import Engine
from bosn.registry import Registry
from bosn.resources import ResourceScanner
from bosn.retention import (
    KEPT_LEASED,
    KEPT_QUIET_PERIOD,
    Pressure,
    Verdict,
    collectable,
    container_should_stop,
    evaluate,
    plan,
)

_REMOVE_COMMANDS: dict[str, list[str]] = {
    "container": ["rm", "--force"],
    "volume": ["volume", "rm", "--force"],
    "image": ["image", "rm", "--force"],
}


@dataclass
class GCResult:
    dry_run: bool
    stopped: list[str] = field(default_factory=list)
    would_stop: list[str] = field(default_factory=list)
    removed: list[str] = field(default_factory=list)
    kept: list[str] = field(default_factory=list)
    skipped_unproven: list[str] = field(default_factory=list)
    errors: list[str] = field(default_factory=list)

    def summary(self) -> dict[str, int]:
        return {
            "stopped": len(self.stopped),
            "would_stop": len(self.would_stop),
            "removed": len(self.removed),
            "kept": len(self.kept),
            "skipped_unproven": len(self.skipped_unproven),
            "errors": len(self.errors),
        }


class Collector:
    def __init__(self, registry: Registry, engine: Engine | None = None) -> None:
        self.registry = registry
        self.engine = engine or Engine()
        self.scanner = ResourceScanner(self.engine)

    def _ownership_proven(self, kind: str, name: str) -> bool:
        """Re-confirm from the engine's labels before deleting anything.

        The registry can be stale or restored from a backup; the labels cannot lie about
        who created a resource. Automatic deletion requires this proof.
        """
        raw = self.scanner.inspect_labels(kind, name)
        return labels.is_owned_by(raw, self.registry.registry_id)

    def collect(
        self,
        *,
        dry_run: bool = True,
        done_workspaces: set[str] | None = None,
        pressure: Pressure | None = None,
    ) -> GCResult:
        done_workspaces = done_workspaces or set()
        verdicts: list[Verdict] = plan(
            self.registry, done_workspaces=done_workspaces, pressure=pressure
        )
        result = GCResult(dry_run=dry_run)

        for verdict in verdicts:
            if not verdict.collect:
                result.kept.append(verdict.name)
                if not container_should_stop(verdict.resource, self.registry.clock.now()):
                    continue
                # The planning snapshot is not a mutation boundary: another client may
                # acquire a lease while the rest of the plan is evaluated. Serialize all
                # registry writers, re-read the resource and its leases, re-confirm engine
                # ownership, and only then stop it while the guard remains held.
                with self.registry.lifecycle_guard():
                    resource = self.registry.get_resource(verdict.resource.id)
                    if resource is None:
                        continue
                    current = evaluate(
                        self.registry,
                        resource,
                        superseded=self.registry.generation_superseded_at(resource.generation)
                        is not None,
                        workspace_done=resource.workspace in done_workspaces,
                        pressure=pressure,
                    )
                    protected = current.reason in {KEPT_LEASED, KEPT_QUIET_PERIOD}
                    if (
                        current.collect
                        or protected
                        or not container_should_stop(resource, self.registry.clock.now())
                    ):
                        continue
                    if not self._ownership_proven(resource.kind, resource.name):
                        result.skipped_unproven.append(resource.name)
                        self.registry.log_event("gc.skipped_unproven", resource.name)
                        continue
                    if dry_run:
                        result.would_stop.append(resource.name)
                        continue
                    stopped = self.engine.run(["container", "stop", resource.name])
                    if stopped.ok:
                        result.stopped.append(resource.name)
                        self.registry.log_event("container.stopped_idle", resource.name)
                    else:
                        message = f"{resource.name}: {stopped.stderr or stopped.stdout}"
                        result.errors.append(message)
                        self.registry.log_event("container.stop_error", message)

        # Containers retain their volumes even after they have stopped. Remove them
        # first so a done workspace can be collected in one pass.
        for verdict in sorted(
            collectable(verdicts), key=lambda verdict: verdict.resource.kind != "container"
        ):
            resource = verdict.resource
            if not self._ownership_proven(resource.kind, resource.name):
                result.skipped_unproven.append(resource.name)
                self.registry.log_event("gc.skipped_unproven", resource.name)
                continue

            if dry_run:
                result.removed.append(resource.name)
                continue

            args = _REMOVE_COMMANDS.get(resource.kind)
            if args is None:
                result.errors.append(f"{resource.name}: unknown kind {resource.kind}")
                continue

            removal = self.engine.run([*args, resource.name])
            if removal.ok:
                self.registry.remove_resource(resource.id)
                self.registry.log_event("gc.removed", f"{resource.kind}:{resource.name}")
                result.removed.append(resource.name)
            else:
                message = f"{resource.name}: {removal.stderr or removal.stdout}"
                result.errors.append(message)
                self.registry.log_event("gc.error", message)

        return result


def status(registry: Registry, engine: Engine | None = None) -> dict:
    """Tiers, leases, and foreign registries. Read-only: works with the daemon dead."""
    engine = engine or Engine()
    from bosn.config import load as load_config

    config = load_config()
    scan = ResourceScanner(engine).scan(registry.registry_id)

    attributed: dict[tuple[str, str, str], dict[str, int]] = defaultdict(
        lambda: {"count": 0, "bytes": 0, "unmeasured": 0}
    )
    measured: dict[str, int | None] = {}
    for resource in registry.list_resources():
        size = resource_bytes(engine, resource)
        measured[resource.id] = size
        bucket = attributed[(resource.workspace, resource.stack, resource.kind)]
        bucket["count"] += 1
        if size is None:
            bucket["unmeasured"] += 1
        else:
            bucket["bytes"] += size
    managed_bytes = sum(size for size in measured.values() if size is not None)
    storage = probe(registry.path.parent)
    pressure = Pressure.assess(
        resource_count=len(measured), managed_bytes=managed_bytes, free_bytes=storage.free_bytes
    )
    verdicts = plan(registry, pressure=pressure)
    reclaimable = sum(measured[v.resource.id] or 0 for v in collectable(verdicts))
    advisory = (
        "Backing-store slack exceeds managed reclaimable bytes; compact the Docker VHDX manually."
        if storage.vhdx_slack_bytes is not None and storage.vhdx_slack_bytes > reclaimable
        else None
    )

    by_reason: dict[str, int] = {}
    for verdict in verdicts:
        by_reason[verdict.reason] = by_reason.get(verdict.reason, 0) + 1

    return {
        "registry_id": registry.registry_id,
        "config": config.report(),
        "registered": len(verdicts),
        "collectable": len(collectable(verdicts)),
        "by_reason": by_reason,
        "engine": scan.counts(),
        "foreign_registries": sorted(scan.foreign_registries),
        "foreign_registry_totals": {"count": len(scan.foreign), "bytes": None},
        "managed_bytes": managed_bytes,
        "managed_reclaimable_bytes": reclaimable,
        "pressure": {
            "under_pressure": pressure.under_pressure,
            "count_exceeded": pressure.count_exceeded,
            "bytes_exceeded": pressure.bytes_exceeded,
            "free_space_exceeded": pressure.free_space_exceeded,
        },
        "storage": {
            "free_bytes": storage.free_bytes,
            "total_bytes": storage.total_bytes,
            "vhdx_slack_bytes": storage.vhdx_slack_bytes,
            "compaction_advisory": advisory,
        },
        "attribution": [
            {"workspace": workspace, "stack": stack, "role": role, **values}
            for (workspace, stack, role), values in sorted(attributed.items())
        ],
        "decisions": [
            {
                "name": verdict.name,
                "kind": verdict.resource.kind,
                "eligible": verdict.collect,
                "reason": verdict.reason,
                "bytes": measured[verdict.resource.id],
            }
            for verdict in verdicts
        ],
    }


def mark_done(registry: Registry, workspace: str) -> int:
    """Mark a workspace finished so its non-machine caches become collectable.

    Dirty work is never destroyed on inference: this is the first-party signal, the
    strongest one there is. Derived signals are the caller's responsibility.
    """
    marked = 0
    for resource in registry.list_resources():
        if resource.workspace == workspace and resource.scope != "machine":
            registry.set_resource_state(resource.id, "done")
            marked += 1
    registry.log_event("workspace.done", f"{workspace} ({marked} resources)")
    return marked


def done_workspaces(registry: Registry) -> set[str]:
    return {r.workspace for r in registry.list_resources() if r.state == "done"}
