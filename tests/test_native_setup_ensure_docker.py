"""Opt-in live-Docker proof for the installed PyO3 setup-ensure boundary.

The Python client submits and observes the job exclusively through the public
native extension.  Docker appears here only as an external verifier and to
remove the one container whose complete Bosn ownership tuple is rechecked.
"""

from __future__ import annotations

import json
import os
import subprocess
import time
from collections.abc import Iterator
from contextlib import contextmanager
from pathlib import Path

import pytest

import bosn
from bosn.native_cli import _configure_native_library_path, native_executable

PINNED_ALPINE = (
    "alpine@sha256:28bd5fe8b56d1bd048e5babf5b10710ebe0bae67db86916198a6eec434943f8b"
)
MANAGED_LABEL = "com.zackees.bosn.setup-managed"
CONTENT_LABEL = "com.zackees.bosn.setup-content-sha256"
NAME_LABEL = "com.zackees.bosn.setup-container"
READY_DEADLINE_SECONDS = 10
JOB_DEADLINE_SECONDS = 90


def _live_docker_enabled() -> bool:
    return os.environ.get("BOSN_RUN_LIVE_DOCKER") == "1"


pytestmark = [
    pytest.mark.docker,
    pytest.mark.slow,
    pytest.mark.skipif(
        not _live_docker_enabled(),
        reason="set BOSN_RUN_LIVE_DOCKER=1 to run the real Docker acceptance",
    ),
]


def _docker(*args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["docker", *args],
        check=check,
        text=True,
        capture_output=True,
        timeout=15,
    )


def _pinned_image_id() -> str:
    result = _docker("image", "inspect", "--format", "{{.Id}}", PINNED_ALPINE, check=False)
    if result.returncode:
        pytest.skip(
            "live Docker proof needs the pre-pulled pinned Alpine image " + PINNED_ALPINE
        )
    image_id = result.stdout.strip()
    assert image_id, "pinned Alpine image did not expose an image identity"
    return image_id


def _inspect_container(name: str) -> tuple[str, bool, str, dict[str, str]] | None:
    result = _docker("container", "inspect", name, check=False)
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


@contextmanager
def _production_daemon(
    state_dir: Path,
) -> Iterator[tuple[Path, subprocess.Popen[bytes]]]:
    """Start exactly the package-local production daemon, not a Cargo binary."""

    executable = native_executable()
    assert executable.is_file()
    # The native wheel launcher configures the package-local bundled libraries
    # before exec. Apply that same package helper to this direct daemon child;
    # it does not select PATH or any checkout binary.
    _configure_native_library_path(executable)
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


def _wait_for_daemon(client: bosn.Client, daemon: subprocess.Popen[bytes]) -> None:
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


def _wait_for_success(client: bosn.Client, job_id: int) -> tuple[str, ...]:
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


def _remove_exact_managed_container(name: str, content_sha256: str) -> None:
    observed = _inspect_container(name)
    if observed is None:
        return
    _, _, _, labels = observed
    assert labels.get(MANAGED_LABEL) == "v1", "cleanup refused an unmanaged container"
    assert labels.get(CONTENT_LABEL) == content_sha256, "cleanup refused another config"
    assert labels.get(NAME_LABEL) == name, "cleanup refused another container name"
    _docker("container", "rm", "--force", name)
    assert _inspect_container(name) is None


def test_native_python_client_ensures_and_reuses_one_managed_app(tmp_path: Path) -> None:
    """One-file setup ensure stays semantic across two daemon processes."""

    _pinned_image_id()
    assert bosn.Client.__module__ == "bosn._native"

    state_dir = tmp_path / "state"
    workspace = tmp_path / "workspace"
    config_dir = tmp_path / "config"
    workspace.mkdir()
    config_dir.mkdir()
    config = config_dir / "setup.toml"
    unique = f"python-native-{os.getpid()}-{time.time_ns()}"
    config.write_text(
        "version = 1\n"
        "[app]\n"
        f"image = '{PINNED_ALPINE}'\n"
        f"command = 'exec sleep 120 # {unique}'\n"
    )

    client = bosn.Client(state_dir)
    # A caller cannot submit raw engine control through this Python signature.
    for field in ("docker_args", "command", "mounts"):
        with pytest.raises(TypeError):
            client.submit_setup_ensure(
                workspace,
                str(config),
                policy="online_refresh",
                deadline_ms=90_000,
                output_limit=1_048_576,
                **{field: ["--privileged"]},
            )

    plan = client.plan_setup(workspace, str(config), policy="online_refresh")
    assert plan.source_kind == "local_file"
    assert plan.app_source_kind == "pinned_image"
    assert plan.image == PINNED_ALPINE
    container_name = f"bosn-setup-{plan.content_sha256}"
    assert _inspect_container(container_name) is None, "refusing an existing deterministic app"

    try:
        with _production_daemon(state_dir) as (_, first_daemon):
            _wait_for_daemon(client, first_daemon)
            first_job = client.submit_setup_ensure(
                workspace,
                str(config),
                policy="online_refresh",
                deadline_ms=90_000,
                output_limit=1_048_576,
            )
            first_logs = _wait_for_success(client, first_job)
            assert isinstance(first_logs, tuple)
            first = _inspect_container(container_name)
            assert first is not None
            first_id, first_running, first_image, first_labels = first
            assert first_running
            assert first_image == _pinned_image_id()
            assert first_labels[MANAGED_LABEL] == "v1"
            assert first_labels[CONTENT_LABEL] == plan.content_sha256
            assert first_labels[NAME_LABEL] == container_name

        # A distinct production daemon and a fresh Python binding client must
        # discover and reuse the matching app rather than replacing it.
        second_client = bosn.Client(state_dir)
        with _production_daemon(state_dir) as (_, second_daemon):
            _wait_for_daemon(second_client, second_daemon)
            second_job = second_client.submit_setup_ensure(
                workspace,
                str(config),
                policy="offline_cache_only",
                deadline_ms=90_000,
                output_limit=1_048_576,
            )
            second_logs = _wait_for_success(second_client, second_job)
            assert isinstance(second_logs, tuple)
            second = _inspect_container(container_name)
            assert second is not None
            second_id, second_running, second_image, second_labels = second
            assert second_id == first_id, "Python ensure replaced the matching app"
            assert second_running
            assert second_image == first_image
            assert second_labels == first_labels

        assert not any(workspace.iterdir()), "ensure wrote into the selected workspace"
    finally:
        _remove_exact_managed_container(container_name, plan.content_sha256)
