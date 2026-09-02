"""Phase 2: the sqlite registry — WAL mode, round-trips, leases, generations.

No Docker needed; these are pure unit tests and run on every platform.
"""

from __future__ import annotations

import sqlite3
from pathlib import Path

import pytest

from bosn.clock import FakeClock
from bosn.registry import SCHEMA_VERSION, Registry, RegistryError


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


def test_integrity_check_is_read_only(tmp_path: Path) -> None:
    path = tmp_path / "registry.sqlite3"
    with Registry(path) as registry:
        registry.register_resource(
            kind="volume",
            name="cache",
            stack="dev",
            generation="digest",
            scope="spec",
            workspace="workspace",
        )
    before = path.read_bytes()
    with Registry(path, read_only=True) as registry:
        assert registry.integrity_check() == "ok"
    assert path.read_bytes() == before


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


def test_engine_identity_is_unique_and_registration_reconciles_in_place(
    registry: Registry, clock: FakeClock
) -> None:
    first = registry.register_resource(
        kind="volume",
        name="shared-cache",
        stack="one",
        generation="sha256:old",
        scope="stack",
        workspace="/w1",
    )
    clock.advance(60)

    second = registry.register_resource(
        kind="volume",
        name="shared-cache",
        stack="two",
        generation="sha256:new",
        scope="machine",
        workspace="/w2",
    )

    assert second.id == first.id
    assert second.last_used == clock.now()
    assert (second.stack, second.generation, second.scope, second.workspace) == (
        "two",
        "sha256:new",
        "machine",
        "/w2",
    )
    assert [(resource.kind, resource.name) for resource in registry.list_resources()] == [
        ("volume", "shared-cache")
    ]


def test_legacy_image_tag_is_merged_into_its_immutable_identity(registry: Registry) -> None:
    legacy = registry.register_resource(
        kind="image",
        name="bosn/test:tag",
        stack="test",
        generation="sha256:g",
        scope="spec",
        workspace="/w1",
    )
    canonical = registry.register_resource(
        kind="image",
        name="sha256:image-id",
        stack="test",
        generation="sha256:g",
        scope="spec",
        workspace="/w2",
    )
    legacy_lease = registry.acquire_lease(legacy.id, pid=1, proc_start=1.0)

    merged = registry.canonicalize_image_identity("bosn/test:tag", "sha256:image-id")

    assert merged is not None
    assert merged.id == canonical.id
    assert registry.get_resource(legacy.id) is None
    moved_lease = registry.get_lease(legacy_lease.id)
    assert moved_lease is not None
    assert moved_lease.resource_id == canonical.id
    assert [(resource.kind, resource.name) for resource in registry.list_resources()] == [
        ("image", "sha256:image-id")
    ]
    uses = registry.conn.execute(
        "SELECT workspace FROM resource_uses WHERE resource_id = ? ORDER BY workspace",
        (canonical.id,),
    ).fetchall()
    assert [row["workspace"] for row in uses] == ["/w1", "/w2"]


def test_legacy_short_image_id_is_merged_into_the_full_identity(registry: Registry) -> None:
    full_id = "sha256:0123456789ababcdef0123456789abcdef0123456789abcdef0123456789ab"
    legacy = registry.register_resource(
        kind="image",
        name="0123456789ab",
        stack="test",
        generation="sha256:g",
        scope="spec",
        workspace="/w",
    )

    merged = registry.canonicalize_image_identity("bosn/test:tag", full_id)

    assert merged is not None
    assert merged.id == legacy.id
    assert merged.name == full_id
    assert registry.get_resource_by_engine_identity("image", "0123456789ab") is None


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
    registry.record_generation("sha256:old", "test", "/w")
    clock.advance(60)
    registry.record_generation("sha256:new", "test", "/w")
    assert registry.supersede_generations("test", keep_digest="sha256:new", workspace="/w") == 1
    assert (
        registry.generation_superseded_at("sha256:old", stack="test", workspace="/w") == clock.now()
    )
    assert registry.generation_superseded_at("sha256:new", stack="test", workspace="/w") is None


def test_generation_supersession_is_scoped_to_one_workspace(
    registry: Registry, clock: FakeClock
) -> None:
    for workspace in ("/w1", "/w2"):
        registry.record_generation("sha256:shared", "test", workspace)
    registry.record_generation("sha256:w1-new", "test", "/w1")

    assert registry.supersede_generations("test", keep_digest="sha256:w1-new", workspace="/w1") == 1
    assert (
        registry.generation_superseded_at("sha256:shared", stack="test", workspace="/w1")
        == clock.now()
    )
    assert registry.generation_superseded_at("sha256:shared", stack="test", workspace="/w2") is None


def test_mixed_done_and_superseded_consumers_make_a_shared_resource_retirable(
    registry: Registry,
) -> None:
    for workspace in ("/w1", "/w2"):
        registry.record_generation("sha256:old", "test", workspace)
        registry.register_resource(
            kind="image",
            name="sha256:image",
            stack="test",
            generation="sha256:old",
            scope="spec",
            workspace=workspace,
        )
    registry.mark_workspace_done("/w1")
    registry.record_generation("sha256:new", "test", "/w2")
    registry.supersede_generations("test", keep_digest="sha256:new", workspace="/w2")
    image = registry.get_resource_by_engine_identity("image", "sha256:image")
    assert image is not None

    assert registry.resource_retention_signals(image.id) == (True, False)


def test_v1_migration_deduplicates_engine_objects_and_preserves_leases(tmp_path: Path) -> None:
    path = tmp_path / "v1.sqlite3"
    connection = sqlite3.connect(path)
    connection.executescript(
        """
        CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
        INSERT INTO meta VALUES ('schema_version', '1');
        INSERT INTO meta VALUES ('registry_id', 'registry-v1');
        CREATE TABLE resources (
            id TEXT PRIMARY KEY, kind TEXT NOT NULL, name TEXT NOT NULL,
            stack TEXT NOT NULL, generation TEXT NOT NULL, scope TEXT NOT NULL,
            workspace TEXT NOT NULL, created_at REAL NOT NULL, last_used REAL NOT NULL,
            state TEXT NOT NULL DEFAULT 'active'
        );
        CREATE TABLE leases (
            id TEXT PRIMARY KEY, resource_id TEXT NOT NULL REFERENCES resources(id)
                ON DELETE CASCADE,
            pid INTEGER NOT NULL, proc_start REAL NOT NULL, acquired_at REAL NOT NULL,
            heartbeat_at REAL NOT NULL, ttl_seconds REAL NOT NULL
        );
        CREATE TABLE generations (
            digest TEXT PRIMARY KEY, stack TEXT NOT NULL, created_at REAL NOT NULL,
            superseded_at REAL
        );
        CREATE TABLE events (
            id INTEGER PRIMARY KEY AUTOINCREMENT, at REAL NOT NULL,
            kind TEXT NOT NULL, detail TEXT NOT NULL DEFAULT ''
        );
        INSERT INTO resources VALUES
            ('old', 'volume', 'cache', 'test', 'sha256:g', 'stack', '/w', 1, 2, 'active'),
            ('new', 'volume', 'cache', 'test', 'sha256:g', 'stack', '/w', 3, 4, 'active');
        INSERT INTO leases VALUES ('lease-old', 'old', 1, 1, 1, 1, 900);
        INSERT INTO generations VALUES ('sha256:g', 'test', 1, NULL);
        """
    )
    connection.close()

    with Registry(path) as migrated:
        resources = migrated.list_resources()
        assert len(resources) == 1
        assert resources[0].id == "new"
        assert resources[0].created_at == 1
        assert migrated.leases_for("new")[0].id == "lease-old"
        assert migrated.meta("schema_version") == str(SCHEMA_VERSION)
        assert migrated.generation_superseded_at("sha256:g", stack="test", workspace="/w") is None
        index_columns = migrated.conn.execute(
            "PRAGMA index_info(idx_resources_engine_identity)"
        ).fetchall()
        assert [row["name"] for row in index_columns] == ["kind", "name"]


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


def test_read_only_registry_refuses_missing_database_without_creating_it(tmp_path: Path) -> None:
    path = tmp_path / "missing.sqlite3"
    with pytest.raises(sqlite3.OperationalError):
        Registry(path, read_only=True)
    assert not path.exists()


def test_a_legacy_not_null_proc_start_column_is_rebuilt_as_nullable(
    tmp_path: Path, clock: FakeClock
) -> None:
    """Databases created before PID-identity leases declared ``proc_start REAL NOT NULL``.

    A failed identity probe must now store NULL rather than a wall-clock guess, so opening
    an old database has to rebuild the table (sqlite has no ALTER COLUMN). The rebuild must
    preserve existing lease *rows*, or reopening the registry after an upgrade would silently
    drop every held lease and expose live resources to collection. But it must NOT preserve
    the legacy ``proc_start`` *value*: every pre-migration value is a wall-clock guess (the
    old ``converge.py`` stored ``registry.clock.now()`` at acquire time, not a real process
    start time), so carrying it forward would let a later liveness check compare a live
    holder's real start time against that guess, find a mismatch, and treat the lease as
    expired -- exactly the bug this migration exists to prevent. The migration therefore
    rewrites every legacy row's ``proc_start`` to NULL, downgrading it to a PID-only lease
    rather than preserving bogus identity.
    """
    path = tmp_path / "registry.sqlite3"
    with Registry(path, clock=clock) as reg:
        resource = reg.register_resource(
            kind="volume",
            name="legacy-cache",
            stack="test",
            generation="sha256:g",
            scope="spec",
            workspace="/w",
        )
        resource_id = resource.id

    # Recreate the pre-migration schema with a row in it, exactly as an old daemon left it.
    legacy = sqlite3.connect(str(path), isolation_level=None)
    legacy.execute("DROP TABLE leases")
    legacy.execute(
        "CREATE TABLE leases ("
        "id TEXT PRIMARY KEY, "
        "resource_id TEXT NOT NULL REFERENCES resources(id) ON DELETE CASCADE, "
        "pid INTEGER NOT NULL, "
        "proc_start REAL NOT NULL, "
        "acquired_at REAL NOT NULL, "
        "heartbeat_at REAL NOT NULL, "
        "ttl_seconds REAL NOT NULL)"
    )
    legacy.execute(
        "INSERT INTO leases VALUES ('old-lease', ?, 4242, 1.5, 0.0, 0.0, 900.0)", (resource_id,)
    )
    legacy.close()

    with Registry(path, clock=clock) as reg:
        survived = reg.get_lease("old-lease")
        assert survived is not None, "the rebuild must not drop existing leases"
        assert survived.pid == 4242
        assert survived.proc_start is None, "legacy proc_start is a wall-clock guess, not identity"

        # The whole point of the rebuild: NULL is now accepted.
        pid_only = reg.acquire_lease(resource_id, pid=99, proc_start=None)
        assert pid_only.proc_start is None

        columns = reg.conn.execute("PRAGMA table_info(leases)").fetchall()
        proc_start = next(c for c in columns if c["name"] == "proc_start")
        assert not proc_start["notnull"]


def test_a_failed_proc_start_migration_raises_a_named_recoverable_error(
    tmp_path: Path, clock: FakeClock
) -> None:
    """A migration failure must not brick the registry behind an opaque sqlite traceback.

    Simulate a realistic trigger: an orphan ``leases`` row (its ``resource_id`` points at a
    resource that no longer exists) fails the foreign key check when copied into the rebuilt
    table. The failure must surface as a ``RegistryError`` that names the migration and gives
    a next step, not a bare ``sqlite3.IntegrityError`` from deep inside ``__init__``.
    """
    path = tmp_path / "registry.sqlite3"
    with Registry(path, clock=clock):
        pass  # create a fresh v2+ database with the current schema

    legacy = sqlite3.connect(str(path), isolation_level=None)
    legacy.execute("DROP TABLE leases")
    legacy.execute(
        "CREATE TABLE leases ("
        "id TEXT PRIMARY KEY, "
        "resource_id TEXT NOT NULL REFERENCES resources(id) ON DELETE CASCADE, "
        "pid INTEGER NOT NULL, "
        "proc_start REAL NOT NULL, "
        "acquired_at REAL NOT NULL, "
        "heartbeat_at REAL NOT NULL, "
        "ttl_seconds REAL NOT NULL)"
    )
    # foreign_keys defaults to off on a raw connection, so this orphan insert is allowed here
    # but will fail the FK check once the migration rebuilds the table under foreign_keys=ON.
    legacy.execute(
        "INSERT INTO leases VALUES ('orphan-lease', 'no-such-resource', 1, 1.0, 0.0, 0.0, 900.0)"
    )
    legacy.close()

    with pytest.raises(RegistryError, match="relax_lease_proc_start_nullability"):
        Registry(path, clock=clock)


def test_v3_migration_adds_retention_and_defaults_every_existing_row_to_warm(
    tmp_path: Path,
) -> None:
    """Pinning is only ever asserted forward, by a manifest that asks for it (#151).

    A row written before the tier existed was created under the tiered clocks and must keep
    obeying them; inferring a pin for it would leak storage nobody can explain.
    """
    path = tmp_path / "v3.sqlite3"
    connection = sqlite3.connect(path)
    connection.executescript(
        """
        CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
        INSERT INTO meta VALUES ('schema_version', '3'), ('registry_id', 'reg-1');
        CREATE TABLE resources (
            id TEXT PRIMARY KEY, kind TEXT NOT NULL, name TEXT NOT NULL, stack TEXT NOT NULL,
            generation TEXT NOT NULL, scope TEXT NOT NULL, workspace TEXT NOT NULL,
            created_at REAL NOT NULL, last_used REAL NOT NULL,
            state TEXT NOT NULL DEFAULT 'active'
        );
        INSERT INTO resources VALUES
            ('r1', 'volume', 'cache', 'test', 'sha256:g', 'stack', '/w', 1, 2, 'active');
        """
    )
    connection.close()

    with Registry(path) as migrated:
        assert migrated.meta("schema_version") == str(SCHEMA_VERSION)
        assert [r.retention for r in migrated.list_resources()] == ["warm"]
        # And the column is usable for new rows, not merely present.
        migrated.register_resource(
            kind="volume",
            name="guest-storage",
            stack="mac",
            generation="g",
            scope="machine",
            workspace="/w",
            retention="pinned",
        )
        stored = migrated.get_resource_by_engine_identity("volume", "guest-storage")
        assert stored is not None and stored.retention == "pinned"


def test_re_registering_a_volume_can_move_it_off_the_pinned_tier(tmp_path: Path) -> None:
    """Editing `retention` back to warm is how a user un-pins without deleting."""
    with Registry(tmp_path / "r.sqlite3") as registry:
        for retention in ("pinned", "warm"):
            registry.register_resource(
                kind="volume",
                name="storage",
                stack="mac",
                generation="g",
                scope="machine",
                workspace="/w",
                retention=retention,
            )
        stored = registry.get_resource_by_engine_identity("volume", "storage")
        assert stored is not None and stored.retention == "warm"
