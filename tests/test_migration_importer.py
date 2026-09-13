"""Cross-language Python-v4 fixture proof and Rust importer failure matrix."""

from __future__ import annotations

import hashlib
import json
import os
import sqlite3
import subprocess
import sys
from pathlib import Path

import pytest

from bosn.migration_lock import acquire_shared

ROOT = Path(__file__).parents[1]
FIXTURE = ROOT / "tests" / "fixtures" / "migration" / "create_python_v4_registry.py"
REGISTRY_ID = "11111111-2222-4333-8444-555555555555"

# This harness creates its synthetic cutover marker with POSIX mode 0600. Native
# Windows ACL preparation belongs in a kernel-backed test helper before this can
# claim equivalent Windows coverage.
pytestmark = pytest.mark.skipif(
    os.name == "nt", reason="migration importer fixture currently has POSIX-only private setup"
)


@pytest.fixture(scope="session")
def rust_import_helper() -> Path:
    result = subprocess.run(
        [
            "soldr",
            "cargo",
            "build",
            "--locked",
            "-p",
            "bosn-registry",
            "--features",
            "migration-test-helper",
            "--bin",
            "migration-import-test",
            "--message-format=json",
        ],
        cwd=ROOT,
        capture_output=True,
        text=True,
        timeout=180,
        check=False,
    )
    assert result.returncode == 0, result.stderr
    for line in result.stdout.splitlines():
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if event.get("reason") == "compiler-artifact" and event.get("target", {}).get(
            "name"
        ) == "migration-import-test":
            executable = event.get("executable")
            if isinstance(executable, str):
                return Path(executable)
    pytest.fail("Rust migration-import helper build produced no executable artifact")


def make_fixture(tmp_path: Path, *, marker: bool = True) -> tuple[Path, Path]:
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    source = state / "registry.sqlite3"
    subprocess.run([sys.executable, str(FIXTURE), str(source)], check=True)
    child = subprocess.Popen([sys.executable, "-c", "pass"])
    assert child.wait(timeout=5) == 0
    with sqlite3.connect(source) as connection:
        connection.execute("UPDATE leases SET pid=?", (child.pid,))
        connection.execute("UPDATE execution_sessions SET client_pid=?", (child.pid,))
        connection.commit()
    if marker:
        (state / "rust-cutover-v1.json").write_text(
            json.dumps({"protocol": 1, "registry_id": REGISTRY_ID}), encoding="utf-8"
        )
        os.chmod(state / "rust-cutover-v1.json", 0o600)
    return state, source


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def invoke(helper: Path, state: Path, destination: Path) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [str(helper), str(state), str(destination)],
        capture_output=True,
        text=True,
        timeout=20,
        check=False,
    )


def test_real_python_v4_fixture_imports_all_tables_and_preserves_source(
    tmp_path: Path, rust_import_helper: Path
) -> None:
    state, source = make_fixture(tmp_path)
    before = digest(source)
    destination = tmp_path / "imported.sqlite3"

    result = invoke(rust_import_helper, state, destination)

    assert result.returncode == 0, result.stderr
    assert result.stdout.strip() == REGISTRY_ID
    assert digest(source) == before
    with sqlite3.connect(destination) as connection:
        counts = {
            table: connection.execute(f"SELECT count(*) FROM {table}").fetchone()[0]
            for table in (
                "meta",
                "resources",
                "resource_uses",
                "leases",
                "execution_sessions",
                "volume_creation_intents",
                "generations",
                "events",
            )
        }
        assert counts == {
            "meta": 3,
            "resources": 3,
            "resource_uses": 3,
            "leases": 1,
            "execution_sessions": 1,
            "volume_creation_intents": 1,
            "generations": 2,
            "events": 1,
        }
        assert connection.execute("SELECT value FROM meta WHERE key='registry_id'").fetchone() == (
            REGISTRY_ID,
        )
        assert connection.execute(
            "SELECT state FROM resources WHERE id='container-old'"
        ).fetchone() == (
            "retired",
        )
        assert connection.execute(
            "SELECT retention FROM resources WHERE id='volume-pinned'"
        ).fetchone() == (
            "pinned",
        )
        assert connection.execute(
            "SELECT count(*) FROM resource_uses WHERE resource_id='image-shared'"
        ).fetchone() == (2,)
        assert connection.execute("SELECT lease_ids FROM execution_sessions").fetchone() == (
            '["lease-active"]',
        )
        labels = connection.execute("SELECT labels FROM volume_creation_intents").fetchone()[0]
        assert labels.startswith('{"com.zackees.bosn.created"')
        assert connection.execute(
            "SELECT superseded_at FROM generations WHERE digest LIKE 'sha256:a%'"
        ).fetchone() == (
            1000.0,
        )
        assert connection.execute("SELECT kind FROM events").fetchone() == ("fixture.created",)


@pytest.mark.parametrize(
    ("mutate", "expected"),
    [
        (lambda c: c.execute("UPDATE execution_sessions SET lease_ids='not-json'"), "BadRow"),
        (
            lambda c: (
                c.execute("PRAGMA foreign_keys=OFF"),
                c.execute("DELETE FROM resources WHERE id='image-shared'"),
            ),
            "InvalidSchema",
        ),
        (
            lambda c: c.execute("UPDATE meta SET value='5' WHERE key='schema_version'"),
            "UnsupportedSchema",
        ),
        (
            lambda c: c.execute("UPDATE sqlite_sequence SET seq=0 WHERE name='events'"),
            "InvalidSchema",
        ),
    ],
    ids=["malformed-json", "foreign-key", "newer-schema", "bad-event-sequence"],
)
def test_import_rejections_leave_source_and_destination_unchanged(
    tmp_path: Path, rust_import_helper: Path, mutate, expected: str
) -> None:
    state, source = make_fixture(tmp_path)
    with sqlite3.connect(source) as connection:
        mutate(connection)
        connection.commit()
    before = digest(source)
    destination = tmp_path / "not-published.sqlite3"

    result = invoke(rust_import_helper, state, destination)

    assert result.returncode != 0
    assert expected in result.stderr
    assert digest(source) == before
    assert not destination.exists()


def exited_child_pid() -> int:
    child = subprocess.Popen([sys.executable, "-c", "pass"])
    assert child.wait(timeout=5) == 0
    return child.pid


def test_mismatched_marker_refuses_without_publishing(
    tmp_path: Path, rust_import_helper: Path
) -> None:
    state, source = make_fixture(tmp_path)
    destination = tmp_path / "destination.sqlite3"
    before = digest(source)
    (state / "rust-cutover-v1.json").write_text(
        json.dumps({"protocol": 1, "registry_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"}),
        encoding="utf-8",
    )
    os.chmod(state / "rust-cutover-v1.json", 0o600)
    result = invoke(rust_import_helper, state, destination)
    assert result.returncode != 0 and "CutoverRegistryMismatch" in result.stderr
    assert digest(source) == before and not destination.exists()


def test_live_owner_refuses_without_publishing(tmp_path: Path, rust_import_helper: Path) -> None:
    state, source = make_fixture(tmp_path)
    destination = tmp_path / "destination.sqlite3"
    (state / "rust-cutover-v1.json").write_text(
        json.dumps({"protocol": 1, "registry_id": REGISTRY_ID}), encoding="utf-8"
    )
    os.chmod(state / "rust-cutover-v1.json", 0o600)
    with sqlite3.connect(source) as connection:
        connection.execute("UPDATE leases SET pid=?", (os.getpid(),))
        connection.execute("UPDATE execution_sessions SET client_pid=?", (os.getpid(),))
        connection.commit()
    before = digest(source)
    result = invoke(rust_import_helper, state, destination)
    assert result.returncode != 0 and "SourceOwnershipLive" in result.stderr
    assert digest(source) == before and not destination.exists()


def test_existing_destination_is_untouched(tmp_path: Path, rust_import_helper: Path) -> None:
    state, source = make_fixture(tmp_path)
    destination = tmp_path / "destination.sqlite3"
    with sqlite3.connect(source) as connection:
        pid = exited_child_pid()
        connection.execute("UPDATE leases SET pid=?", (pid,))
        connection.execute("UPDATE execution_sessions SET client_pid=?", (pid,))
        connection.commit()
    destination.write_bytes(b"existing target must survive")
    before_target = digest(destination)
    result = invoke(rust_import_helper, state, destination)
    assert result.returncode != 0 and "ImportTargetExists" in result.stderr
    assert digest(destination) == before_target


def test_held_migration_guard_refuses_without_publishing(
    tmp_path: Path, rust_import_helper: Path
) -> None:
    state, _source = make_fixture(tmp_path, marker=False)
    destination = tmp_path / "destination.sqlite3"
    guard = acquire_shared(state)
    try:
        (state / "rust-cutover-v1.json").write_text(
            json.dumps({"protocol": 1, "registry_id": REGISTRY_ID}), encoding="utf-8"
        )
        os.chmod(state / "rust-cutover-v1.json", 0o600)
        result = invoke(rust_import_helper, state, destination)
        assert result.returncode != 0 and "MigrationGuardHeld" in result.stderr
        assert not destination.exists()
    finally:
        guard.close()


def test_import_paginates_more_than_one_thousand_events(
    tmp_path: Path, rust_import_helper: Path
) -> None:
    state, source = make_fixture(tmp_path)
    with sqlite3.connect(source) as connection:
        connection.executemany(
            "INSERT INTO events(at,kind,detail) VALUES (?,?,?)",
            [(2000.0 + index, "bulk", f"event-{index}") for index in range(1001)],
        )
        connection.commit()
    before = digest(source)
    destination = tmp_path / "paged.sqlite3"

    result = invoke(rust_import_helper, state, destination)

    assert result.returncode == 0, result.stderr
    assert digest(source) == before
    with sqlite3.connect(destination) as connection:
        assert connection.execute("SELECT count(*) FROM events").fetchone() == (1002,)
        assert connection.execute(
            "SELECT detail FROM events ORDER BY id DESC LIMIT 1"
        ).fetchone() == (
            "event-1000",
        )
        assert connection.execute(
            "SELECT seq FROM sqlite_sequence WHERE name='events'"
        ).fetchone() == (
            1002,
        )
