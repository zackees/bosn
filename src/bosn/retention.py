"""Tiered retention.

Retention is tiered because containers are not volumes. A container is disposable --
seconds to recreate from a warm image. A cache volume is the asset that turns a 20-minute
cold build into 30 seconds. They get separate clocks:

- containers idle-stop at 1 h and are removed at 24 h
- warm volumes live 72 h
- superseded generations are capped at 24 h
- machine-shared caches age only under pressure

Nothing here deletes anything. This module decides; `gc` executes, and only against
resources carrying complete ownership proof.
"""

from __future__ import annotations

from dataclasses import dataclass

from bosn.config import Config
from bosn.registry import Lease, Registry, Resource
from bosn.resources import (
    QUIET_PERIOD_SECONDS,
    lease_is_expired,
    process_alive,
)

HOUR = 3600.0
DAY = 24 * HOUR
GIB = 1024**3
DEFAULT_RESOURCE_CEILING = 1_000
DEFAULT_MANAGED_BYTES_CEILING = 100 * GIB
DEFAULT_MIN_FREE_BYTES = 10 * GIB

CONTAINER_IDLE_STOP = 1 * HOUR
CONTAINER_REMOVE = 1 * DAY
VOLUME_WARM_TTL = 3 * DAY
SUPERSEDED_CAP = 1 * DAY

# Reasons a resource is kept. Ordered by how strongly they bind.
KEPT_LEASED = "leased"
KEPT_QUIET_PERIOD = "quiet-period"
KEPT_WARM = "warm"
KEPT_MACHINE_SCOPE = "machine-scope"

# Reasons a resource is collectable.
COLLECT_SUPERSEDED = "superseded"
COLLECT_IDLE = "idle"
COLLECT_DONE = "workspace-done"
COLLECT_PRESSURE = "pressure"


@dataclass(frozen=True)
class Verdict:
    resource: Resource
    collect: bool
    reason: str

    @property
    def name(self) -> str:
        return self.resource.name


@dataclass(frozen=True)
class Pressure:
    """Free-space pressure on the engine's backing store."""

    under_pressure: bool = False
    count_exceeded: bool = False
    bytes_exceeded: bool = False
    free_space_exceeded: bool = False

    @classmethod
    def assess(
        cls,
        *,
        resource_count: int,
        managed_bytes: int,
        free_bytes: int,
        resource_ceiling: int = DEFAULT_RESOURCE_CEILING,
        managed_bytes_ceiling: int = DEFAULT_MANAGED_BYTES_CEILING,
        min_free_bytes: int = DEFAULT_MIN_FREE_BYTES,
    ) -> Pressure:
        """Evaluate all three pressure signals without conflating bytes and free space."""
        count_exceeded = resource_count > resource_ceiling
        bytes_exceeded = managed_bytes > managed_bytes_ceiling
        free_space_exceeded = free_bytes < min_free_bytes
        return cls(
            under_pressure=count_exceeded or bytes_exceeded or free_space_exceeded,
            count_exceeded=count_exceeded,
            bytes_exceeded=bytes_exceeded,
            free_space_exceeded=free_space_exceeded,
        )


def container_should_stop(resource: Resource, now: float, *, config: Config | None = None) -> bool:
    from bosn.config import load

    config = config or load()
    return resource.kind == "container" and (now - resource.last_used) >= config.get(
        "container_idle_stop"
    )


def _ttl_for(resource: Resource, *, config: Config) -> float:
    if resource.kind == "container":
        return config.get("container_remove")
    return config.get("warm_volume_ttl")


def evaluate(
    registry: Registry,
    resource: Resource,
    *,
    now: float | None = None,
    superseded: bool = False,
    workspace_done: bool = False,
    pressure: Pressure | None = None,
    config: Config | None = None,
    alive_probe=process_alive,
) -> Verdict:
    """Decide a single resource's fate. Never mutates anything."""
    now = registry.clock.now() if now is None else now
    pressure = pressure or Pressure()
    if config is None:
        from bosn.config import load

        config = load()

    leases: list[Lease] = registry.leases_for(resource.id)
    if any(not lease_is_expired(lease, now, alive_probe=alive_probe) for lease in leases):
        # Leased resources are untouchable, full stop -- this outranks every other signal,
        # including an explicit `done`.
        return Verdict(resource, False, KEPT_LEASED)

    age = now - resource.last_used

    if resource.state == "adopted" and age < QUIET_PERIOD_SECONDS:
        return Verdict(resource, False, KEPT_QUIET_PERIOD)

    if superseded:
        if age >= config.get("superseded_cap"):
            return Verdict(resource, True, COLLECT_SUPERSEDED)
        return Verdict(resource, False, KEPT_WARM)

    if workspace_done and resource.scope != "machine":
        return Verdict(resource, True, COLLECT_DONE)

    # Pressure can evict warm resources, but never leases/adoption quiet-period resources.
    # Machine-wide caches are deliberately considered after all workspace-scoped caches.
    if pressure.under_pressure and resource.scope != "machine":
        return Verdict(resource, True, COLLECT_PRESSURE)

    if resource.scope == "machine" and not pressure.under_pressure:
        # Machine-shared caches age only under pressure.
        return Verdict(resource, False, KEPT_MACHINE_SCOPE)

    if age >= _ttl_for(resource, config=config):
        return Verdict(resource, True, COLLECT_IDLE)

    return Verdict(resource, False, KEPT_WARM)


def plan(
    registry: Registry,
    *,
    now: float | None = None,
    done_workspaces: set[str] | None = None,
    pressure: Pressure | None = None,
    config: Config | None = None,
    alive_probe=process_alive,
) -> list[Verdict]:
    """Evaluate every registered resource. Pure: the registry is not modified."""
    now = registry.clock.now() if now is None else now
    done_workspaces = done_workspaces or set()

    verdicts: list[Verdict] = []
    for resource in registry.list_resources():
        superseded, workspace_done = registry.resource_retention_signals(
            resource.id, done_workspaces=done_workspaces
        )
        verdicts.append(
            evaluate(
                registry,
                resource,
                now=now,
                superseded=superseded,
                workspace_done=workspace_done,
                pressure=pressure,
                config=config,
                alive_probe=alive_probe,
            )
        )
    return verdicts


def collectable(verdicts: list[Verdict]) -> list[Verdict]:
    # A single order is essential under pressure: clearly obsolete first, then completed
    # worktrees, then ordinary pressure candidates, with shared machine caches last.
    order = {COLLECT_SUPERSEDED: 0, COLLECT_DONE: 1, COLLECT_IDLE: 2, COLLECT_PRESSURE: 3}
    return sorted(
        (v for v in verdicts if v.collect),
        key=lambda v: (
            v.resource.scope == "machine",
            order.get(v.reason, 99),
            v.resource.last_used,
        ),
    )


def kept(verdicts: list[Verdict]) -> list[Verdict]:
    return [v for v in verdicts if not v.collect]
