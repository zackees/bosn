#!/usr/bin/env python3
"""Create a deterministic, synthetic Bosn Python-registry schema-v4 database.

Usage: ``python create_python_v4_registry.py DESTINATION``.  This uses only the
standard library and checkpoints WAL before closing, so DESTINATION is one portable
SQLite file.  It is an import characterization fixture, not production migration code.
"""

from __future__ import annotations

import json
import sqlite3
import sys
from pathlib import Path

REGISTRY_ID = "11111111-2222-4333-8444-555555555555"
WORKSPACE_A = "/synthetic/workspace-a"
WORKSPACE_B = "/synthetic/workspace-b"
GENERATION_OLD = "sha256:" + "a" * 64
GENERATION_CURRENT = "sha256:" + "b" * 64

SCHEMA = """
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE resources (
    id TEXT PRIMARY KEY, kind TEXT NOT NULL, name TEXT NOT NULL, stack TEXT NOT NULL,
    generation TEXT NOT NULL, scope TEXT NOT NULL, workspace TEXT NOT NULL,
    created_at REAL NOT NULL, last_used REAL NOT NULL, state TEXT NOT NULL DEFAULT 'active',
    retention TEXT NOT NULL DEFAULT 'warm'
);
CREATE INDEX idx_resources_stack ON resources(stack);
CREATE INDEX idx_resources_state ON resources(state);
CREATE UNIQUE INDEX idx_resources_engine_identity ON resources(kind, name);
CREATE TABLE resource_uses (
    resource_id TEXT NOT NULL REFERENCES resources(id) ON DELETE CASCADE,
    workspace TEXT NOT NULL, stack TEXT NOT NULL, generation TEXT NOT NULL,
    last_used REAL NOT NULL, state TEXT NOT NULL DEFAULT 'active',
    PRIMARY KEY (resource_id, workspace, stack, generation)
);
CREATE INDEX idx_resource_uses_workspace ON resource_uses(workspace);
CREATE TABLE leases (
    id TEXT PRIMARY KEY, resource_id TEXT NOT NULL REFERENCES resources(id) ON DELETE CASCADE,
    pid INTEGER NOT NULL, proc_start REAL, acquired_at REAL NOT NULL,
    heartbeat_at REAL NOT NULL, ttl_seconds REAL NOT NULL
);
CREATE INDEX idx_leases_resource ON leases(resource_id);
CREATE TABLE execution_sessions (
    id TEXT PRIMARY KEY, container_id TEXT NOT NULL, engine_binary TEXT NOT NULL,
    client_pid INTEGER NOT NULL, client_start REAL, lease_ids TEXT NOT NULL
);
CREATE TABLE volume_creation_intents (
    name TEXT PRIMARY KEY, labels TEXT NOT NULL, stack TEXT NOT NULL, generation TEXT NOT NULL,
    scope TEXT NOT NULL, workspace TEXT NOT NULL
);
CREATE TABLE generations (
    workspace TEXT NOT NULL, stack TEXT NOT NULL, digest TEXT NOT NULL, created_at REAL NOT NULL,
    superseded_at REAL, PRIMARY KEY (workspace, stack, digest)
);
CREATE TABLE events (
    id INTEGER PRIMARY KEY AUTOINCREMENT, at REAL NOT NULL, kind TEXT NOT NULL,
    detail TEXT NOT NULL DEFAULT ''
);
"""


def labels(
    *, kind: str, generation: str, scope: str, workspace: str, retention: str | None = None
) -> str:
    value = {
        "com.zackees.bosn.registry": REGISTRY_ID,
        "com.zackees.bosn.kind": kind,
        "com.zackees.bosn.stack": "synthetic",
        "com.zackees.bosn.generation": generation,
        "com.zackees.bosn.scope": scope,
        "com.zackees.bosn.workspace": workspace,
        "com.zackees.bosn.created": "2026-01-02T03:04:05Z",
    }
    if retention is not None:
        value["com.zackees.bosn.retention"] = retention
    return json.dumps(value, sort_keys=True, separators=(",", ":"))


def create(destination: Path) -> None:
    if destination.exists():
        raise SystemExit(f"refusing to overwrite existing fixture: {destination}")
    destination.parent.mkdir(parents=True, exist_ok=True)
    connection = sqlite3.connect(destination)
    try:
        connection.execute("PRAGMA foreign_keys=ON")
        connection.execute("PRAGMA journal_mode=WAL")
        connection.executescript(SCHEMA)
        connection.executemany(
            "INSERT INTO meta(key, value) VALUES (?, ?)",
            [("schema_version", "4"), ("registry_id", REGISTRY_ID)],
        )
        connection.executemany(
            """INSERT INTO resources VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)""",
            [
                (
                    "image-shared",
                    "image",
                    "sha256:image-synthetic",
                    "synthetic",
                    GENERATION_CURRENT,
                    "machine",
                    WORKSPACE_A,
                    1000.0,
                    1100.0,
                    "active",
                    "warm",
                ),
                (
                    "volume-pinned",
                    "volume",
                    "bosn-synthetic-guest-disk",
                    "synthetic",
                    GENERATION_CURRENT,
                    "machine",
                    WORKSPACE_A,
                    1001.0,
                    1101.0,
                    "active",
                    "pinned",
                ),
                (
                    "container-old",
                    "container",
                    "bosn-synthetic-old",
                    "synthetic",
                    GENERATION_OLD,
                    "stack",
                    WORKSPACE_A,
                    900.0,
                    950.0,
                    "retired",
                    "warm",
                ),
            ],
        )
        connection.executemany(
            "INSERT INTO resource_uses VALUES (?, ?, ?, ?, ?, ?)",
            [
                ("image-shared", WORKSPACE_A, "synthetic", GENERATION_CURRENT, 1100.0, "active"),
                ("image-shared", WORKSPACE_B, "synthetic", GENERATION_CURRENT, 1090.0, "active"),
                ("volume-pinned", WORKSPACE_A, "synthetic", GENERATION_CURRENT, 1101.0, "active"),
            ],
        )
        connection.execute(
            "INSERT INTO leases VALUES (?, ?, ?, ?, ?, ?, ?)",
            ("lease-active", "image-shared", 4242, 123.5, 1100.0, 1102.0, 900.0),
        )
        connection.execute(
            "INSERT INTO execution_sessions VALUES (?, ?, ?, ?, ?, ?)",
            ("session-build-01", "container-engine-abc", "docker", 4242, 123.5, '["lease-active"]'),
        )
        connection.execute(
            "INSERT INTO volume_creation_intents VALUES (?, ?, ?, ?, ?, ?)",
            (
                "bosn-intent-volume",
                labels(
                    kind="volume",
                    generation=GENERATION_CURRENT,
                    scope="stack",
                    workspace=WORKSPACE_A,
                ),
                "synthetic",
                GENERATION_CURRENT,
                "stack",
                WORKSPACE_A,
            ),
        )
        connection.executemany(
            "INSERT INTO generations VALUES (?, ?, ?, ?, ?)",
            [
                (WORKSPACE_A, "synthetic", GENERATION_OLD, 900.0, 1000.0),
                (WORKSPACE_A, "synthetic", GENERATION_CURRENT, 1000.0, None),
            ],
        )
        connection.execute(
            "INSERT INTO events(at, kind, detail) VALUES (?, ?, ?)",
            (1102.0, "fixture.created", "synthetic v4 import characterization"),
        )
        connection.commit()
        connection.execute("PRAGMA wal_checkpoint(TRUNCATE)")
    finally:
        connection.close()


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit(f"usage: {Path(sys.argv[0]).name} DESTINATION")
    create(Path(sys.argv[1]))
