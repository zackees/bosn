"""The sqlite registry.

State is sqlite in WAL mode, chosen over alternatives specifically for failure shape:
WAL readers never block behind a hung writer, so read-only verbs keep working when the
daemon is wedged. The daemon is the only writer; read-only verbs open the file directly.

Losing the database is survivable. Ownership lives in the Docker labels; the registry is
authoritative only for time and leases, so a lost registry rebuilds by rescanning labels.
"""

from __future__ import annotations

import json
import os
import sqlite3
import threading
import uuid
from collections.abc import Iterator
from contextlib import contextmanager
from dataclasses import dataclass
from pathlib import Path

from bosn.clock import Clock, SystemClock

SCHEMA_VERSION = 4

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
    state       TEXT NOT NULL DEFAULT 'active',
    retention   TEXT NOT NULL DEFAULT 'warm'
);

CREATE INDEX IF NOT EXISTS idx_resources_stack ON resources(stack);
CREATE INDEX IF NOT EXISTS idx_resources_state ON resources(state);

CREATE TABLE IF NOT EXISTS resource_uses (
    resource_id  TEXT NOT NULL REFERENCES resources(id) ON DELETE CASCADE,
    workspace    TEXT NOT NULL,
    stack        TEXT NOT NULL,
    generation   TEXT NOT NULL,
    last_used    REAL NOT NULL,
    state        TEXT NOT NULL DEFAULT 'active',
    PRIMARY KEY (resource_id, workspace, stack, generation)
);

CREATE INDEX IF NOT EXISTS idx_resource_uses_workspace ON resource_uses(workspace);

CREATE TABLE IF NOT EXISTS leases (
    id            TEXT PRIMARY KEY,
    resource_id   TEXT NOT NULL REFERENCES resources(id) ON DELETE CASCADE,
    pid           INTEGER NOT NULL,
    proc_start    REAL,
    acquired_at   REAL NOT NULL,
    heartbeat_at  REAL NOT NULL,
    ttl_seconds   REAL NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_leases_resource ON leases(resource_id);

CREATE TABLE IF NOT EXISTS execution_sessions (
    id             TEXT PRIMARY KEY,
    container_id   TEXT NOT NULL,
    engine_binary  TEXT NOT NULL,
    client_pid     INTEGER NOT NULL,
    client_start   REAL,
    lease_ids      TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS volume_creation_intents (
    name            TEXT PRIMARY KEY,
    labels          TEXT NOT NULL,
    stack           TEXT NOT NULL,
    generation      TEXT NOT NULL,
    scope           TEXT NOT NULL,
    workspace       TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS generations (
    workspace   TEXT NOT NULL,
    stack       TEXT NOT NULL,
    digest      TEXT NOT NULL,
    created_at  REAL NOT NULL,
    superseded_at REAL,
    PRIMARY KEY (workspace, stack, digest)
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
    # "warm" (the tiered clocks decide) or "pinned" (no automatic rule ever collects it).
    # Persisted on the row rather than re-derived from the manifest because GC runs from the
    # registry alone -- it has no manifest in hand, and the workspace that declared the
    # volume may not even be checked out any more.
    retention: str = "warm"


@dataclass(frozen=True)
class Lease:
    id: str
    resource_id: str
    pid: int
    proc_start: float | None
    acquired_at: float
    heartbeat_at: float
    ttl_seconds: float

    def expired_by_time(self, now: float) -> bool:
        return (now - self.heartbeat_at) > self.ttl_seconds


@dataclass(frozen=True)
class ExecutionSession:
    id: str
    container_id: str
    engine_binary: str
    client_pid: int
    client_start: float | None
    lease_ids: tuple[str, ...]


@dataclass(frozen=True)
class VolumeCreationIntent:
    name: str
    labels: dict[str, str]
    stack: str
    generation: str
    scope: str
    workspace: str


class RegistryError(RuntimeError):
    """A schema migration could not be completed safely."""


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
        if read_only:
            # URI mode=ro refuses a missing database and prevents SQLite from creating WAL,
            # schema, or migration state behind a supposedly read-only CLI command.
            uri = f"file:{self.path.as_posix()}?mode=ro"
            self.conn = sqlite3.connect(
                uri, uri=True, isolation_level=None, check_same_thread=False
            )
        else:
            self.conn = sqlite3.connect(
                str(self.path), isolation_level=None, check_same_thread=False
            )
        self.conn.row_factory = sqlite3.Row
        with self._lock:
            if not read_only:
                self.conn.execute("PRAGMA journal_mode=WAL")
            self.conn.execute("PRAGMA foreign_keys=ON")
            self.conn.execute("PRAGMA busy_timeout=5000")
            if not read_only:
                self.conn.executescript(SCHEMA)
                self._ensure_meta()
                self._migrate_schema()

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

    def _resources_has_retention(self) -> bool:
        rows = self.conn.execute("PRAGMA table_info(resources)").fetchall()
        return any(row["name"] == "retention" for row in rows)

    def _migrate_schema(self) -> None:
        """Upgrade old registries without losing ownership, liveness, or leases."""
        raw_version = self.meta("schema_version")
        version = int(raw_version) if raw_version is not None else 1
        if version > SCHEMA_VERSION:
            raise RuntimeError(
                f"registry schema {version} is newer than supported version {SCHEMA_VERSION}"
            )
        if version < 2:
            self.conn.execute("BEGIN IMMEDIATE")
            try:
                self._deduplicate_resources()
                self.conn.execute(
                    "CREATE TABLE generations_v2 ("
                    "workspace TEXT NOT NULL, stack TEXT NOT NULL, digest TEXT NOT NULL, "
                    "created_at REAL NOT NULL, superseded_at REAL, "
                    "PRIMARY KEY (workspace, stack, digest))"
                )
                self.conn.execute(
                    "INSERT OR IGNORE INTO generations_v2 "
                    "(workspace, stack, digest, created_at, superseded_at) "
                    "SELECT resources.workspace, generations.stack, generations.digest, "
                    "generations.created_at, generations.superseded_at FROM generations "
                    "JOIN resources ON resources.stack = generations.stack "
                    "AND resources.generation = generations.digest"
                )
                self.conn.execute(
                    "INSERT OR IGNORE INTO generations_v2 "
                    "(workspace, stack, digest, created_at, superseded_at) "
                    "SELECT '', generations.stack, generations.digest, "
                    "generations.created_at, generations.superseded_at FROM generations "
                    "WHERE NOT EXISTS (SELECT 1 FROM generations_v2 "
                    "WHERE generations_v2.stack = generations.stack "
                    "AND generations_v2.digest = generations.digest)"
                )
                self.conn.execute("DROP TABLE generations")
                self.conn.execute("ALTER TABLE generations_v2 RENAME TO generations")
                self.conn.execute(
                    "INSERT OR IGNORE INTO resource_uses"
                    "(resource_id, workspace, stack, generation, last_used, state) "
                    "SELECT id, workspace, stack, generation, last_used, state FROM resources"
                )
                self.conn.execute(
                    "UPDATE meta SET value = ? WHERE key = 'schema_version'",
                    (str(SCHEMA_VERSION),),
                )
                self.conn.execute("COMMIT")
            except KeyboardInterrupt:
                self.conn.execute("ROLLBACK")
                raise
            except Exception:
                self.conn.execute("ROLLBACK")
                raise

        if version < 3:
            self.conn.execute("BEGIN IMMEDIATE")
            try:
                self.conn.execute(
                    "CREATE TABLE IF NOT EXISTS volume_creation_intents ("
                    "name TEXT PRIMARY KEY, labels TEXT NOT NULL, stack TEXT NOT NULL, "
                    "generation TEXT NOT NULL, scope TEXT NOT NULL, workspace TEXT NOT NULL)"
                )
                self.conn.execute("UPDATE meta SET value = ? WHERE key = 'schema_version'", ("3",))
                self.conn.execute("COMMIT")
            except KeyboardInterrupt:
                self.conn.execute("ROLLBACK")
                raise
            except Exception:
                self.conn.execute("ROLLBACK")
                raise

        if version < 4:
            # Additive, and deliberately defaulted to 'warm': every row that predates the
            # pinned tier was created under the tiered clocks and must keep obeying them.
            # Pinning is only ever asserted forward, by a manifest that asks for it.
            self.conn.execute("BEGIN IMMEDIATE")
            try:
                if not self._resources_has_retention():
                    self.conn.execute(
                        "ALTER TABLE resources ADD COLUMN retention TEXT NOT NULL DEFAULT 'warm'"
                    )
                self.conn.execute("UPDATE meta SET value = ? WHERE key = 'schema_version'", ("4",))
                self.conn.execute("COMMIT")
            except KeyboardInterrupt:
                self.conn.execute("ROLLBACK")
                raise
            except Exception:
                self.conn.execute("ROLLBACK")
                raise

        # Fresh v2 databases and migrated databases both get the invariant. Keeping the
        # index creation out of SCHEMA lets migration collapse legacy duplicates first.
        self._deduplicate_resources()
        self.conn.execute(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_resources_engine_identity "
            "ON resources(kind, name)"
        )
        self.conn.execute(
            "INSERT OR IGNORE INTO resource_uses"
            "(resource_id, workspace, stack, generation, last_used, state) "
            "SELECT id, workspace, stack, generation, last_used, state FROM resources"
        )
        # Not gated on SCHEMA_VERSION like the v1->v2 step above: this shape change is
        # backward-compatible (a NOT-NULL writer's INSERT still satisfies a nullable column,
        # so an old and a new binary can both operate on either shape) and it is safe to run
        # every open, so `PRAGMA table_info` introspection is the simpler, idempotent gate.
        # Bumping SCHEMA_VERSION would only be needed if a future change made the old shape
        # actively unsafe to read/write, which this one does not.
        try:
            self._relax_lease_proc_start_nullability()
        except KeyboardInterrupt:
            raise
        except Exception as exc:
            raise RegistryError(
                "registry migration 'relax_lease_proc_start_nullability' failed: "
                f"{type(exc).__name__}: {exc}; the database was left unchanged (the migration "
                "runs in its own transaction), so retry opening the registry -- if this "
                "persists, back up and remove the registry file to rebuild it by rescanning "
                "engine labels"
            ) from exc

    def _relax_lease_proc_start_nullability(self) -> None:
        """Allow ``proc_start`` to be NULL for PID-only leases.

        Databases created before this change have ``leases.proc_start REAL NOT NULL``. Every
        row already stored there is a wall-clock guess, not a real process identity: the
        pre-migration ``converge.py`` wrote ``registry.clock.now()`` at acquire time, which
        essentially never matches the holder's real process start time (that would require
        acquiring the lease within the liveness check's tolerance of the process's own
        launch). Carrying that guess forward as if it were identity is actively dangerous --
        a later liveness check would compare it against the real start time, find a mismatch,
        and judge a live holder's lease expired, letting GC reap an in-use resource. So this
        rebuild does not preserve legacy ``proc_start`` values at all: every migrated lease
        becomes PID-only (``proc_start IS NULL``), which is exactly the semantics the nullable
        column exists to express, and falls through to the safe PID-only liveness check.
        SQLite has no ``ALTER COLUMN``, so relaxing the constraint means rebuilding the table.
        """
        columns = self.conn.execute("PRAGMA table_info(leases)").fetchall()
        proc_start_column = next((c for c in columns if c["name"] == "proc_start"), None)
        if proc_start_column is None or not proc_start_column["notnull"]:
            return
        self.conn.execute("BEGIN IMMEDIATE")
        try:
            self.conn.execute(
                "CREATE TABLE leases_nullable ("
                "id TEXT PRIMARY KEY, "
                "resource_id TEXT NOT NULL REFERENCES resources(id) ON DELETE CASCADE, "
                "pid INTEGER NOT NULL, "
                "proc_start REAL, "
                "acquired_at REAL NOT NULL, "
                "heartbeat_at REAL NOT NULL, "
                "ttl_seconds REAL NOT NULL)"
            )
            self.conn.execute(
                "INSERT INTO leases_nullable"
                "(id, resource_id, pid, proc_start, acquired_at, heartbeat_at, ttl_seconds) "
                "SELECT id, resource_id, pid, NULL, acquired_at, heartbeat_at, "
                "ttl_seconds FROM leases"
            )
            self.conn.execute("DROP TABLE leases")
            self.conn.execute("ALTER TABLE leases_nullable RENAME TO leases")
            self.conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_leases_resource ON leases(resource_id)"
            )
            self.conn.execute("COMMIT")
        except KeyboardInterrupt:
            self.conn.execute("ROLLBACK")
            raise
        except Exception:
            self.conn.execute("ROLLBACK")
            raise

    def _deduplicate_resources(self) -> None:
        duplicates = self.conn.execute(
            "SELECT kind, name FROM resources GROUP BY kind, name HAVING COUNT(*) > 1"
        ).fetchall()
        for duplicate in duplicates:
            rows = self.conn.execute(
                "SELECT * FROM resources WHERE kind = ? AND name = ? "
                "ORDER BY last_used DESC, created_at DESC, id DESC",
                (duplicate["kind"], duplicate["name"]),
            ).fetchall()
            keeper = rows[0]
            for stale in rows[1:]:
                self.conn.execute(
                    "UPDATE leases SET resource_id = ? WHERE resource_id = ?",
                    (keeper["id"], stale["id"]),
                )
                self.conn.execute("DELETE FROM resources WHERE id = ?", (stale["id"],))
            self.conn.execute(
                "UPDATE resources SET created_at = ?, last_used = ? WHERE id = ?",
                (
                    min(float(row["created_at"]) for row in rows),
                    max(float(row["last_used"]) for row in rows),
                    keeper["id"],
                ),
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

    def integrity_check(self) -> str:
        """Return SQLite's read-only integrity diagnosis without altering the database.

        Damage bad enough to stop the pager makes the PRAGMA itself raise rather than
        report -- reproducibly so on Linux, where a page-header hit that Windows happened
        to survive raises "database disk image is malformed". Reporting that as a string
        is the whole point: `doctor` prints recovery guidance from this value, so raising
        here would crash the diagnostic in exactly the corrupt-database case it exists to
        diagnose. Never returns "ok" for a database it could not read.
        """
        try:
            row = self._exec("PRAGMA integrity_check").fetchone()
        except sqlite3.DatabaseError as exc:
            return f"unreadable: {exc}"
        return str(row[0]) if row is not None else "no result"

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
        retention: str = "warm",
    ) -> Resource:
        return self._reconcile_resource(
            kind=kind,
            name=name,
            stack=stack,
            generation=generation,
            scope=scope,
            workspace=workspace,
            resource_id=resource_id,
            created_at=created_at,
            retention=retention,
        )

    def reconcile_resource(
        self,
        *,
        kind: str,
        name: str,
        stack: str,
        generation: str,
        scope: str,
        workspace: str,
        retention: str = "warm",
    ) -> Resource:
        """Make one engine object have one current, freshly-used registry identity."""
        return self._reconcile_resource(
            kind=kind,
            name=name,
            stack=stack,
            generation=generation,
            scope=scope,
            workspace=workspace,
            retention=retention,
        )

    def _reconcile_resource(
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
        retention: str = "warm",
    ) -> Resource:
        with self._lock:
            previous = self.conn.execute(
                "SELECT id FROM resources WHERE kind = ? AND name = ?",
                (kind, name),
            ).fetchone()
            now = self.clock.now()
            rid = resource_id or str(uuid.uuid4())
            created = now if created_at is None else created_at
            self.conn.execute(
                "INSERT INTO resources"
                "(id, kind, name, stack, generation, scope, workspace, created_at, "
                "last_used, state, retention) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'active', ?) "
                "ON CONFLICT(kind, name) DO UPDATE SET stack = excluded.stack, "
                "generation = excluded.generation, scope = excluded.scope, "
                # Retention follows the current declaration in both directions. Re-declaring
                # a volume `warm` after it was pinned is how a user un-pins one without
                # deleting it, and the digest rolled when they made that edit, so this
                # statement is the first write of the new generation, not a stale one
                # overwriting a fresh intent.
                "workspace = excluded.workspace, retention = excluded.retention, "
                "last_used = ?, state = 'active'",
                (
                    rid,
                    kind,
                    name,
                    stack,
                    generation,
                    scope,
                    workspace,
                    created,
                    created,
                    retention,
                    now,
                ),
            )
            current = self.conn.execute(
                "SELECT id FROM resources WHERE kind = ? AND name = ?", (kind, name)
            ).fetchone()
            assert current is not None
            rid = str(current["id"])
            self.conn.execute(
                "INSERT INTO resource_uses"
                "(resource_id, workspace, stack, generation, last_used, state) "
                "VALUES (?, ?, ?, ?, ?, 'active') "
                "ON CONFLICT(resource_id, workspace, stack, generation) "
                "DO UPDATE SET last_used = excluded.last_used, state = 'active'",
                (rid, workspace, stack, generation, now),
            )
            event = "resource.registered" if previous is None else "resource.reconciled"
            self.log_event(event, f"{kind}:{name}")
            resource = self.get_resource(rid)
            assert resource is not None
            return resource

    def get_resource(self, resource_id: str) -> Resource | None:
        row = self._exec("SELECT * FROM resources WHERE id = ?", (resource_id,)).fetchone()
        return _resource_from_row(row) if row else None

    def get_resource_by_engine_identity(self, kind: str, name: str) -> Resource | None:
        row = self._exec(
            "SELECT * FROM resources WHERE kind = ? AND name = ?", (kind, name)
        ).fetchone()
        return _resource_from_row(row) if row else None

    def canonicalize_image_identity(self, tag: str, image_id: str) -> Resource | None:
        """Use Docker's immutable image ID for both new and adopted image rows.

        Older registries stored either the mutable build tag or Docker's truncated image
        ID. This small migration preserves their consumer uses and leases while moving
        both legacy aliases to the ID that `docker image ls` and adoption now expose.
        """
        # Preserve the order for predictable migrations and logs: the old mutable tag
        # first, then Docker's legacy 12-character `image ls` ID.
        aliases = (tag, image_id.removeprefix("sha256:")[:12])
        with self._lock:
            canonical = self.conn.execute(
                "SELECT * FROM resources WHERE kind = 'image' AND name = ?", (image_id,)
            ).fetchone()
            for alias in dict.fromkeys(aliases):
                if alias == image_id:
                    continue
                legacy = self.conn.execute(
                    "SELECT id FROM resources WHERE kind = 'image' AND name = ?", (alias,)
                ).fetchone()
                if legacy is None:
                    continue
                if canonical is None:
                    self.conn.execute(
                        "UPDATE resources SET name = ? WHERE id = ?",
                        (image_id, legacy["id"]),
                    )
                    canonical = self.conn.execute(
                        "SELECT * FROM resources WHERE id = ?", (legacy["id"],)
                    ).fetchone()
                    assert canonical is not None
                    continue
                self._merge_resource_rows(
                    source_id=str(legacy["id"]), target_id=str(canonical["id"])
                )
            return _resource_from_row(canonical) if canonical is not None else None

    def _merge_resource_rows(self, *, source_id: str, target_id: str) -> None:
        """Move leases and consumer uses into an existing canonical resource row."""
        source = self.get_resource(source_id)
        assert source is not None
        for use in self.conn.execute(
            "SELECT workspace, stack, generation, last_used, state "
            "FROM resource_uses WHERE resource_id = ?",
            (source_id,),
        ).fetchall():
            existing = self.conn.execute(
                "SELECT last_used, state FROM resource_uses "
                "WHERE resource_id = ? AND workspace = ? AND stack = ? AND generation = ?",
                (target_id, use["workspace"], use["stack"], use["generation"]),
            ).fetchone()
            if existing is None:
                self.conn.execute(
                    "INSERT INTO resource_uses"
                    "(resource_id, workspace, stack, generation, last_used, state) "
                    "VALUES (?, ?, ?, ?, ?, ?)",
                    (
                        target_id,
                        use["workspace"],
                        use["stack"],
                        use["generation"],
                        use["last_used"],
                        use["state"],
                    ),
                )
                continue
            state = (
                "active"
                if "active" in {str(existing["state"]), str(use["state"])}
                else str(existing["state"])
            )
            self.conn.execute(
                "UPDATE resource_uses SET last_used = ?, state = ? "
                "WHERE resource_id = ? AND workspace = ? AND stack = ? AND generation = ?",
                (
                    max(float(existing["last_used"]), float(use["last_used"])),
                    state,
                    target_id,
                    use["workspace"],
                    use["stack"],
                    use["generation"],
                ),
            )
        self.conn.execute(
            "UPDATE leases SET resource_id = ? WHERE resource_id = ?", (target_id, source_id)
        )
        self.conn.execute("DELETE FROM resources WHERE id = ?", (source_id,))
        self.conn.execute(
            "UPDATE resources SET created_at = MIN(created_at, ?), "
            "last_used = MAX(last_used, ?) WHERE id = ?",
            (source.created_at, source.last_used, target_id),
        )

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
        with self._lock:
            self.conn.execute("UPDATE resources SET state = ? WHERE id = ?", (state, resource_id))
            self.conn.execute(
                "UPDATE resource_uses SET state = ? WHERE resource_id = ?",
                (state, resource_id),
            )

    def _all_uses_done(self, resource_id: str) -> bool:
        uses = self._exec(
            "SELECT workspace, state FROM resource_uses WHERE resource_id = ?",
            (resource_id,),
        ).fetchall()
        return bool(uses) and all(str(use["state"]) == "done" for use in uses)

    def resource_retention_signals(
        self, resource_id: str, *, done_workspaces: set[str] | None = None
    ) -> tuple[bool, bool]:
        """Return (superseded, done) only when every consumer is inactive.

        A mixed object whose A consumer is done and B consumer is superseded is safe to
        retire using superseded policy -- eager for images, conservatively capped otherwise
        -- rather than immediate done collection. Any one active/current consumer keeps the
        shared engine object.
        """
        done_workspaces = done_workspaces or set()
        uses = self._exec(
            "SELECT workspace, stack, generation, state FROM resource_uses WHERE resource_id = ?",
            (resource_id,),
        ).fetchall()
        if not uses:
            return False, False
        any_superseded = False
        for use in uses:
            is_done = str(use["state"]) == "done" or str(use["workspace"]) in done_workspaces
            is_superseded = (
                self.generation_superseded_at(
                    str(use["generation"]),
                    stack=str(use["stack"]),
                    workspace=str(use["workspace"]),
                )
                is not None
            )
            if not is_done and not is_superseded:
                return False, False
            any_superseded = any_superseded or is_superseded
        return (True, False) if any_superseded else (False, True)

    def mark_workspace_done(self, workspace: str) -> int:
        with self._lock:
            rows = self.conn.execute(
                "SELECT DISTINCT resource_uses.resource_id FROM resource_uses "
                "JOIN resources ON resources.id = resource_uses.resource_id "
                "WHERE resource_uses.workspace = ? AND resources.scope != 'machine'",
                (workspace,),
            ).fetchall()
            resource_ids = [str(row["resource_id"]) for row in rows]
            self.conn.execute(
                "UPDATE resource_uses SET state = 'done' WHERE workspace = ? "
                "AND resource_id IN (SELECT id FROM resources WHERE scope != 'machine')",
                (workspace,),
            )
            for resource_id in resource_ids:
                state = "done" if self._all_uses_done(resource_id) else "active"
                self.conn.execute(
                    "UPDATE resources SET state = ? WHERE id = ?", (state, resource_id)
                )
            return len(resource_ids)

    def done_workspace_ids(self) -> set[str]:
        rows = self._exec(
            "SELECT resource_uses.workspace FROM resource_uses "
            "JOIN resources ON resources.id = resource_uses.resource_id "
            "WHERE resources.scope != 'machine' GROUP BY resource_uses.workspace "
            "HAVING SUM(CASE WHEN resource_uses.state = 'done' THEN 0 ELSE 1 END) = 0"
        ).fetchall()
        return {str(row["workspace"]) for row in rows}

    def remove_resource(self, resource_id: str) -> None:
        self._exec("DELETE FROM resources WHERE id = ?", (resource_id,))
        self.log_event("resource.removed", resource_id)

    # -- leases ------------------------------------------------------------

    def acquire_lease(
        self,
        resource_id: str,
        *,
        pid: int,
        proc_start: float | None,
        ttl_seconds: float = 900.0,
    ) -> Lease:
        now = self.clock.now()
        lease_id = str(uuid.uuid4())
        with self._lock:
            self.conn.execute(
                "INSERT INTO leases"
                "(id, resource_id, pid, proc_start, acquired_at, heartbeat_at, ttl_seconds)"
                " VALUES (?, ?, ?, ?, ?, ?, ?)",
                (lease_id, resource_id, pid, proc_start, now, now, ttl_seconds),
            )
            self.conn.execute(
                "UPDATE resources SET last_used = ?, state = 'active' WHERE id = ?",
                (now, resource_id),
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

    def all_leases(self) -> list[Lease]:
        rows = self._exec("SELECT * FROM leases").fetchall()
        return [_lease_from_row(row) for row in rows]

    def heartbeat(self, lease_id: str) -> None:
        self._exec("UPDATE leases SET heartbeat_at = ? WHERE id = ?", (self.clock.now(), lease_id))

    def release_lease(self, lease_id: str) -> None:
        self._exec("DELETE FROM leases WHERE id = ?", (lease_id,))
        self.log_event("lease.released", lease_id)

    def save_execution_session(self, session: ExecutionSession) -> None:
        """Persist ownership before a foreground client may start its remote command."""
        self._exec(
            "INSERT INTO execution_sessions"
            "(id, container_id, engine_binary, client_pid, client_start, lease_ids) "
            "VALUES (?, ?, ?, ?, ?, ?)",
            (
                session.id,
                session.container_id,
                session.engine_binary,
                session.client_pid,
                session.client_start,
                json.dumps(session.lease_ids),
            ),
        )

    def delete_execution_session(self, session_id: str) -> None:
        self._exec("DELETE FROM execution_sessions WHERE id = ?", (session_id,))

    def execution_sessions(self) -> list[ExecutionSession]:
        rows = self._exec("SELECT * FROM execution_sessions ORDER BY id").fetchall()
        sessions: list[ExecutionSession] = []
        for row in rows:
            raw_lease_ids = json.loads(row["lease_ids"])
            if not isinstance(raw_lease_ids, list) or not all(
                isinstance(lease_id, str) for lease_id in raw_lease_ids
            ):
                raise RegistryError(
                    f"execution session {row['id']!r} has invalid persisted lease ids"
                )
            sessions.append(
                ExecutionSession(
                    id=row["id"],
                    container_id=row["container_id"],
                    engine_binary=row["engine_binary"],
                    client_pid=row["client_pid"],
                    client_start=row["client_start"],
                    lease_ids=tuple(raw_lease_ids),
                )
            )
        return sessions

    def save_volume_creation_intent(self, intent: VolumeCreationIntent) -> None:
        self._exec(
            "INSERT OR REPLACE INTO volume_creation_intents "
            "(name, labels, stack, generation, scope, workspace) VALUES (?, ?, ?, ?, ?, ?)",
            (
                intent.name,
                json.dumps(intent.labels, sort_keys=True),
                intent.stack,
                intent.generation,
                intent.scope,
                intent.workspace,
            ),
        )

    def volume_creation_intent(self, name: str) -> VolumeCreationIntent | None:
        row = self._exec("SELECT * FROM volume_creation_intents WHERE name = ?", (name,)).fetchone()
        if row is None:
            return None
        raw_labels = json.loads(row["labels"])
        if not isinstance(raw_labels, dict) or not all(
            isinstance(key, str) and isinstance(value, str) for key, value in raw_labels.items()
        ):
            raise RegistryError(f"volume creation intent {name!r} has invalid labels")
        return VolumeCreationIntent(
            name=row["name"],
            labels=raw_labels,
            stack=row["stack"],
            generation=row["generation"],
            scope=row["scope"],
            workspace=row["workspace"],
        )

    def delete_volume_creation_intent(self, name: str) -> None:
        self._exec("DELETE FROM volume_creation_intents WHERE name = ?", (name,))

    def prune_lease(self, lease_id: str) -> None:
        """Delete a confirmed-dead lease found during maintenance.

        A distinct event from ``lease.released`` -- that one is a holder's own graceful
        release, this one is the daemon reclaiming a lease whose holder is gone. Deleting a
        row that is already gone is a silent no-op, which is what keeps pruning idempotent.
        """
        self._exec("DELETE FROM leases WHERE id = ?", (lease_id,))
        self.log_event("lease.pruned", lease_id)

    # -- generations -------------------------------------------------------

    def record_generation(self, digest: str, stack: str, workspace: str) -> None:
        self._exec(
            "INSERT INTO generations(workspace, stack, digest, created_at, superseded_at) "
            "VALUES (?, ?, ?, ?, NULL) ON CONFLICT(workspace, stack, digest) "
            "DO UPDATE SET superseded_at = NULL",
            (workspace, stack, digest, self.clock.now()),
        )

    def supersede_generations(self, stack: str, keep_digest: str, workspace: str) -> int:
        cur = self._exec(
            "UPDATE generations SET superseded_at = ?"
            " WHERE workspace = ? AND stack = ? AND digest != ? AND superseded_at IS NULL",
            (self.clock.now(), workspace, stack, keep_digest),
        )
        return cur.rowcount

    def generation_recorded(self, digest: str, stack: str, workspace: str) -> bool:
        return (
            self._exec(
                "SELECT 1 FROM generations WHERE workspace = ? AND stack = ? AND digest = ?",
                (workspace, stack, digest),
            ).fetchone()
            is not None
        )

    def generation_superseded_at(self, digest: str, *, stack: str, workspace: str) -> float | None:
        row = self._exec(
            "SELECT superseded_at FROM generations "
            "WHERE workspace = ? AND stack = ? AND digest = ?",
            (workspace, stack, digest),
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

    def latest_event(self, kind: str, *, detail_prefix: str = "") -> sqlite3.Row | None:
        """Return the newest event of ``kind``, optionally scoped to one durable identity.

        Diagnostic callers use this to explain a protected state without treating an event
        as authority to change it.  Prefix matching is deliberately parameterized: execution
        recovery events begin with ``session=<uuid> `` and a report for one session must not
        accidentally attribute another session's engine error to it.
        """
        if detail_prefix:
            return self._exec(
                "SELECT * FROM events WHERE kind = ? AND detail LIKE ? ORDER BY id DESC LIMIT 1",
                (kind, f"{detail_prefix}%"),
            ).fetchone()
        return self._exec(
            "SELECT * FROM events WHERE kind = ? ORDER BY id DESC LIMIT 1", (kind,)
        ).fetchone()


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
        # A registry restored from a backup taken before schema 4, or read through a
        # connection opened read-only against an un-migrated file, has no such column.
        # Absent means warm, which is the safe reading: it never invents a pin.
        retention=(row["retention"] if "retention" in row.keys() else "warm") or "warm",
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
