"""Opt-in real-Docker acceptance for the offline Python-v4 cutover boundary.

Run with the documented Nix OpenSSL environment and a pre-pulled pinned Alpine
image:

``BOSN_RUN_LIVE_DOCKER=1 uv run pytest -q tests/test_live_python_v4_cutover_docker.py``

The test creates only names containing its UUID and removes only those exact
objects after rechecking every Bosn and test ownership label.  It never uses a
Docker selector, prune, release command, or fake engine.
"""

from __future__ import annotations

import json
import os
import shutil
import sqlite3
import subprocess
import sys
import uuid
from collections.abc import Iterator
from contextlib import contextmanager
from pathlib import Path

import pytest

FIXTURE = Path(__file__).parent / "fixtures" / "migration" / "create_python_v4_registry.py"
PINNED_ALPINE = "alpine@sha256:28bd5fe8b56d1bd048e5babf5b10710ebe0bae67db86916198a6eec434943f8b"
TEST_LABEL = "com.zackees.bosn.live-v4-cutover-test"
CREATED = "2026-09-14T00:00:00Z"
GENERATION = "sha256:" + "c" * 64
DOCKER_TIMEOUT_SECONDS = 20


def _live_docker_enabled() -> bool:
    return os.environ.get("BOSN_RUN_LIVE_DOCKER") == "1"


pytestmark = [
    pytest.mark.docker,
    pytest.mark.slow,
    pytest.mark.skipif(
        not _live_docker_enabled(),
        reason="set BOSN_RUN_LIVE_DOCKER=1 to run the real Docker cutover acceptance",
    ),
]


def _docker(*args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["docker", *args],
        check=check,
        text=True,
        capture_output=True,
        timeout=DOCKER_TIMEOUT_SECONDS,
    )


def _require_live_docker_and_image() -> None:
    try:
        docker = _docker("version", "--format", "{{.Server.Version}}", check=False)
    except FileNotFoundError:
        pytest.skip("live Docker cutover proof needs a Docker CLI")
    if docker.returncode:
        pytest.skip("live Docker cutover proof needs a reachable Docker daemon")
    image = _docker("image", "inspect", PINNED_ALPINE, check=False)
    if image.returncode:
        pytest.skip("live Docker cutover proof needs the pre-pulled pinned Alpine image")


def _native_cli_command() -> list[str]:
    """Use the normal locked workspace CLI with or without the local wrapper."""
    if soldr := shutil.which("soldr"):
        return [soldr, "cargo"]
    return ["cargo"]


def _native_registry(*arguments: str) -> dict[str, object]:
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
        *arguments,
    ]
    completed = subprocess.run(command, check=True, text=True, capture_output=True, timeout=180)
    return json.loads(completed.stdout.splitlines()[-1])


def _ownership_labels(
    registry_id: str,
    *,
    kind: str,
    workspace: str,
    scope: str,
    retention: str,
    test_id: str,
) -> dict[str, str]:
    return {
        "com.zackees.bosn.registry": registry_id,
        "com.zackees.bosn.kind": kind,
        "com.zackees.bosn.stack": "live-v4-cutover",
        "com.zackees.bosn.generation": GENERATION,
        "com.zackees.bosn.scope": scope,
        "com.zackees.bosn.workspace": workspace,
        "com.zackees.bosn.created": CREATED,
        "com.zackees.bosn.retention": retention,
        TEST_LABEL: test_id,
    }


def _docker_labels(labels: dict[str, str]) -> list[str]:
    return [value for key, item in labels.items() for value in ("--label", f"{key}={item}")]


def _inspect_volume(name: str) -> dict[str, object] | None:
    result = _docker("volume", "inspect", name, check=False)
    if result.returncode:
        assert result.returncode == 1, result.stderr
        return None
    (record,) = json.loads(result.stdout)
    assert record["Name"] == name
    return record


def _inspect_container(name: str) -> dict[str, object] | None:
    result = _docker("container", "inspect", name, check=False)
    if result.returncode:
        assert result.returncode == 1, result.stderr
        return None
    (record,) = json.loads(result.stdout)
    assert record["Name"] == f"/{name}"
    return record


def _assert_our_labels(record: dict[str, object], expected: dict[str, str]) -> None:
    labels = record.get("Labels")
    if labels is None:
        config = record.get("Config")
        assert isinstance(config, dict)
        labels = config.get("Labels")
    assert isinstance(labels, dict)
    assert labels == expected, "cleanup refused a resource with unexpected ownership labels"


def _remove_exact_container(name: str, labels: dict[str, str]) -> None:
    record = _inspect_container(name)
    if record is None:
        return
    _assert_our_labels(record, labels)
    _docker("container", "rm", "--force", name)
    assert _inspect_container(name) is None


def _remove_exact_volume(name: str, labels: dict[str, str]) -> None:
    record = _inspect_volume(name)
    if record is None:
        return
    _assert_our_labels(record, labels)
    _docker("volume", "rm", name)
    assert _inspect_volume(name) is None


def _write_volume(name: str, sentinel: str) -> None:
    _docker(
        "run",
        "--rm",
        "--mount",
        f"type=volume,source={name},target=/fixture",
        PINNED_ALPINE,
        "sh",
        "-ec",
        f"printf '%s' '{sentinel}' > /fixture/bosn-live-cutover-proof",
    )


def _assert_volume_contents(name: str, sentinel: str) -> None:
    result = _docker(
        "run",
        "--rm",
        "--mount",
        f"type=volume,source={name},target=/fixture,readonly",
        PINNED_ALPINE,
        "cat",
        "/fixture/bosn-live-cutover-proof",
    )
    assert result.stdout == sentinel


@contextmanager
def _protected_live_fixture(
    registry_id: str, workspace_a: str, workspace_b: str, test_id: str
) -> Iterator[tuple[str, str, str, dict[str, str], dict[str, str], dict[str, str]]]:
    """Create and clean only exact test-owned resources, even after assertion failures."""
    suffix = test_id.replace("-", "")
    pinned_volume = f"bosn-live-v4-pinned-{suffix}"
    shared_volume = f"bosn-live-v4-shared-{suffix}"
    container = f"bosn-live-v4-container-{suffix}"
    pinned_labels = _ownership_labels(
        registry_id,
        kind="volume",
        workspace=workspace_a,
        scope="machine",
        retention="pinned",
        test_id=test_id,
    )
    shared_labels = _ownership_labels(
        registry_id,
        kind="volume",
        workspace=workspace_a,
        scope="machine",
        retention="warm",
        test_id=test_id,
    )
    container_labels = _ownership_labels(
        registry_id,
        kind="container",
        workspace=workspace_a,
        scope="stack",
        retention="pinned",
        test_id=test_id,
    )
    created_volumes: list[tuple[str, dict[str, str]]] = []
    container_created = False
    try:
        for name, labels in ((pinned_volume, pinned_labels), (shared_volume, shared_labels)):
            assert _inspect_volume(name) is None, "refusing an existing deterministic test volume"
            result = _docker("volume", "create", *_docker_labels(labels), name)
            assert result.stdout.strip() == name
            created_volumes.append((name, labels))
        _write_volume(pinned_volume, f"pinned:{test_id}")
        _write_volume(shared_volume, f"shared:{test_id}")
        assert _inspect_container(container) is None, (
            "refusing an existing deterministic test container"
        )
        _docker(
            "container",
            "create",
            "--name",
            container,
            *_docker_labels(container_labels),
            "--mount",
            f"type=volume,source={pinned_volume},target=/fixture/pinned",
            "--mount",
            f"type=volume,source={shared_volume},target=/fixture/shared",
            PINNED_ALPINE,
            "sh",
            "-c",
            "exec sleep 300",
        )
        container_created = True
        yield (
            pinned_volume,
            shared_volume,
            container,
            pinned_labels,
            shared_labels,
            container_labels,
        )
    finally:
        if container_created:
            _remove_exact_container(container, container_labels)
        for name, labels in reversed(created_volumes):
            _remove_exact_volume(name, labels)


def _create_quiesced_v4_registry(
    legacy: Path,
    *,
    registry_id: str,
    workspace_a: str,
    workspace_b: str,
    pinned_volume: str,
    shared_volume: str,
    container: str,
) -> bytes:
    """Reuse the fixture's exact v4 schema, but bind its rows to real Docker names."""
    source = legacy / "registry.sqlite3"
    subprocess.run([sys.executable, str(FIXTURE), str(source)], check=True)
    with sqlite3.connect(source) as connection:
        connection.execute("PRAGMA foreign_keys=ON")
        for table in (
            "execution_sessions",
            "leases",
            "volume_creation_intents",
            "resource_uses",
            "resources",
            "generations",
            "events",
        ):
            connection.execute(f"DELETE FROM {table}")
        connection.execute("UPDATE meta SET value=? WHERE key='registry_id'", (registry_id,))
        resources = [
            ("volume-pinned", "volume", pinned_volume, "machine", workspace_a, "pinned"),
            ("volume-shared", "volume", shared_volume, "machine", workspace_a, "warm"),
            ("container-protected", "container", container, "stack", workspace_a, "pinned"),
        ]
        connection.executemany(
            "INSERT INTO resources VALUES (?, ?, ?, 'live-v4-cutover', ?, ?, ?, 1000, 1001, "
            "'active', ?)",
            [
                (item[0], item[1], item[2], GENERATION, item[3], item[4], item[5])
                for item in resources
            ],
        )
        connection.executemany(
            "INSERT INTO resource_uses VALUES (?, ?, 'live-v4-cutover', ?, 1001, 'active')",
            [
                ("volume-pinned", workspace_a, GENERATION),
                ("volume-shared", workspace_a, GENERATION),
                ("volume-shared", workspace_b, GENERATION),
                ("container-protected", workspace_a, GENERATION),
            ],
        )
        connection.executemany(
            "INSERT INTO generations VALUES (?, 'live-v4-cutover', ?, 1000, NULL)",
            [(workspace_a, GENERATION), (workspace_b, GENERATION)],
        )
        connection.execute(
            "INSERT INTO events(at, kind, detail) VALUES (1001, 'live.fixture.created', 'quiesced')"
        )
        connection.commit()
        connection.execute("PRAGMA wal_checkpoint(TRUNCATE)")
    marker = legacy / "rust-cutover-v1.json"
    marker.write_text(json.dumps({"protocol": 1, "registry_id": registry_id}), encoding="utf-8")
    os.chmod(marker, 0o600)
    return source.read_bytes()


def _read_reconciled_rows(state_dir: Path) -> tuple[str | None, list[tuple[str, str, str]], int]:
    database = (state_dir / "registry.sqlite3").resolve().as_uri() + "?mode=ro"
    with sqlite3.connect(database, uri=True) as connection:
        gate = connection.execute(
            "SELECT value FROM meta WHERE key='migration.reconciliation_required'"
        ).fetchone()
        resources = connection.execute(
            "SELECT id,state,retention FROM resources ORDER BY id"
        ).fetchall()
        proofs = connection.execute(
            "SELECT COUNT(*) FROM events WHERE kind='migration.reconcile.verified'"
        ).fetchone()[0]
    return (None if gate is None else gate[0], resources, proofs)


def test_live_docker_native_python_v4_cutover_reconciles_without_lifecycle_mutation(
    tmp_path: Path,
) -> None:
    """The real native commands retain protected pinned and shared Docker fixtures."""
    _require_live_docker_and_image()
    legacy = tmp_path / "legacy"
    state = tmp_path / "native"
    legacy.mkdir(mode=0o700)
    state.mkdir(mode=0o700)
    os.chmod(legacy, 0o700)
    os.chmod(state, 0o700)
    registry_id = str(uuid.uuid4())
    test_id = str(uuid.uuid4())
    workspace_a = str((tmp_path / "workspace-a").resolve())
    workspace_b = str((tmp_path / "workspace-b").resolve())

    with _protected_live_fixture(registry_id, workspace_a, workspace_b, test_id) as fixture:
        (
            pinned_volume,
            shared_volume,
            container,
            pinned_labels,
            shared_labels,
            container_labels,
        ) = fixture
        source_before = _create_quiesced_v4_registry(
            legacy,
            registry_id=registry_id,
            workspace_a=workspace_a,
            workspace_b=workspace_b,
            pinned_volume=pinned_volume,
            shared_volume=shared_volume,
            container=container,
        )
        pinned_before = _inspect_volume(pinned_volume)
        shared_before = _inspect_volume(shared_volume)
        container_before = _inspect_container(container)
        assert pinned_before is not None
        assert shared_before is not None
        assert container_before is not None
        _assert_our_labels(pinned_before, pinned_labels)
        _assert_our_labels(shared_before, shared_labels)
        _assert_our_labels(container_before, container_labels)

        imported = _native_registry(
            "import-v4",
            "--legacy-state-dir",
            str(legacy),
            "--state-dir",
            str(state),
            "--yes",
            "--json",
        )
        assert imported["action"] == "registry_import_v4"
        assert imported["registry_id"] == registry_id
        assert imported["reconciliation_required"] is True
        assert imported["source_preserved"] is True
        assert (legacy / "registry.sqlite3").read_bytes() == source_before

        preview = _native_registry("reconcile-v4", "preview", "--state-dir", str(state), "--json")
        assert preview == {
            "action": "registry_reconcile_v4_preview",
            "preview_only": True,
            "verified": ["container-protected", "volume-pinned", "volume-shared"],
            "refusals": [],
            "reconciliation_cleared": False,
        }
        applied = _native_registry(
            "reconcile-v4",
            "apply",
            "--state-dir",
            str(state),
            "--apply",
            "--yes",
            "--json",
        )
        assert applied == {
            "action": "registry_reconcile_v4_apply",
            "preview_only": False,
            "verified": ["container-protected", "volume-pinned", "volume-shared"],
            "refusals": [],
            "reconciliation_cleared": True,
        }
        gate, resources, proof_count = _read_reconciled_rows(state)
        assert gate is None
        assert resources == [
            ("container-protected", "active", "pinned"),
            ("volume-pinned", "active", "pinned"),
            ("volume-shared", "active", "warm"),
        ]
        assert proof_count == 3

        assert _inspect_volume(pinned_volume) == pinned_before
        assert _inspect_volume(shared_volume) == shared_before
        assert _inspect_container(container) == container_before
        _assert_volume_contents(pinned_volume, f"pinned:{test_id}")
        _assert_volume_contents(shared_volume, f"shared:{test_id}")
