"""Portable Python-v4 registry fixture for the Rust-state import contract."""

from __future__ import annotations

import json
import sqlite3
import subprocess
import sys
from pathlib import Path

import pytest

FIXTURE = Path(__file__).parent / "fixtures" / "migration" / "create_python_v4_registry.py"


def test_python_v4_registry_fixture_covers_import_relationships(tmp_path: Path) -> None:
    """The fixture deliberately exercises every v4 table and critical import links."""
    destination = tmp_path / "python-v4.sqlite3"
    subprocess.run([sys.executable, str(FIXTURE), str(destination)], check=True)

    with sqlite3.connect(destination) as connection:
        tables = {
            row[0]
            for row in connection.execute(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'"
            )
        }
        assert tables == {
            "meta",
            "resources",
            "resource_uses",
            "leases",
            "execution_sessions",
            "volume_creation_intents",
            "generations",
            "events",
        }
        assert connection.execute(
            "SELECT value FROM meta WHERE key = 'schema_version'"
        ).fetchone() == ("4",)
        assert connection.execute(
            "SELECT value FROM meta WHERE key = 'registry_id'"
        ).fetchone() == ("11111111-2222-4333-8444-555555555555",)
        assert connection.execute(
            "SELECT retention FROM resources WHERE id = 'volume-pinned'"
        ).fetchone() == ("pinned",)
        assert connection.execute(
            "SELECT COUNT(*) FROM resource_uses WHERE resource_id = 'image-shared'"
        ).fetchone() == (2,)
        assert connection.execute(
            "SELECT resource_id FROM leases WHERE id = 'lease-active'"
        ).fetchone() == ("image-shared",)
        assert connection.execute(
            "SELECT lease_ids FROM execution_sessions WHERE id = 'session-build-01'"
        ).fetchone() == ('["lease-active"]',)
        intent_labels = connection.execute(
            "SELECT labels FROM volume_creation_intents WHERE name = 'bosn-intent-volume'"
        ).fetchone()[0]
        assert json.loads(intent_labels) == {
            "com.zackees.bosn.created": "2026-01-02T03:04:05Z",
            "com.zackees.bosn.generation": "sha256:" + "b" * 64,
            "com.zackees.bosn.kind": "volume",
            "com.zackees.bosn.registry": "11111111-2222-4333-8444-555555555555",
            "com.zackees.bosn.scope": "stack",
            "com.zackees.bosn.stack": "synthetic",
            "com.zackees.bosn.workspace": "/synthetic/workspace-a",
        }
        assert connection.execute("SELECT COUNT(*) FROM generations").fetchone() == (2,)
        assert connection.execute("SELECT kind FROM events").fetchone() == ("fixture.created",)
        indexes = {row[1]: row[2] for row in connection.execute("PRAGMA index_list(resources)")}
        assert indexes["idx_resources_engine_identity"] == 1
        with pytest.raises(sqlite3.IntegrityError):
            connection.execute(
                "INSERT INTO resources SELECT 'duplicate-image', kind, name, stack, generation, "
                "scope, workspace, created_at, last_used, state, retention "
                "FROM resources WHERE id = 'image-shared'"
            )
        assert connection.execute("PRAGMA integrity_check").fetchone() == ("ok",)
        assert connection.execute("PRAGMA foreign_key_check").fetchall() == []
