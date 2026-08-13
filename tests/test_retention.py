"""Phase 6: tiered retention. Clock-injected, no sleeps, no Docker."""

from __future__ import annotations

from pathlib import Path

import pytest

from bosn import retention
from bosn.clock import FakeClock
from bosn.registry import Registry
from bosn.retention import (
    COLLECT_DONE,
    COLLECT_IDLE,
    COLLECT_SUPERSEDED,
    KEPT_LEASED,
    KEPT_MACHINE_SCOPE,
    KEPT_QUIET_PERIOD,
    KEPT_WARM,
    Pressure,
    evaluate,
)

ALIVE = lambda pid, start=None: True  # noqa: E731
DEAD = lambda pid, start=None: False  # noqa: E731

HOUR = retention.HOUR
DAY = retention.DAY


@pytest.fixture
def clock() -> FakeClock:
    return FakeClock()


@pytest.fixture
def registry(tmp_path: Path, clock: FakeClock):
    with Registry(tmp_path / "r.sqlite3", clock=clock) as reg:
        yield reg


def make(registry: Registry, kind="volume", scope="spec", workspace="/w", generation="g"):
    return registry.register_resource(
        kind=kind,
        name=f"{kind}-1",
        stack="s",
        generation=generation,
        scope=scope,
        workspace=workspace,
    )


# -- leases outrank everything ---------------------------------------------


def test_a_leased_resource_is_untouchable(registry: Registry, clock: FakeClock) -> None:
    resource = make(registry)
    registry.acquire_lease(resource.id, pid=1, proc_start=1.0, ttl_seconds=60)
    clock.advance(10 * DAY)

    verdict = evaluate(registry, resource, alive_probe=ALIVE)
    assert not verdict.collect
    assert verdict.reason == KEPT_LEASED


def test_a_lease_outranks_an_explicit_done(registry: Registry, clock: FakeClock) -> None:
    resource = make(registry)
    registry.acquire_lease(resource.id, pid=1, proc_start=1.0, ttl_seconds=60)
    clock.advance(10 * DAY)

    verdict = evaluate(registry, resource, workspace_done=True, alive_probe=ALIVE)
    assert not verdict.collect, "a live lease must beat every other signal"


def test_a_dead_holders_lease_stops_protecting(registry: Registry, clock: FakeClock) -> None:
    resource = make(registry)
    registry.acquire_lease(resource.id, pid=1, proc_start=1.0, ttl_seconds=60)
    clock.advance(10 * DAY)

    verdict = evaluate(registry, resource, workspace_done=True, alive_probe=DEAD)
    assert verdict.collect


# -- container vs volume clocks --------------------------------------------


def test_containers_idle_stop_at_one_hour(registry: Registry, clock: FakeClock) -> None:
    resource = make(registry, kind="container")
    assert not retention.container_should_stop(resource, clock.now())
    clock.advance(HOUR + 1)
    assert retention.container_should_stop(resource, clock.now())


def test_containers_are_removed_at_one_day(registry: Registry, clock: FakeClock) -> None:
    resource = make(registry, kind="container")
    clock.advance(23 * HOUR)
    assert not evaluate(registry, resource, alive_probe=DEAD).collect
    clock.advance(2 * HOUR)
    assert evaluate(registry, resource, alive_probe=DEAD).reason == COLLECT_IDLE


def test_warm_volumes_survive_far_longer_than_containers(
    registry: Registry, clock: FakeClock
) -> None:
    """A cache volume is the asset; a container is disposable."""
    volume = make(registry, kind="volume")
    clock.advance(2 * DAY)
    assert evaluate(registry, volume, alive_probe=DEAD).reason == KEPT_WARM

    clock.advance(2 * DAY)  # now past 72 h
    assert evaluate(registry, volume, alive_probe=DEAD).reason == COLLECT_IDLE


# -- superseded generations ------------------------------------------------


def test_superseded_generations_are_capped_at_one_day(registry: Registry, clock: FakeClock) -> None:
    resource = make(registry)
    clock.advance(23 * HOUR)
    assert not evaluate(registry, resource, superseded=True, alive_probe=DEAD).collect

    clock.advance(2 * HOUR)
    verdict = evaluate(registry, resource, superseded=True, alive_probe=DEAD)
    assert verdict.collect and verdict.reason == COLLECT_SUPERSEDED


def test_a_superseded_generation_still_serving_a_lease_is_kept(
    registry: Registry, clock: FakeClock
) -> None:
    """The old generation keeps serving its live leases, then ages out."""
    resource = make(registry)
    registry.acquire_lease(resource.id, pid=1, proc_start=1.0, ttl_seconds=60)
    clock.advance(10 * DAY)
    assert not evaluate(registry, resource, superseded=True, alive_probe=ALIVE).collect


# -- machine scope ---------------------------------------------------------


def test_machine_scoped_caches_age_only_under_pressure(
    registry: Registry, clock: FakeClock
) -> None:
    resource = make(registry, scope="machine")
    clock.advance(100 * DAY)

    assert evaluate(registry, resource, alive_probe=DEAD).reason == KEPT_MACHINE_SCOPE
    under = evaluate(registry, resource, pressure=Pressure(under_pressure=True), alive_probe=DEAD)
    assert under.collect


def test_done_never_collects_machine_scoped_caches(registry: Registry, clock: FakeClock) -> None:
    """One workspace finishing must not evict the machine-wide cargo registry."""
    resource = make(registry, scope="machine")
    clock.advance(DAY)
    verdict = evaluate(registry, resource, workspace_done=True, alive_probe=DEAD)
    assert not verdict.collect


# -- done ------------------------------------------------------------------


def test_done_makes_this_workspaces_caches_collectable(
    registry: Registry, clock: FakeClock
) -> None:
    resource = make(registry, scope="spec")
    verdict = evaluate(registry, resource, workspace_done=True, alive_probe=DEAD)
    assert verdict.collect and verdict.reason == COLLECT_DONE


# -- adoption quiet period -------------------------------------------------


def test_adopted_resources_are_protected_for_the_quiet_period(
    registry: Registry, clock: FakeClock
) -> None:
    resource = make(registry)
    registry.set_resource_state(resource.id, "adopted")
    refreshed = registry.get_resource(resource.id)
    assert refreshed is not None

    clock.advance(23 * HOUR)
    assert evaluate(registry, refreshed, alive_probe=DEAD).reason == KEPT_QUIET_PERIOD

    # Past the quiet period the resource is not collected on the spot -- it simply rejoins
    # the normal tiers, and a warm volume still has its 72 h.
    clock.advance(2 * HOUR)
    assert evaluate(registry, refreshed, alive_probe=DEAD).reason == KEPT_WARM

    clock.advance(3 * DAY)
    assert evaluate(registry, refreshed, alive_probe=DEAD).collect


# -- planning --------------------------------------------------------------


def test_plan_is_pure(registry: Registry, clock: FakeClock) -> None:
    make(registry, kind="volume")
    clock.advance(10 * DAY)
    before = len(registry.list_resources())
    retention.plan(registry, alive_probe=DEAD)
    assert len(registry.list_resources()) == before, "planning must not mutate the registry"
