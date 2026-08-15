"""Tiered retention.

Retention is tiered because containers are not volumes. A container is disposable --
seconds to recreate from a warm image. A cache volume is the asset that turns a 20-minute
cold build into 30 seconds. They get separate clocks:

- containers idle-stop at 1 h and are removed at 24 h
- networks share the container's 24 h removal clock -- disposable, not a cache
- warm volumes live 72 h
- superseded generations are capped at 24 h
- machine-shared caches age only under pressure
- a container the engine reports as running is exempt from all of the above -- the tiers
  assume "disposable, cheap to recreate," which is false the instant something is executing
  inside it

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
KEPT_RUNNING = "running"
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
    if resource.kind in ("container", "network"):
        # A network is cheap to recreate -- `docker compose up` (or a plain `docker
        # network create`) rebuilds it in milliseconds with no data to lose. It is
        # disposable infrastructure like a container, not a warm cache asset like a
        # volume, so it shares the container's short removal clock rather than the
        # volume's 72 h warm TTL.
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
    running_containers: frozenset[str] | None = frozenset(),
) -> Verdict:
    """Decide a single resource's fate. Never mutates anything.

    ``running_containers`` is the per-pass snapshot from
    ``resources.running_container_names``: the set of container names the engine reports as
    currently running, or ``None`` when the engine could not answer. The default of an empty
    frozenset means "no run-state information was supplied" -- callers that never touch
    containers, and the large majority of existing tests, get exactly today's behavior. A
    caller that *does* thread engine state through must pass ``None`` explicitly to get the
    fail-safe "protect everything" behavior; an empty set and ``None`` are deliberately kept
    on separate branches below so a falsy check can never conflate "nothing is running" with
    "the engine could not say".
    """
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

    running = resource.kind == "container" and (
        True if running_containers is None else resource.name in running_containers
    )
    if running and not (workspace_done or superseded):
        # A running container is not the "cheap to recreate from a warm image" object the
        # age tiers were designed around, so run state outranks age and pressure. It does
        # *not* outrank an explicit `done` or a superseded generation, and the distinction
        # is the difference between "running" and "doing work": bosn's own persistent
        # container idles in a running state indefinitely, so treating running as absolute
        # meant `bosn done` reclaimed nothing at all for a workspace whose container was
        # merely up -- which is the normal case, and the whole point of the verb.
        #
        # `done` and supersession are first-party statements that this workspace or
        # generation is finished. Age and pressure are only guesses about idleness, and
        # those are exactly the guesses a running container should override.
        return Verdict(resource, False, KEPT_RUNNING)

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
    running_containers: frozenset[str] | None = frozenset(),
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
                running_containers=running_containers,
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
