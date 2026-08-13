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

from bosn.registry import Lease, Registry, Resource
from bosn.resources import (
    QUIET_PERIOD_SECONDS,
    lease_is_expired,
    process_alive,
)

HOUR = 3600.0
DAY = 24 * HOUR

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


def container_should_stop(resource: Resource, now: float) -> bool:
    return resource.kind == "container" and (now - resource.last_used) >= CONTAINER_IDLE_STOP


def _ttl_for(resource: Resource) -> float:
    if resource.kind == "container":
        return CONTAINER_REMOVE
    return VOLUME_WARM_TTL


def evaluate(
    registry: Registry,
    resource: Resource,
    *,
    now: float | None = None,
    superseded: bool = False,
    workspace_done: bool = False,
    pressure: Pressure | None = None,
    alive_probe=process_alive,
) -> Verdict:
    """Decide a single resource's fate. Never mutates anything."""
    now = registry.clock.now() if now is None else now
    pressure = pressure or Pressure()

    leases: list[Lease] = registry.leases_for(resource.id)
    if any(not lease_is_expired(lease, now, alive_probe=alive_probe) for lease in leases):
        # Leased resources are untouchable, full stop -- this outranks every other signal,
        # including an explicit `done`.
        return Verdict(resource, False, KEPT_LEASED)

    age = now - resource.last_used

    if resource.state == "adopted" and age < QUIET_PERIOD_SECONDS:
        return Verdict(resource, False, KEPT_QUIET_PERIOD)

    if superseded:
        if age >= SUPERSEDED_CAP:
            return Verdict(resource, True, COLLECT_SUPERSEDED)
        return Verdict(resource, False, KEPT_WARM)

    if workspace_done and resource.scope != "machine":
        return Verdict(resource, True, COLLECT_DONE)

    if resource.scope == "machine" and not pressure.under_pressure:
        # Machine-shared caches age only under pressure.
        return Verdict(resource, False, KEPT_MACHINE_SCOPE)

    if age >= _ttl_for(resource):
        return Verdict(resource, True, COLLECT_IDLE)

    return Verdict(resource, False, KEPT_WARM)


def plan(
    registry: Registry,
    *,
    now: float | None = None,
    done_workspaces: set[str] | None = None,
    pressure: Pressure | None = None,
    alive_probe=process_alive,
) -> list[Verdict]:
    """Evaluate every registered resource. Pure: the registry is not modified."""
    now = registry.clock.now() if now is None else now
    done_workspaces = done_workspaces or set()

    verdicts: list[Verdict] = []
    for resource in registry.list_resources():
        verdicts.append(
            evaluate(
                registry,
                resource,
                now=now,
                superseded=registry.generation_superseded_at(resource.generation) is not None,
                workspace_done=resource.workspace in done_workspaces,
                pressure=pressure,
                alive_probe=alive_probe,
            )
        )
    return verdicts


def collectable(verdicts: list[Verdict]) -> list[Verdict]:
    return [v for v in verdicts if v.collect]


def kept(verdicts: list[Verdict]) -> list[Verdict]:
    return [v for v in verdicts if not v.collect]
