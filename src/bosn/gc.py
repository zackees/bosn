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

from dataclasses import dataclass, field

from bosn import labels
from bosn.engine import Engine
from bosn.registry import Registry
from bosn.resources import ResourceScanner
from bosn.retention import Pressure, Verdict, collectable, container_should_stop, plan

_REMOVE_COMMANDS: dict[str, list[str]] = {
    "container": ["rm", "--force"],
    "volume": ["volume", "rm", "--force"],
    "image": ["image", "rm", "--force"],
}


@dataclass
class GCResult:
    dry_run: bool
    removed: list[str] = field(default_factory=list)
    kept: list[str] = field(default_factory=list)
    skipped_unproven: list[str] = field(default_factory=list)
    errors: list[str] = field(default_factory=list)

    def summary(self) -> dict[str, int]:
        return {
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
        verdicts: list[Verdict] = plan(
            self.registry, done_workspaces=done_workspaces, pressure=pressure
        )
        result = GCResult(dry_run=dry_run)

        for verdict in verdicts:
            if not verdict.collect:
                result.kept.append(verdict.name)
                if container_should_stop(verdict.resource, self.registry.clock.now()):
                    stopped = self.engine.run(["container", "stop", verdict.name])
                    if stopped.ok:
                        self.registry.log_event("container.stopped_idle", verdict.name)
                    else:
                        self.registry.log_event(
                            "container.stop_error",
                            f"{verdict.name}: {stopped.stderr or stopped.stdout}",
                        )

        for verdict in collectable(verdicts):
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
    verdicts = plan(registry)
    scan = ResourceScanner(engine).scan(registry.registry_id)

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
