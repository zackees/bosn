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


def test_native_python_client_ensures_supported_compose_yaml_source(tmp_path: Path) -> None:
    """A lossless one-service Compose YAML source uses the normal setup lifecycle.

    The test deliberately speaks only the typed Python setup API.  It neither
    invokes nor requires a Compose executable, and the YAML carries no raw
    Docker arguments: the fixed setup engine receives only the translated
    pinned-image, environment, and fixed-shell command semantics.
    """

    image_id = _pinned_image_id()
    assert bosn.Client.__module__ == "bosn._native"

    state_dir = tmp_path / "state"
    workspace = tmp_path / "workspace"
    config_dir = tmp_path / "config"
    workspace.mkdir()
    config_dir.mkdir()
    config = config_dir / "compose.yaml"
    unique = f"python-compose-{os.getpid()}-{time.time_ns()}"
    config.write_text(
        "services:\n"
        "  app:\n"
        f"    image: {PINNED_ALPINE}\n"
        "    environment:\n"
        f"      BOSN_COMPOSE_PROOF: {unique}\n"
        f"    command: [sh, -lc, 'exec sleep 120 # {unique}']\n",
        encoding="utf-8",
    )

    client = bosn.Client(state_dir)
    plan = client.plan_setup(workspace, str(config), policy="online_refresh")
    assert plan.source_kind == "local_file"
    assert plan.app_source_kind == "pinned_image"
    assert plan.image == PINNED_ALPINE
    assert plan.asset_root is None
    assert plan.task_names == ()
    assert not plan.applied
    container_name = f"bosn-setup-{plan.content_sha256}"
    assert _inspect_container(container_name) is None, "refusing an existing deterministic app"

    try:
        with _production_daemon(state_dir) as (_, daemon):
            _wait_for_daemon(client, daemon)
            first_job = client.submit_setup_ensure(
                workspace,
                str(config),
                policy="online_refresh",
                deadline_ms=90_000,
                output_limit=1_048_576,
            )
            assert isinstance(_wait_for_success(client, first_job), tuple)

            first = _inspect_container(container_name)
            assert first is not None
            first_id, first_running, first_image, first_labels = first
            assert first_running
            assert first_image == image_id
            assert first_labels[MANAGED_LABEL] == "v1"
            assert first_labels[CONTENT_LABEL] == plan.content_sha256
            assert first_labels[NAME_LABEL] == container_name

            # The daemon owns both durable resource records.  The image record
            # is tied to Docker's inspected immutable identity rather than the
            # Compose source reference, while the container record is tied to
            # the verified YAML-content generation.
            resources = client.registry_resources(limit=16).records
            assert len(resources) == 2
            container_resource = next(
                record
                for record in resources
                if record.id == f"setup-container:{plan.content_sha256}"
            )
            assert (
                container_resource.kind,
                container_resource.name,
                container_resource.stack,
                container_resource.generation,
                container_resource.state,
                container_resource.retention,
            ) == (
                "container",
                container_name,
                "setup",
                f"sha256:{plan.content_sha256}",
                "active",
                "pinned",
            )
            image_resource = next(record for record in resources if record.kind == "image")
            assert (
                image_resource.id,
                image_resource.name,
                image_resource.stack,
                image_resource.generation,
                image_resource.state,
                image_resource.retention,
            ) == (
                f"setup-image:{image_id}",
                f"setup-image:{image_id}",
                "setup",
                image_id,
                "active",
                "pinned",
            )
            assert [event.kind for event in client.setup_ensure_events(limit=8).records] == [
                "setup.ensure.succeeded",
                "setup.ensure.submitted",
            ]

            # Reuse goes through the same YAML source and typed API; it does
            # not run a Compose command or replace the verified container.
            second_job = client.submit_setup_ensure(
                workspace,
                str(config),
                policy="offline_cache_only",
                deadline_ms=90_000,
                output_limit=1_048_576,
            )
            assert isinstance(_wait_for_success(client, second_job), tuple)
            second = _inspect_container(container_name)
            assert second is not None
            second_id, second_running, second_image, second_labels = second
            assert second_id == first_id, "Compose YAML ensure replaced the matching app"
            assert second_running
            assert second_image == first_image
            assert second_labels == first_labels
            assert client.status().resources == 2

        assert not any(workspace.iterdir()), "ensure wrote into the selected workspace"
    finally:
        _remove_exact_managed_container(container_name, plan.content_sha256)


def test_native_python_client_runs_declared_task_inside_ensured_managed_app(
    tmp_path: Path,
) -> None:
    """A declared app task uses the ensured app, never an ephemeral task container.

    The managed app receives Docker's default hostname, so a document-derived
    task can persist that value in its declared writable workspace mount.  It
    must equal the already inspected managed app's hostname after the task
    finishes.  That observation distinguishes ``docker container exec`` from
    the separate ephemeral ``docker run --rm`` setup-task operation without
    exposing a raw container or command input to the Python caller.
    """

    image_id = _pinned_image_id()
    assert bosn.Client.__module__ == "bosn._native"

    state_dir = tmp_path / "state"
    workspace = tmp_path / "workspace"
    config_dir = tmp_path / "config"
    workspace.mkdir()
    config_dir.mkdir()
    config = config_dir / "setup.toml"
    unique = f"python-app-task-{os.getpid()}-{time.time_ns()}"
    stdout_marker = f"bosn-app-task-stdout-{unique}"
    stderr_marker = f"bosn-app-task-stderr-{unique}"
    config.write_text(
        "version = 1\n"
        "[app]\n"
        f"image = '{PINNED_ALPINE}'\n"
        f"command = 'exec sleep 120 # {unique}'\n"
        "[[app.mount]]\n"
        "source = '.'\n"
        "target = '/workspace'\n"
        "readonly = false\n"
        "[task.prove]\n"
        "command = '''hostname > /workspace/app-task-hostname.txt\n"
        f"printf '%s\\n' '{unique}' > /workspace/app-task-proof.txt\n"
        f"printf '%s\\n' '{stdout_marker}'\n"
        f"printf '%s\\n' '{stderr_marker}' >&2\n"
        "'''\n",
        encoding="utf-8",
    )

    client = bosn.Client(state_dir)
    plan = client.plan_setup(workspace, str(config), policy="online_refresh")
    container_name = f"bosn-setup-{plan.content_sha256}"
    assert _inspect_container(container_name) is None, "refusing an existing deterministic app"

    # The public native method takes precisely the semantic task selection and
    # bounds.  It cannot be turned into a raw docker exec operation.
    for field in ("container", "command", "docker_args", "mounts"):
        with pytest.raises(TypeError):
            client.submit_setup_app_task(
                workspace,
                str(config),
                policy="online_refresh",
                task_name="prove",
                deadline_ms=90_000,
                output_limit=1_048_576,
                **{field: "unsafe"},
            )

    try:
        with _production_daemon(state_dir) as (_, daemon):
            _wait_for_daemon(client, daemon)
            ensure_job = client.submit_setup_ensure(
                workspace,
                str(config),
                policy="online_refresh",
                deadline_ms=90_000,
                output_limit=1_048_576,
            )
            assert isinstance(_wait_for_success(client, ensure_job), tuple)

            before = _inspect_container(container_name)
            assert before is not None
            before_id, before_running, before_image, before_labels = before
            assert before_running
            assert before_image == image_id
            assert before_labels == {
                MANAGED_LABEL: "v1",
                CONTENT_LABEL: plan.content_sha256,
                NAME_LABEL: container_name,
            }
            expected_hostname = _docker(
                "container", "inspect", "--format", "{{.Config.Hostname}}", container_name
            ).stdout.strip()
            assert expected_hostname

            app_task_job = client.submit_setup_app_task(
                workspace,
                str(config),
                policy="offline_cache_only",
                task_name="prove",
                deadline_ms=90_000,
                output_limit=1_048_576,
            )
            logs = _wait_for_success(client, app_task_job)
            assert len(logs) <= 64, "fresh app-task logs exceeded the requested public page"
            assert all(len(line) <= 8_192 for line in logs), "app-task log record was unbounded"
            assert "[setup-app-task] verifying application image" in logs
            assert "[setup-app-task] proving exact managed application ownership" in logs
            assert "[setup-app-task] running declared task prove" in logs
            assert any(stdout_marker in line for line in logs)
            assert any(stderr_marker in line for line in logs)
            assert any(
                f"completed declared app task prove in managed container {container_name}" in line
                for line in logs
            )

            assert (workspace / "app-task-proof.txt").read_text() == f"{unique}\n"
            assert (workspace / "app-task-hostname.txt").read_text().strip() == expected_hostname

            after = _inspect_container(container_name)
            assert after is not None
            after_id, after_running, after_image, after_labels = after
            assert after_id == before_id, "app task replaced the ensured managed app"
            assert after_running
            assert after_image == before_image
            assert after_labels == before_labels
            assert client.status().sessions == 0, "known app-task completion retained a session"
    finally:
        _remove_exact_managed_container(container_name, plan.content_sha256)
