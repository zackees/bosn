"""Phase 6: tiered retention. Clock-injected, no sleeps, no Docker."""

from __future__ import annotations

from pathlib import Path

import pytest

from bosn import retention
from bosn.clock import FakeClock
from bosn.config import load as load_config
from bosn.registry import Registry
from bosn.retention import (
    COLLECT_DONE,
    COLLECT_IDLE,
    COLLECT_PRESSURE,
    COLLECT_SUPERSEDED,
    KEPT_LEASED,
    KEPT_MACHINE_SCOPE,
    KEPT_QUIET_PERIOD,
    KEPT_RUNNING,
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


def test_explicit_policy_snapshot_controls_retention(registry: Registry, clock: FakeClock) -> None:
    resource = make(registry, kind="container")
    clock.advance(2)
    config = load_config(flags={"container_idle_stop": 1, "container_remove": 1})
    assert retention.container_should_stop(resource, clock.now(), config=config)
    assert evaluate(registry, resource, config=config, alive_probe=DEAD).collect


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


def test_networks_share_the_container_removal_clock_not_the_volume_warm_ttl(
    registry: Registry, clock: FakeClock
) -> None:
    """A network is cheap to recreate, like a container -- not a warm cache asset."""
    network = make(registry, kind="network")

    # Past the volume's 72h warm TTL would still be warm, but well past the container's
    # 24h removal clock: a network should already be collectable here.
    clock.advance(1 * DAY + 1)
    verdict = evaluate(registry, network, alive_probe=DEAD)
    assert verdict.collect
    assert verdict.reason == COLLECT_IDLE


def test_a_fresh_network_is_kept_warm_before_its_removal_clock_elapses(
    registry: Registry, clock: FakeClock
) -> None:
    network = make(registry, kind="network")
    clock.advance(1 * HOUR)
    verdict = evaluate(registry, network, alive_probe=DEAD)
    assert not verdict.collect
    assert verdict.reason == KEPT_WARM


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


def test_pressure_evicts_workspace_caches_before_machine_caches(
    registry: Registry, clock: FakeClock
) -> None:
    workspace = make(registry, scope="spec")
    machine = registry.register_resource(
        kind="volume", name="machine", stack="s", generation="g", scope="machine", workspace="/w"
    )
    clock.advance(100 * DAY)
    verdict = evaluate(
        registry, workspace, pressure=Pressure(under_pressure=True), alive_probe=DEAD
    )
    assert verdict.collect and verdict.reason == COLLECT_PRESSURE
    planned = retention.collectable(
        retention.plan(registry, pressure=Pressure(under_pressure=True), alive_probe=DEAD)
    )
    assert [item.name for item in planned] == [workspace.name, machine.name]


def test_pressure_assessment_keeps_count_bytes_and_free_space_distinct() -> None:
    pressure = Pressure.assess(
        resource_count=5,
        managed_bytes=100,
        free_bytes=9,
        resource_ceiling=4,
        managed_bytes_ceiling=101,
        min_free_bytes=10,
    )
    assert pressure.under_pressure
    assert pressure.count_exceeded
    assert not pressure.bytes_exceeded
    assert pressure.free_space_exceeded


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


# -- run state (issue #90) --------------------------------------------------
#
# `compose up -d` releases its project lease the moment the command returns, while the
# containers it started keep running. From that point on, age/pressure/done alone cannot
# tell a container mid-execution apart from an idle one -- only the engine's own run state
# can. These pin the run-state floor: it protects a *running* container from every signal
# that would otherwise collect it, without making a *stopped* container of identical age
# immortal.


def test_a_running_container_survives_a_pressure_pass_that_would_otherwise_collect_it(
    registry: Registry, clock: FakeClock
) -> None:
    """RED before the fix: this collected under COLLECT_PRESSURE like any other resource."""
    resource = make(registry, kind="container")
    clock.advance(10 * DAY)

    verdict = evaluate(
        registry,
        resource,
        pressure=Pressure(under_pressure=True),
        alive_probe=DEAD,
        running_containers=frozenset({resource.name}),
    )

    assert not verdict.collect
    assert verdict.reason == KEPT_RUNNING


def test_a_running_container_is_never_collected_on_age_alone(
    registry: Registry, clock: FakeClock
) -> None:
    resource = make(registry, kind="container")
    clock.advance(2 * DAY)  # well past the 24h container removal clock

    verdict = evaluate(
        registry, resource, alive_probe=DEAD, running_containers=frozenset({resource.name})
    )

    assert not verdict.collect
    assert verdict.reason == KEPT_RUNNING


def test_a_stopped_container_of_identical_age_and_scope_is_still_collected(
    registry: Registry, clock: FakeClock
) -> None:
    """The anti-over-reach case: run-state protection must not become blanket immortality."""
    resource = make(registry, kind="container")
    clock.advance(2 * DAY)

    verdict = evaluate(
        registry,
        resource,
        pressure=Pressure(under_pressure=True),
        alive_probe=DEAD,
        running_containers=frozenset(),  # engine answered: nothing is running
    )

    assert verdict.collect
    assert verdict.reason == COLLECT_PRESSURE


def test_an_unavailable_engine_protects_every_container(
    registry: Registry, clock: FakeClock
) -> None:
    """`None` from the probe means the engine could not answer -- fail safe, not fail open."""
    resource = make(registry, kind="container")
    clock.advance(2 * DAY)

    verdict = evaluate(
        registry,
        resource,
        pressure=Pressure(under_pressure=True),
        alive_probe=DEAD,
        running_containers=None,
    )

    assert not verdict.collect
    assert verdict.reason == KEPT_RUNNING


def test_none_and_empty_set_are_not_interchangeable(registry: Registry, clock: FakeClock) -> None:
    """Pinning the distinction directly: same resource, same age, opposite outcome."""
    resource = make(registry, kind="container")
    clock.advance(2 * DAY)

    unknown = evaluate(registry, resource, alive_probe=DEAD, running_containers=None)
    answered_idle = evaluate(registry, resource, alive_probe=DEAD, running_containers=frozenset())

    assert not unknown.collect and unknown.reason == KEPT_RUNNING
    assert answered_idle.collect and answered_idle.reason == COLLECT_IDLE


def test_non_container_kinds_are_unaffected_by_run_state(
    registry: Registry, clock: FakeClock
) -> None:
    """`running_containers` only ever gates `kind == 'container'`."""
    volume = make(registry, kind="volume")
    clock.advance(100 * DAY)

    verdict = evaluate(
        registry,
        volume,
        alive_probe=DEAD,
        # Even a matching name (and even `None`, the fail-safe value for containers) must
        # not touch a volume's own tier.
        running_containers=None,
    )

    assert verdict.collect
    assert verdict.reason == COLLECT_IDLE


def test_a_running_container_is_not_collected_by_an_explicit_done(
    registry: Registry, clock: FakeClock
) -> None:
    """Executing work is not disposable, even on the strongest first-party signal there is.

    `workspace_done` is a first-party signal, but it is set on the *workspace*, not
    observed on the *container* -- it says nothing about whether the process inside the
    container has finished. Run state is fresher and more specific, so it wins.
    """
    resource = make(registry, kind="container")

    verdict = evaluate(
        registry,
        resource,
        workspace_done=True,
        alive_probe=DEAD,
        running_containers=frozenset({resource.name}),
    )

    assert not verdict.collect
    assert verdict.reason == KEPT_RUNNING


def test_a_running_container_is_not_collected_by_supersession(
    registry: Registry, clock: FakeClock
) -> None:
    resource = make(registry, kind="container")
    clock.advance(2 * DAY)  # past the superseded cap

    verdict = evaluate(
        registry,
        resource,
        superseded=True,
        alive_probe=DEAD,
        running_containers=frozenset({resource.name}),
    )

    assert not verdict.collect
    assert verdict.reason == KEPT_RUNNING


def test_a_lease_outranks_running_in_the_reported_reason(
    registry: Registry, clock: FakeClock
) -> None:
    """Both protect the resource; the stronger, longer-standing signal names itself."""
    resource = make(registry, kind="container")
    registry.acquire_lease(resource.id, pid=1, proc_start=1.0, ttl_seconds=60)
    clock.advance(10 * DAY)

    verdict = evaluate(
        registry, resource, alive_probe=ALIVE, running_containers=frozenset({resource.name})
    )

    assert not verdict.collect
    assert verdict.reason == KEPT_LEASED
