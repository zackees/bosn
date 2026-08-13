"""Phase 2: the sqlite registry — WAL mode, round-trips, leases, generations.

No Docker needed; these are pure unit tests and run on every platform.
"""

from __future__ import annotations

from pathlib import Path

import pytest

from bosn.clock import FakeClock
from bosn.registry import Registry


@pytest.fixture
def clock() -> FakeClock:
    return FakeClock()


@pytest.fixture
def registry(tmp_path: Path, clock: FakeClock):
    with Registry(tmp_path / "registry.sqlite3", clock=clock) as reg:
        yield reg


def test_database_is_wal_mode(registry: Registry) -> None:
    assert registry.journal_mode == "wal"


def test_registry_id_is_a_stable_uuid(tmp_path: Path) -> None:
    path = tmp_path / "r.sqlite3"
    with Registry(path) as first:
        original = first.registry_id
    with Registry(path) as second:
        assert second.registry_id == original
    assert len(original) == 36


def test_registry_ids_differ_across_databases(tmp_path: Path) -> None:
    with Registry(tmp_path / "a.sqlite3") as a, Registry(tmp_path / "b.sqlite3") as b:
        assert a.registry_id != b.registry_id


def test_resource_round_trip(registry: Registry) -> None:
    resource = registry.register_resource(
        kind="volume",
        name="bosn-target-abc",
        stack="test",
        generation="sha256:abc",
        scope="spec",
        workspace="/w/one",
    )
    fetched = registry.get_resource(resource.id)
    assert fetched == resource
    assert fetched is not None
    assert fetched.state == "active"
    assert [r.id for r in registry.list_resources()] == [resource.id]


def test_touch_resource_advances_last_used(registry: Registry, clock: FakeClock) -> None:
    resource = registry.register_resource(
        kind="container",
        name="c1",
        stack="test",
        generation="g",
        scope="stack",
        workspace="/w",
    )
    assert resource.last_used == resource.created_at
    clock.advance(3600)
    registry.touch_resource(resource.id)
    updated = registry.get_resource(resource.id)
    assert updated is not None
    assert updated.last_used == resource.created_at + 3600


def test_lease_expiry_is_driven_by_the_injected_clock(registry: Registry, clock: FakeClock) -> None:
    resource = registry.register_resource(
        kind="volume", name="v", stack="s", generation="g", scope="spec", workspace="/w"
    )
    lease = registry.acquire_lease(resource.id, pid=4242, proc_start=1.0, ttl_seconds=900)
    assert not lease.expired_by_time(clock.now())

    clock.advance(899)
    assert not lease.expired_by_time(clock.now())

    clock.advance(2)
    assert lease.expired_by_time(clock.now())


def test_heartbeat_defers_expiry(registry: Registry, clock: FakeClock) -> None:
    resource = registry.register_resource(
        kind="volume", name="v", stack="s", generation="g", scope="spec", workspace="/w"
    )
    lease = registry.acquire_lease(resource.id, pid=1, proc_start=1.0, ttl_seconds=100)
    clock.advance(90)
    registry.heartbeat(lease.id)
    clock.advance(90)
    refreshed = registry.get_lease(lease.id)
    assert refreshed is not None
    assert not refreshed.expired_by_time(clock.now())


def test_removing_a_resource_cascades_to_its_leases(registry: Registry) -> None:
    resource = registry.register_resource(
        kind="volume", name="v", stack="s", generation="g", scope="spec", workspace="/w"
    )
    registry.acquire_lease(resource.id, pid=1, proc_start=1.0)
    assert registry.leases_for(resource.id)
    registry.remove_resource(resource.id)
    assert registry.leases_for(resource.id) == []


def test_generations_supersede_all_but_the_current_digest(
    registry: Registry, clock: FakeClock
) -> None:
    registry.record_generation("sha256:old", "test")
    clock.advance(60)
    registry.record_generation("sha256:new", "test")
    assert registry.supersede_generations("test", keep_digest="sha256:new") == 1
    assert registry.generation_superseded_at("sha256:old") == clock.now()
    assert registry.generation_superseded_at("sha256:new") is None


def test_events_are_recorded_for_lifecycle_actions(registry: Registry) -> None:
    resource = registry.register_resource(
        kind="volume", name="v", stack="s", generation="g", scope="spec", workspace="/w"
    )
    registry.acquire_lease(resource.id, pid=1, proc_start=1.0)
    kinds = [row["kind"] for row in registry.events()]
    assert "resource.registered" in kinds
    assert "lease.acquired" in kinds


def test_a_second_reader_sees_writes_and_is_not_blocked(tmp_path: Path) -> None:
    """WAL readers never block behind a writer -- that is why WAL was chosen."""
    path = tmp_path / "shared.sqlite3"
    with Registry(path) as writer:
        writer.register_resource(
            kind="volume", name="v", stack="s", generation="g", scope="spec", workspace="/w"
        )
        # open a reader while the writer connection is still live
        with Registry(path, read_only=True) as reader:
            assert len(reader.list_resources()) == 1
            writer.register_resource(
                kind="volume", name="v2", stack="s", generation="g", scope="spec", workspace="/w"
            )
            assert len(reader.list_resources()) == 2
