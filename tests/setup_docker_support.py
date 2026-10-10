"""Live-Docker helpers shared by the native setup/manifest ensure tests."""

from __future__ import annotations

import json
import os
import sqlite3
import subprocess
import time
from collections.abc import Iterator
from contextlib import contextmanager
from pathlib import Path

import pytest

import bosn
from native_binary import native_binary

PINNED_ALPINE = "alpine@sha256:28bd5fe8b56d1bd048e5babf5b10710ebe0bae67db86916198a6eec434943f8b"
# A Linux manifest stack runs a fixed daemon-owned idle PID 1 (declared tasks
# run through exec), so any base image works; the manifest surface still has
# no command escape hatch. MySQL is kept as a realistic non-trivial image.
PINNED_MANIFEST_MYSQL = (
    "mysql@sha256:7dcddc01f13bab2f15cde676d44d01f61fc9f99fe7785e86196dfc07d358ae2b"
)
MANAGED_LABEL = "com.zackees.bosn.setup-managed"
CONTENT_LABEL = "com.zackees.bosn.setup-content-sha256"
NAME_LABEL = "com.zackees.bosn.setup-container"
READY_DEADLINE_SECONDS = 10
JOB_DEADLINE_SECONDS = 90


def live_docker_enabled() -> bool:
    return os.environ.get("BOSN_RUN_LIVE_DOCKER") == "1"


def docker(*args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["docker", *args],
        check=check,
        text=True,
        capture_output=True,
        timeout=15,
    )


def pinned_image_id(image: str = PINNED_ALPINE) -> str:
    result = docker("image", "inspect", "--format", "{{.Id}}", image, check=False)
    if result.returncode:
        pytest.skip("live Docker proof needs the pre-pulled pinned image " + image)
    image_id = result.stdout.strip()
    assert image_id, "pinned Alpine image did not expose an image identity"
    return image_id


def inspect_container(name: str) -> tuple[str, bool, str, dict[str, str]] | None:
    result = docker("container", "inspect", name, check=False)
    if result.returncode:
        assert result.returncode == 1, result.stderr
        return None
    (record,) = json.loads(result.stdout)
    return (
        record["Id"],
        bool(record["State"]["Running"]),
        record["Image"],
        record["Config"].get("Labels") or {},
    )


def container_environment(name: str) -> set[str]:
    result = docker("container", "inspect", "--format", "{{json .Config.Env}}", name)
    values = json.loads(result.stdout)
    assert isinstance(values, list)
    assert all(isinstance(value, str) for value in values)
    return set(values)


@contextmanager
def production_daemon(
    state_dir: Path,
) -> Iterator[tuple[Path, subprocess.Popen[bytes]]]:
    """Start exactly the package-local production daemon, not a Cargo binary."""

    executable = native_binary()
    assert executable.is_file()
    # The installed binary links OpenSSL statically, so it runs with no
    # launcher configuring the library path.
    daemon = subprocess.Popen(
        [str(executable), "daemon", "serve", "--state-dir", str(state_dir)],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    try:
        yield executable, daemon
    finally:
        if daemon.poll() is None:
            # Use the production daemon's authenticated shutdown path. This is
            # daemon lifecycle only: setup submission and observation in this
            # test remain exclusively on the Python native Client boundary.
            stopped = subprocess.run(
                [str(executable), "daemon", "stop", "--state-dir", str(state_dir)],
                stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                timeout=10,
            )
            try:
                daemon.wait(timeout=10)
            except subprocess.TimeoutExpired:
                daemon.kill()
                daemon.wait(timeout=10)
            assert stopped.returncode == 0, "production daemon stop command failed"


def wait_for_daemon(client: bosn.Client, daemon: subprocess.Popen[bytes]) -> None:
    deadline = time.monotonic() + READY_DEADLINE_SECONDS
    last_error: RuntimeError | None = None
    while time.monotonic() < deadline:
        assert daemon.poll() is None, "production daemon exited before becoming ready"
        try:
            client.status()
        except RuntimeError as error:
            last_error = error
            time.sleep(0.05)
        else:
            return
    raise AssertionError(f"production daemon did not become ready: {last_error}")


def wait_for_success(client: bosn.Client, job_id: int) -> tuple[str, ...]:
    """Observe status and bounded logs only through the Python native API."""

    deadline = time.monotonic() + JOB_DEADLINE_SECONDS
    after = 0
    lines: list[str] = []
    while time.monotonic() < deadline:
        page = client.job_logs(job_id, after=after, limit=64)
        assert not page.gap, "a fresh job cannot have an evicted-log gap"
        assert 0 <= page.retained_from <= page.next
        assert len(page.records) <= 64
        lines.extend(record.line for record in page.records)
        after = page.next

        status = client.job_status(job_id)
        assert status.id == job_id
        if status.state == "Succeeded":
            return tuple(lines)
        if status.state in {"Failed", "Cancelled", "Superseded"}:
            raise AssertionError(f"setup ensure ended {status.state}: {status.error}; logs={lines}")
        time.sleep(0.05)
    raise AssertionError(f"setup ensure did not finish; logs={lines}")


def wait_for_failure(client: bosn.Client, job_id: int) -> str:
    """Wait only for the expected pre-engine refusal path."""

    deadline = time.monotonic() + JOB_DEADLINE_SECONDS
    while time.monotonic() < deadline:
        status = client.job_status(job_id)
        assert status.id == job_id
        if status.state == "Failed":
            assert status.error
            return status.error
        if status.state in {"Succeeded", "Cancelled", "Superseded"}:
            raise AssertionError(f"manifest refusal ended {status.state}: {status.error}")
        time.sleep(0.05)
    raise AssertionError("manifest refusal did not finish")


def remove_exact_managed_container(name: str, content_sha256: str) -> None:
    observed = inspect_container(name)
    if observed is None:
        return
    _, _, _, labels = observed
    assert labels.get(MANAGED_LABEL) == "v1", "cleanup refused an unmanaged container"
    assert labels.get(CONTENT_LABEL) == content_sha256, "cleanup refused another config"
    assert labels.get(NAME_LABEL) == name, "cleanup refused another container name"
    docker("container", "rm", "--force", name)
    assert inspect_container(name) is None


def remove_exact_managed_volume(name: str, content_sha256: str) -> None:
    result = docker("volume", "inspect", name, check=False)
    if result.returncode:
        assert result.returncode == 1, result.stderr
        return
    (record,) = json.loads(result.stdout)
    labels = record.get("Labels") or {}
    assert labels.get(MANAGED_LABEL) == "v1", "cleanup refused an unmanaged volume"
    assert labels.get(CONTENT_LABEL) == content_sha256, "cleanup refused another volume"
    assert labels.get(NAME_LABEL) == name, "cleanup refused another volume name"
    docker("volume", "rm", name)


def read_exact_resource_uses(state_dir: Path) -> set[tuple[str, str, str, str, str]]:
    """Read only the durable use facts after the daemon committed the job.

    Resource-use diagnostics do not yet have a public Python accessor.  This
    verifier opens the daemon-created SQLite file read-only and queries no
    mutable operation, keeping all lifecycle calls on the public Client.
    """

    database = (state_dir / "registry.sqlite3").resolve().as_uri() + "?mode=ro"
    connection = sqlite3.connect(database, uri=True)
    try:
        return set(
            connection.execute(
                "SELECT resource_id, workspace, stack, generation, state FROM resource_uses"
            ).fetchall()
        )
    finally:
        connection.close()


def read_manifest_success_events(state_dir: Path) -> list[tuple[str, str]]:
    """Read the manifest-specific durable audit facts without mutating state."""

    database = (state_dir / "registry.sqlite3").resolve().as_uri() + "?mode=ro"
    connection = sqlite3.connect(database, uri=True)
    try:
        return connection.execute(
            "SELECT kind, detail FROM events WHERE kind = 'manifest.ensure.succeeded' ORDER BY id"
        ).fetchall()
    finally:
        connection.close()
