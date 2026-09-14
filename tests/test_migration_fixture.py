"""Portable Python-v4 registry fixture for the Rust-state import contract."""

from __future__ import annotations

import json
import os
import shutil
import sqlite3
import subprocess
import sys
from pathlib import Path

import pytest

FIXTURE = Path(__file__).parent / "fixtures" / "migration" / "create_python_v4_registry.py"


def _native_cli_command() -> list[str]:
    """Return the workspace CLI command on both developer and CI hosts.

    ``soldr`` is the local build wrapper used for fast Rust iteration, but it
    is intentionally not required by the GitHub Actions images.  The test
    still invokes the same Cargo binary with the same locked dependency graph
    when the wrapper is unavailable.
    """
    if soldr := shutil.which("soldr"):
        return [soldr, "cargo"]
    return ["cargo"]


def test_native_cli_command_uses_cargo_when_soldr_is_not_installed(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(shutil, "which", lambda executable: None)

    assert _native_cli_command() == ["cargo"]


def test_native_cli_command_uses_soldr_when_available(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(
        shutil,
        "which",
        lambda executable: "/opt/bin/soldr" if executable == "soldr" else None,
    )

    assert _native_cli_command() == ["/opt/bin/soldr", "cargo"]


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


def test_native_cli_imports_the_complete_v4_fixture_without_changing_source(
    tmp_path: Path,
) -> None:
    """Exercise the actual offline product command, not a test-only SQLite path."""
    legacy = tmp_path / "legacy"
    destination = tmp_path / "native"
    legacy.mkdir(mode=0o700)
    destination.mkdir(mode=0o700)
    os.chmod(legacy, 0o700)
    os.chmod(destination, 0o700)
    source = legacy / "registry.sqlite3"
    subprocess.run([sys.executable, str(FIXTURE), str(source)], check=True)
    marker = legacy / "rust-cutover-v1.json"
    marker.write_text(
        '{"protocol":1,"registry_id":"11111111-2222-4333-8444-555555555555"}',
        encoding="utf-8",
    )
    os.chmod(marker, 0o600)
    source_before = source.read_bytes()
    command = [
        *_native_cli_command(),
        "run",
        "-j1",
        "-p",
        "bosn-service",
        "--bin",
        "bosn",
        "--locked",
        "--",
        "registry",
        "import-v4",
        "--legacy-state-dir",
        str(legacy),
        "--state-dir",
        str(destination),
        "--yes",
        "--json",
    ]
    completed = subprocess.run(command, check=True, text=True, capture_output=True)
    receipt = json.loads(completed.stdout.splitlines()[-1])
    assert receipt == {
        "action": "registry_import_v4",
        "reconciliation_required": True,
        "registry_id": "11111111-2222-4333-8444-555555555555",
        "source_preserved": True,
        "table_counts": {
            "events": 1,
            "execution_sessions": 1,
            "generations": 2,
            "leases": 1,
            "meta": 2,
            "resource_uses": 3,
            "resources": 3,
            "volume_creation_intents": 1,
        },
    }
    assert source.read_bytes() == source_before
    with sqlite3.connect(destination / "registry.sqlite3") as connection:
        assert connection.execute(
            "SELECT value FROM meta WHERE key='schema_version'"
        ).fetchone() == ("5",)
        assert connection.execute(
            "SELECT value FROM meta WHERE key='migration.reconciliation_required'"
        ).fetchone() == ("true",)
        assert connection.execute(
            "SELECT id,name,retention FROM resources WHERE id='volume-pinned'"
        ).fetchone() == ("volume-pinned", "bosn-synthetic-guest-disk", "pinned")
    repeated = subprocess.run(command, text=True, capture_output=True)
    assert repeated.returncode != 0
    assert json.loads(repeated.stdout) == {
        "action": "registry_import_v4",
        "error": "cutover refused",
    }
