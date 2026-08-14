"""The sqlite registry.

State is sqlite in WAL mode, chosen over alternatives specifically for failure shape:
WAL readers never block behind a hung writer, so read-only verbs keep working when the
daemon is wedged. The daemon is the only writer; read-only verbs open the file directly.

Losing the database is survivable. Ownership lives in the Docker labels; the registry is
authoritative only for time and leases, so a lost registry rebuilds by rescanning labels.
"""

from __future__ import annotations

import os
import sqlite3
import threading
import uuid
from collections.abc import Iterator
from contextlib import contextmanager
from dataclasses import dataclass
from pathlib import Path

from bosn.clock import Clock, SystemClock

SCHEMA_VERSION = 1

SCHEMA = """
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS resources (
    id          TEXT PRIMARY KEY,
    kind        TEXT NOT NULL,
    name        TEXT NOT NULL,
    stack       TEXT NOT NULL,
    generation  TEXT NOT NULL,
    scope       TEXT NOT NULL,
    workspace   TEXT NOT NULL,
    created_at  REAL NOT NULL,
    last_used   REAL NOT NULL,
    state       TEXT NOT NULL DEFAULT 'active'
);

CREATE INDEX IF NOT EXISTS idx_resources_stack ON resources(stack);
CREATE INDEX IF NOT EXISTS idx_resources_state ON resources(state);

CREATE TABLE IF NOT EXISTS leases (
    id            TEXT PRIMARY KEY,
    resource_id   TEXT NOT NULL REFERENCES resources(id) ON DELETE CASCADE,
    pid           INTEGER NOT NULL,
    proc_start    REAL NOT NULL,
    acquired_at   REAL NOT NULL,
    heartbeat_at  REAL NOT NULL,
    ttl_seconds   REAL NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_leases_resource ON leases(resource_id);

CREATE TABLE IF NOT EXISTS generations (
    digest      TEXT PRIMARY KEY,
    stack       TEXT NOT NULL,
    created_at  REAL NOT NULL,
    superseded_at REAL
);

CREATE TABLE IF NOT EXISTS events (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    at        REAL NOT NULL,
    kind      TEXT NOT NULL,
    detail    TEXT NOT NULL DEFAULT ''
);
"""


def default_state_dir() -> Path:
    override = os.environ.get("BOSN_STATE_DIR")
    if override:
        return Path(override)
    is_windows = os.name.lower().startswith("nt")
    if is_windows:
        base = os.environ.get("LOCALAPPDATA") or str(Path.home() / "AppData" / "Local")
        return Path(base) / "bosn"
    xdg = os.environ.get("XDG_STATE_HOME")
    if xdg:
        return Path(xdg) / "bosn"
    return Path.home() / ".local" / "state" / "bosn"


def default_db_path() -> Path:
    return default_state_dir() / "registry.sqlite3"


@dataclass(frozen=True)
class Resource:
    id: str
    kind: str
    name: str
    stack: str
    generation: str
    scope: str
    workspace: str
    created_at: float
    last_used: float
    state: str = "active"


@dataclass(frozen=True)
class Lease:
    id: str
    resource_id: str
    pid: int
    proc_start: float
    acquired_at: float
    heartbeat_at: float
    ttl_seconds: float

    def expired_by_time(self, now: float) -> bool:
        return (now - self.heartbeat_at) > self.ttl_seconds


class Registry:
    """A WAL-mode sqlite registry. Use as a context manager or call close()."""

    def __init__(
        self,
        path: Path | str | None = None,
        *,
        clock: Clock | None = None,
        read_only: bool = False,
    ) -> None:
        self.path = Path(path) if path is not None else default_db_path()
        self.clock: Clock = clock or SystemClock()
        self.read_only = read_only
        if not read_only:
            self.path.parent.mkdir(parents=True, exist_ok=True)
        # The daemon serves requests on a thread pool while owning one connection, so the
        # connection must outlive its creating thread; a lock keeps it single-writer.
        self._lock = threading.RLock()
        self.conn = sqlite3.connect(str(self.path), isolation_level=None, check_same_thread=False)
        self.conn.row_factory = sqlite3.Row
        with self._lock:
            self.conn.execute("PRAGMA journal_mode=WAL")
            self.conn.execute("PRAGMA foreign_keys=ON")
            self.conn.execute("PRAGMA busy_timeout=5000")
            if not read_only:
                self.conn.executescript(SCHEMA)
                self._ensure_meta()

    # -- lifecycle ---------------------------------------------------------

    def __enter__(self) -> Registry:
        return self

    def __exit__(self, *_exc: object) -> None:
        self.close()

    def close(self) -> None:
        with self._lock:
            self.conn.close()

    def _exec(self, sql: str, params: tuple[object, ...] = ()) -> sqlite3.Cursor:
        """Every statement goes through here so the daemon stays single-writer."""
        with self._lock:
            return self.conn.execute(sql, params)

    @contextmanager
    def lifecycle_guard(self) -> Iterator[None]:
        """Serialize a lifecycle decision and its engine mutation across registry writers.

        GC must not validate that a container is unleased, release the database lock, and
        then stop it after another connection has acquired a lease. ``BEGIN IMMEDIATE``
        excludes writes from other sqlite connections while the in-process lock covers the
        daemon's worker threads. The guarded sections are deliberately narrow and reserved
        for lifecycle mutations that must be atomic with their final registry recheck.
        """
        with self._lock:
            self.conn.execute("BEGIN IMMEDIATE")
            try:
                yield
            except KeyboardInterrupt:
                self.conn.execute("ROLLBACK")
                raise
            except Exception:
                self.conn.execute("ROLLBACK")
                raise
            else:
                self.conn.execute("COMMIT")

    def _ensure_meta(self) -> None:
        self._exec(
            "INSERT OR IGNORE INTO meta(key, value) VALUES ('schema_version', ?)",
            (str(SCHEMA_VERSION),),
        )
        self._exec(
            "INSERT OR IGNORE INTO meta(key, value) VALUES ('registry_id', ?)",
            (str(uuid.uuid4()),),
        )

    def meta(self, key: str) -> str | None:
        row = self._exec("SELECT value FROM meta WHERE key = ?", (key,)).fetchone()
        return row["value"] if row else None

    def set_meta(self, key: str, value: str) -> None:
        """Set metadata during explicit recovery; normal resource writes never need this."""
        self._exec("INSERT OR REPLACE INTO meta(key, value) VALUES (?, ?)", (key, value))

    @property
    def registry_id(self) -> str:
        value = self.meta("registry_id")
        if value is None:
            raise RuntimeError("registry has no registry_id; database not initialized")
        return value

    @property
    def journal_mode(self) -> str:
        return str(self._exec("PRAGMA journal_mode").fetchone()[0]).lower()

    # -- resources ---------------------------------------------------------

    def register_resource(
        self,
        *,
        kind: str,
        name: str,
        stack: str,
        generation: str,
        scope: str,
        workspace: str,
        resource_id: str | None = None,
        created_at: float | None = None,
    ) -> Resource:
        now = self.clock.now()
        rid = resource_id or str(uuid.uuid4())
        created = now if created_at is None else created_at
        self._exec(
            "INSERT OR REPLACE INTO resources"
            "(id, kind, name, stack, generation, scope, workspace, created_at, last_used, state)"
            " VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'active')",
            (rid, kind, name, stack, generation, scope, workspace, created, created),
        )
        self.log_event("resource.registered", f"{kind}:{name}")
        resource = self.get_resource(rid)
        assert resource is not None
        return resource

    def reconcile_resource(
        self,
        *,
        kind: str,
        name: str,
        stack: str,
        generation: str,
        scope: str,
        workspace: str,
    ) -> Resource:
        """Make existing rows for one engine name describe its current owned object.

        Container names are stable across generation replacement. Updating their row in
        place preserves identity for leases while correcting stale generation metadata;
        issue #35 separately owns enforcing uniqueness for every resource kind.
        """
        with self._lock:
            rows = self.conn.execute(
                "SELECT id FROM resources WHERE kind = ? AND name = ? ORDER BY created_at",
                (kind, name),
            ).fetchall()
            if not rows:
                return self.register_resource(
                    kind=kind,
                    name=name,
                    stack=stack,
                    generation=generation,
                    scope=scope,
                    workspace=workspace,
                )
            now = self.clock.now()
            self.conn.execute(
                "UPDATE resources SET stack = ?, generation = ?, scope = ?, workspace = ?, "
                "last_used = ?, state = 'active' WHERE kind = ? AND name = ?",
                (stack, generation, scope, workspace, now, kind, name),
            )
            self.log_event("resource.reconciled", f"{kind}:{name}")
            resource = self.get_resource(str(rows[0]["id"]))
            assert resource is not None
            return resource

    def get_resource(self, resource_id: str) -> Resource | None:
        row = self._exec("SELECT * FROM resources WHERE id = ?", (resource_id,)).fetchone()
        return _resource_from_row(row) if row else None

    def list_resources(self, *, state: str | None = None) -> list[Resource]:
        if state is None:
            rows = self._exec("SELECT * FROM resources ORDER BY created_at").fetchall()
        else:
            rows = self._exec(
                "SELECT * FROM resources WHERE state = ? ORDER BY created_at", (state,)
            ).fetchall()
        return [_resource_from_row(row) for row in rows]

    def touch_resource(self, resource_id: str) -> None:
        self._exec(
            "UPDATE resources SET last_used = ? WHERE id = ?", (self.clock.now(), resource_id)
        )

    def set_resource_state(self, resource_id: str, state: str) -> None:
        self._exec("UPDATE resources SET state = ? WHERE id = ?", (state, resource_id))

    def remove_resource(self, resource_id: str) -> None:
        self._exec("DELETE FROM resources WHERE id = ?", (resource_id,))
        self.log_event("resource.removed", resource_id)

    # -- leases ------------------------------------------------------------

    def acquire_lease(
        self,
        resource_id: str,
        *,
        pid: int,
        proc_start: float,
        ttl_seconds: float = 900.0,
    ) -> Lease:
        now = self.clock.now()
        lease_id = str(uuid.uuid4())
        self._exec(
            "INSERT INTO leases"
            "(id, resource_id, pid, proc_start, acquired_at, heartbeat_at, ttl_seconds)"
            " VALUES (?, ?, ?, ?, ?, ?, ?)",
            (lease_id, resource_id, pid, proc_start, now, now, ttl_seconds),
        )
        self.log_event("lease.acquired", resource_id)
        lease = self.get_lease(lease_id)
        assert lease is not None
        return lease

    def get_lease(self, lease_id: str) -> Lease | None:
        row = self._exec("SELECT * FROM leases WHERE id = ?", (lease_id,)).fetchone()
        return _lease_from_row(row) if row else None

    def leases_for(self, resource_id: str) -> list[Lease]:
        rows = self._exec("SELECT * FROM leases WHERE resource_id = ?", (resource_id,)).fetchall()
        return [_lease_from_row(row) for row in rows]

    def heartbeat(self, lease_id: str) -> None:
        self._exec("UPDATE leases SET heartbeat_at = ? WHERE id = ?", (self.clock.now(), lease_id))

    def release_lease(self, lease_id: str) -> None:
        self._exec("DELETE FROM leases WHERE id = ?", (lease_id,))
        self.log_event("lease.released", lease_id)

    # -- generations -------------------------------------------------------

    def record_generation(self, digest: str, stack: str) -> None:
        self._exec(
            "INSERT OR IGNORE INTO generations(digest, stack, created_at) VALUES (?, ?, ?)",
            (digest, stack, self.clock.now()),
        )

    def supersede_generations(self, stack: str, keep_digest: str) -> int:
        cur = self._exec(
            "UPDATE generations SET superseded_at = ?"
            " WHERE stack = ? AND digest != ? AND superseded_at IS NULL",
            (self.clock.now(), stack, keep_digest),
        )
        return cur.rowcount

    def generation_recorded(self, digest: str) -> bool:
        return (
            self._exec("SELECT 1 FROM generations WHERE digest = ?", (digest,)).fetchone()
            is not None
        )

    def generation_superseded_at(self, digest: str) -> float | None:
        row = self._exec(
            "SELECT superseded_at FROM generations WHERE digest = ?", (digest,)
        ).fetchone()
        return row["superseded_at"] if row else None

    # -- events ------------------------------------------------------------

    def log_event(self, kind: str, detail: str = "") -> None:
        self._exec(
            "INSERT INTO events(at, kind, detail) VALUES (?, ?, ?)",
            (self.clock.now(), kind, detail),
        )

    def events(self, limit: int = 100) -> list[sqlite3.Row]:
        return self._exec("SELECT * FROM events ORDER BY id DESC LIMIT ?", (limit,)).fetchall()


def _resource_from_row(row: sqlite3.Row) -> Resource:
    return Resource(
        id=row["id"],
        kind=row["kind"],
        name=row["name"],
        stack=row["stack"],
        generation=row["generation"],
        scope=row["scope"],
        workspace=row["workspace"],
        created_at=row["created_at"],
        last_used=row["last_used"],
        state=row["state"],
    )


def _lease_from_row(row: sqlite3.Row) -> Lease:
    return Lease(
        id=row["id"],
        resource_id=row["resource_id"],
        pid=row["pid"],
        proc_start=row["proc_start"],
        acquired_at=row["acquired_at"],
        heartbeat_at=row["heartbeat_at"],
        ttl_seconds=row["ttl_seconds"],
    )
