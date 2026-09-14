"""Opt-in live-Docker proof for the installed PyO3 setup-ensure boundary.

The Python client submits and observes the job exclusively through the public
native extension.  Docker appears here only as an external verifier and to
remove the one container whose complete Bosn ownership tuple is rechecked.
"""

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
from bosn.native_cli import _configure_native_library_path, native_executable

PINNED_ALPINE = "alpine@sha256:28bd5fe8b56d1bd048e5babf5b10710ebe0bae67db86916198a6eec434943f8b"
# The legacy-manifest runtime intentionally derives no application command.
# Unlike Alpine's interactive shell default, MySQL's image-declared server is
# long-running, so it proves the daemon has actually started a standard Linux
# application without adding a command escape hatch to the manifest surface.
PINNED_MANIFEST_MYSQL = (
    "mysql@sha256:7dcddc01f13bab2f15cde676d44d01f61fc9f99fe7785e86196dfc07d358ae2b"
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


def _pinned_image_id(image: str = PINNED_ALPINE) -> str:
    result = _docker("image", "inspect", "--format", "{{.Id}}", image, check=False)
    if result.returncode:
        pytest.skip("live Docker proof needs the pre-pulled pinned image " + image)
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


def _container_environment(name: str) -> set[str]:
    result = _docker("container", "inspect", "--format", "{{json .Config.Env}}", name)
    values = json.loads(result.stdout)
    assert isinstance(values, list)
    assert all(isinstance(value, str) for value in values)
    return set(values)


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


def _wait_for_failure(client: bosn.Client, job_id: int) -> str:
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


def _remove_exact_managed_volume(name: str, content_sha256: str) -> None:
    result = _docker("volume", "inspect", name, check=False)
    if result.returncode:
        assert result.returncode == 1, result.stderr
        return
    (record,) = json.loads(result.stdout)
    labels = record.get("Labels") or {}
    assert labels.get(MANAGED_LABEL) == "v1", "cleanup refused an unmanaged volume"
    assert labels.get(CONTENT_LABEL) == content_sha256, "cleanup refused another volume"
    assert labels.get(NAME_LABEL) == name, "cleanup refused another volume name"
    _docker("volume", "rm", name)


def _read_exact_resource_uses(state_dir: Path) -> set[tuple[str, str, str, str, str]]:
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


def _read_manifest_success_events(state_dir: Path) -> list[tuple[str, str]]:
    """Read the manifest-specific durable audit facts without mutating state."""

    database = (state_dir / "registry.sqlite3").resolve().as_uri() + "?mode=ro"
    connection = sqlite3.connect(database, uri=True)
    try:
        return connection.execute(
            "SELECT kind, detail FROM events WHERE kind = 'manifest.ensure.succeeded' ORDER BY id"
        ).fetchall()
    finally:
        connection.close()


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
        f"version = 1\n[app]\nimage = '{PINNED_ALPINE}'\ncommand = 'exec sleep 120 # {unique}'\n"
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


def test_native_python_client_ensures_and_reuses_manifest_stack(tmp_path: Path) -> None:
    """A manifest stack becomes one verified, daemon-owned Linux application.

    This covers the Rust manifest bridge's supported runtime subset through
    the installed PyO3 Client.  It deliberately has no raw image, container,
    Docker, or command argument: image and environment are declaration data
    in the workspace-contained ``bosn.toml`` only.
    """

    image_id = _pinned_image_id(PINNED_MANIFEST_MYSQL)
    assert bosn.Client.__module__ == "bosn._native"

    state_dir = tmp_path / "state"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    unique = f"python-manifest-{os.getpid()}-{time.time_ns()}"
    manifest = workspace / "bosn.toml"
    manifest.write_text(
        "[stack.linux]\n"
        f"image = '{PINNED_MANIFEST_MYSQL}'\n"
        "[stack.linux.env]\n"
        "MYSQL_ALLOW_EMPTY_PASSWORD = 'yes'\n"
        f"BOSN_MANIFEST_PROOF = '{unique}'\n",
        encoding="utf-8",
    )
    with manifest.open("a", encoding="utf-8") as handle:
        handle.write(
            "[stack.linux.volumes.proof]\n"
            "scope = 'stack'\n"
            "destination = '/var/lib/bosn-proof'\n"
            "retention = 'pinned'\n"
            "[task.seed-volume]\n"
            "stack = 'linux'\n"
            f"cmd = \"mkdir -p /var/lib/bosn-proof && printf %s '{unique}' "
            '> /var/lib/bosn-proof/marker"\n'
            "[task.prove-volume]\n"
            "stack = 'linux'\n"
            f'cmd = "test \\"$(cat /var/lib/bosn-proof/marker)\\" = \'{unique}\'"\n'
            "[task.prove]\n"
            "stack = 'linux'\n"
            f'cmd = "test \\"$BOSN_MANIFEST_PROOF\\" = \'{unique}\'"\n'
        )
    unsupported = workspace / "unsupported.toml"
    unsupported.write_text(
        f"[stack.rejected]\nimage = '{PINNED_MANIFEST_MYSQL}'\nworkdir = '/'\n",
        encoding="utf-8",
    )

    client = bosn.Client(state_dir)
    # The native boundary exposes exactly the semantic selectors and bounds.
    # It must not become a raw Docker/create or command execution endpoint.
    for field in ("image", "container", "docker_args", "command", "mounts"):
        with pytest.raises(TypeError):
            client.submit_manifest_ensure(
                workspace,
                "bosn.toml",
                "linux",
                deadline_ms=90_000,
                output_limit=1_048_576,
                **{field: "unsafe"},
            )
    with pytest.raises(ValueError, match="safe workspace-relative path"):
        client.submit_manifest_ensure(
            workspace,
            "../bosn.toml",
            "linux",
            deadline_ms=90_000,
            output_limit=1_048_576,
        )

    managed_containers: list[tuple[str, str]] = []
    managed_volumes: list[tuple[str, str]] = []
    try:
        with _production_daemon(state_dir) as (_, daemon):
            _wait_for_daemon(client, daemon)

            # An image-only workdir remains outside the supported typed shape
            # and is refused before it reaches Docker.
            rejected = client.submit_manifest_ensure(
                workspace,
                "unsupported.toml",
                "rejected",
                deadline_ms=90_000,
                output_limit=1_048_576,
            )
            assert "not covered by a declared workspace mount" in _wait_for_failure(
                client, rejected
            )

            first_job = client.submit_manifest_ensure(
                workspace,
                "bosn.toml",
                "linux",
                deadline_ms=90_000,
                output_limit=1_048_576,
            )
            first_logs = _wait_for_success(client, first_job)
            assert "[manifest] preparing immutable application image" in first_logs

            resources = client.registry_resources(limit=16).records
            assert len(resources) == 3
            container_resource = next(record for record in resources if record.kind == "container")
            image_resource = next(record for record in resources if record.kind == "image")
            volume_resource = next(record for record in resources if record.kind == "volume")
            assert container_resource.stack == "linux"
            assert container_resource.generation.startswith("sha256:")
            assert container_resource.state == "active"
            assert container_resource.retention == "pinned"
            content_sha256 = container_resource.generation.removeprefix("sha256:")
            assert len(content_sha256) == 64
            container_name = f"bosn-setup-{content_sha256}"
            managed_containers.append((container_name, content_sha256))
            assert container_resource.id == (
                f"manifest-container:linux:{container_resource.generation}"
            )
            assert container_resource.name == container_name
            assert volume_resource.stack == "linux"
            assert volume_resource.retention == "pinned"
            volume_content = volume_resource.generation.removeprefix("sha256:")
            managed_volumes.append((volume_resource.name, volume_content))
            assert (
                _docker(
                    "volume",
                    "inspect",
                    "--format",
                    f'{{{{index .Labels "{CONTENT_LABEL}"}}}}',
                    volume_resource.name,
                ).stdout.strip()
                == volume_content
            )
            assert (
                image_resource.id,
                image_resource.name,
                image_resource.stack,
                image_resource.generation,
                image_resource.state,
                image_resource.retention,
            ) == (
                f"manifest-image:{image_id}",
                f"manifest-image:{image_id}",
                "linux",
                image_id,
                "active",
                "pinned",
            )

            first = _inspect_container(container_name)
            assert first is not None
            first_id, first_running, first_image, first_labels = first
            assert first_running
            assert first_image == image_id
            assert first_labels[MANAGED_LABEL] == "v1"
            assert first_labels[CONTENT_LABEL] == content_sha256
            assert first_labels[NAME_LABEL] == container_name
            assert {
                "MYSQL_ALLOW_EMPTY_PASSWORD=yes",
                f"BOSN_MANIFEST_PROOF={unique}",
            } <= _container_environment(container_name)

            expected_uses = {
                (
                    container_resource.id,
                    str(workspace.resolve()),
                    "linux",
                    container_resource.generation,
                    "active",
                ),
                (
                    image_resource.id,
                    str(workspace.resolve()),
                    "linux",
                    image_id,
                    "active",
                ),
                (
                    volume_resource.id,
                    str(workspace.resolve()),
                    "linux",
                    volume_resource.generation,
                    "active",
                ),
            }
            # The durable resources, resource uses, and terminal success event
            # are all committed before the daemon reports the job as succeeded.
            assert _read_exact_resource_uses(state_dir) == expected_uses
            assert _read_manifest_success_events(state_dir) == [
                ("manifest.ensure.succeeded", f"job_id={first_job}")
            ]

            seed_job = client.submit_manifest_app_task(
                workspace,
                "bosn.toml",
                "linux",
                "seed-volume",
                deadline_ms=90_000,
                output_limit=1_048_576,
            )
            assert isinstance(_wait_for_success(client, seed_job), tuple)

            # Reusing the unchanged declaration must keep both the exact app
            # container and the declared Bosn-managed volume intact.
            second_job = client.submit_manifest_ensure(
                workspace,
                "bosn.toml",
                "linux",
                deadline_ms=90_000,
                output_limit=1_048_576,
            )
            assert isinstance(_wait_for_success(client, second_job), tuple)
            second = _inspect_container(container_name)
            assert second is not None
            assert second[0] == first_id
            assert (
                _docker(
                    "volume",
                    "inspect",
                    "--format",
                    f'{{{{index .Labels "{CONTENT_LABEL}"}}}}',
                    volume_resource.name,
                ).stdout.strip()
                == volume_content
            )

            # A changed accepted declaration derives a new manifest generation.
            # The daemon creates/starts the new exact app first, atomically
            # retires only the old durable use, and deliberately leaves the
            # old container running for explicit conservative lifecycle work.
            rollover_unique = f"{unique}-rollover"
            manifest.write_text(
                "[stack.linux]\n"
                f"image = '{PINNED_MANIFEST_MYSQL}'\n"
                "[stack.linux.env]\n"
                "MYSQL_ALLOW_EMPTY_PASSWORD = 'yes'\n"
                f"BOSN_MANIFEST_PROOF = '{rollover_unique}'\n"
                "[stack.linux.volumes.proof]\n"
                "scope = 'stack'\n"
                "destination = '/var/lib/bosn-proof'\n"
                "retention = 'pinned'\n"
                "[task.prove-volume]\n"
                "stack = 'linux'\n"
                f'cmd = "test \\"$(cat /var/lib/bosn-proof/marker)\\" = \'{unique}\'"\n'
                "[task.prove]\n"
                "stack = 'linux'\n"
                f'cmd = "test \\"$BOSN_MANIFEST_PROOF\\" = \'{rollover_unique}\'"\n',
                encoding="utf-8",
            )
            rollover_job = client.submit_manifest_ensure(
                workspace,
                "bosn.toml",
                "linux",
                deadline_ms=90_000,
                output_limit=1_048_576,
            )
            assert isinstance(_wait_for_success(client, rollover_job), tuple)
            rollover_resources = client.registry_resources(limit=16).records
            retired = next(
                record for record in rollover_resources if record.id == container_resource.id
            )
            new_container = next(
                record
                for record in rollover_resources
                if record.kind == "container" and record.state == "active"
            )
            assert retired.state == "retired"
            assert new_container.id != container_resource.id
            rollover_content = new_container.generation.removeprefix("sha256:")
            rollover_name = f"bosn-setup-{rollover_content}"
            managed_containers.append((rollover_name, rollover_content))
            assert _inspect_container(container_name) is not None
            rollover_observed = _inspect_container(rollover_name)
            assert rollover_observed is not None
            assert rollover_observed[1]
            assert rollover_observed[2] == image_id
            assert rollover_observed[3][CONTENT_LABEL] == rollover_content
            volume_proof_job = client.submit_manifest_app_task(
                workspace,
                "bosn.toml",
                "linux",
                "prove-volume",
                deadline_ms=90_000,
                output_limit=1_048_576,
            )
            assert isinstance(_wait_for_success(client, volume_proof_job), tuple)
            rollover_uses = _read_exact_resource_uses(state_dir)
            assert (
                container_resource.id,
                str(workspace.resolve()),
                "linux",
                container_resource.generation,
                "retired",
            ) in rollover_uses
            assert (
                new_container.id,
                str(workspace.resolve()),
                "linux",
                new_container.generation,
                "active",
            ) in rollover_uses
            assert _read_manifest_success_events(state_dir) == [
                ("manifest.ensure.succeeded", f"job_id={first_job}"),
                ("manifest.ensure.succeeded", f"job_id={second_job}"),
                ("manifest.ensure.succeeded", f"job_id={rollover_job}"),
            ]
            # Manifest app-task accepts only the declared task selector. The
            # command above is persisted in bosn.toml and proves `exec` sees
            # the app's declared environment; callers cannot inject a command
            # or target a container directly.
            for field in ("command", "container", "docker_args", "image", "mounts"):
                with pytest.raises(TypeError):
                    client.submit_manifest_app_task(
                        workspace,
                        "bosn.toml",
                        "linux",
                        "prove",
                        deadline_ms=90_000,
                        output_limit=1_048_576,
                        **{field: "unsafe"},
                    )
            task_job = client.submit_manifest_app_task(
                workspace,
                "bosn.toml",
                "linux",
                "prove",
                deadline_ms=90_000,
                output_limit=1_048_576,
            )
            task_logs = _wait_for_success(client, task_job)
            assert "[manifest-app-task] proving exact managed application ownership" in task_logs
            assert "[manifest-app-task] running declared task prove" in task_logs
            assert client.status().sessions == 0
    finally:
        for name, content in reversed(managed_containers):
            _remove_exact_managed_container(name, content)
        for name, content in reversed(managed_volumes):
            _remove_exact_managed_volume(name, content)


def test_native_default_manifest_stack_autostarts_after_daemon_restart(tmp_path: Path) -> None:
    """A fresh installed wheel restarts only a selected manifest app.

    The daemon is deliberately stopped before Docker is asked to stop the
    already-proven exact container. Starting a new bundled daemon must use the
    durable default-stack intent and exact source/registry/label/image proof;
    this test never asks the public API to start a raw container.
    """

    image_id = _pinned_image_id(PINNED_MANIFEST_MYSQL)
    state_dir = tmp_path / "state"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    unique = f"manifest-autostart-{os.getpid()}-{time.time_ns()}"
    (workspace / "bosn.toml").write_text(
        "[stack.app]\n"
        f"image = '{PINNED_MANIFEST_MYSQL}'\n"
        "default = true\n"
        "[stack.app.env]\n"
        "MYSQL_ALLOW_EMPTY_PASSWORD = 'yes'\n"
        f"BOSN_AUTOSTART_PROOF = '{unique}'\n",
        encoding="utf-8",
    )
    client = bosn.Client(state_dir)
    container_name: str | None = None
    content_sha256: str | None = None
    try:
        with _production_daemon(state_dir) as (_, daemon):
            _wait_for_daemon(client, daemon)
            job = client.submit_manifest_ensure(
                workspace,
                "bosn.toml",
                "app",
                deadline_ms=90_000,
                output_limit=1_048_576,
            )
            _wait_for_success(client, job)
            container = next(
                record
                for record in client.registry_resources(limit=16).records
                if record.kind == "container" and record.stack == "app"
            )
            content_sha256 = container.generation.removeprefix("sha256:")
            container_name = container.name
            assert container_name is not None
            observed = _inspect_container(container_name)
            assert observed is not None and observed[1]
            assert observed[2] == image_id

        assert container_name is not None and content_sha256 is not None
        _docker("container", "stop", container_name)
        stopped = _inspect_container(container_name)
        assert stopped is not None and not stopped[1]

        with _production_daemon(state_dir) as (_, daemon):
            _wait_for_daemon(client, daemon)
            restarted = _inspect_container(container_name)
            assert restarted is not None and restarted[1]
            assert restarted[2] == image_id
            assert any(
                event.kind == "manifest.recovery.started"
                for event in client.setup_ensure_events(limit=64).records
            )
    finally:
        if container_name is not None and content_sha256 is not None:
            _remove_exact_managed_container(container_name, content_sha256)


def test_native_python_client_converges_all_manifest_stacks_in_order(tmp_path: Path) -> None:
    """One installed-wheel job safely converges every declared stack.

    The legacy TOML shape deliberately has no dependency edge. This proof uses
    two independently valid pinned Linux stacks and verifies the all-stack
    client surface, durable records, deterministic progress logs, and real
    managed containers without adding a caller-selected Docker control.
    """

    image_id = _pinned_image_id(PINNED_MANIFEST_MYSQL)
    state_dir = tmp_path / "state"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    unique = f"manifest-converge-{os.getpid()}-{time.time_ns()}"
    (workspace / "bosn.toml").write_text(
        "[stack.zebra]\n"
        f"image = '{PINNED_MANIFEST_MYSQL}'\n"
        "[stack.zebra.env]\n"
        "MYSQL_ALLOW_EMPTY_PASSWORD = 'yes'\n"
        f"BOSN_CONVERGE_PROOF = '{unique}-z'\n"
        "[stack.alpha]\n"
        f"image = '{PINNED_MANIFEST_MYSQL}'\n"
        "[stack.alpha.env]\n"
        "MYSQL_ALLOW_EMPTY_PASSWORD = 'yes'\n"
        f"BOSN_CONVERGE_PROOF = '{unique}-a'\n",
        encoding="utf-8",
    )
    client = bosn.Client(state_dir)
    for field in ("stack", "root", "depends_on", "docker_args", "image"):
        with pytest.raises(TypeError):
            client.submit_manifest_converge(
                workspace,
                "bosn.toml",
                deadline_ms=90_000,
                output_limit=1_048_576,
                **{field: "unsafe"},
            )

    managed_containers: list[tuple[str, str]] = []
    try:
        with _production_daemon(state_dir) as (_, daemon):
            _wait_for_daemon(client, daemon)
            job = client.submit_manifest_converge(
                workspace,
                "bosn.toml",
                deadline_ms=90_000,
                output_limit=1_048_576,
            )
            logs = _wait_for_success(client, job)
            assert "[manifest-converge] ensuring stack alpha (1/2)" in logs
            assert "[manifest-converge] ensuring stack zebra (2/2)" in logs
            resources = client.registry_resources(limit=16).records
            containers = [
                record
                for record in resources
                if record.kind == "container" and record.state == "active"
            ]
            assert [record.stack for record in containers] == ["alpha", "zebra"]
            for record in containers:
                assert record.generation.startswith("sha256:")
                content = record.generation.removeprefix("sha256:")
                name = f"bosn-setup-{content}"
                managed_containers.append((name, content))
                observed = _inspect_container(name)
                assert observed is not None
                assert observed[1]
                assert observed[2] == image_id
                assert observed[3][CONTENT_LABEL] == content
            assert _read_manifest_success_events(state_dir) == [
                ("manifest.ensure.succeeded", f"job_id={job}"),
                ("manifest.ensure.succeeded", f"job_id={job}"),
            ]
    finally:
        for name, content in reversed(managed_containers):
            _remove_exact_managed_container(name, content)


def test_native_manifest_task_inherits_declared_workspace_binds_and_workdir(
    tmp_path: Path,
) -> None:
    """Manifest binds/workdir remain declaration data through managed exec.

    The workdir is selected only by the manifest and reaches the persistent
    app at container creation.  The named task has no mount, workdir, or raw
    Docker input, yet observes both a writable workspace bind and a separate
    readonly bind.  This is intentionally opt-in because it needs the pinned
    MySQL image and a live local Docker daemon.
    """

    image_id = _pinned_image_id(PINNED_MANIFEST_MYSQL)
    state_dir = tmp_path / "state"
    workspace = tmp_path / "workspace"
    writable = workspace / "writable"
    readonly = workspace / "readonly"
    writable.mkdir(parents=True)
    readonly.mkdir()
    unique = f"manifest-bind-{os.getpid()}-{time.time_ns()}"
    (readonly / "proof.txt").write_text(unique + "\n", encoding="utf-8")
    manifest = workspace / "bosn.toml"
    manifest.write_text(
        "[stack.linux]\n"
        f"image = '{PINNED_MANIFEST_MYSQL}'\n"
        "workdir = '/workspace/writable'\n"
        "[stack.linux.env]\n"
        "MYSQL_ALLOW_EMPTY_PASSWORD = 'yes'\n"
        "[stack.linux.mounts.writable]\n"
        "source = 'writable'\n"
        "destination = '/workspace/writable'\n"
        "[stack.linux.mounts.readonly]\n"
        "source = 'readonly'\n"
        "destination = '/workspace/readonly'\n"
        "readonly = true\n"
        "[task.prove]\n"
        "stack = 'linux'\n"
        "cmd = '''test \"$(pwd)\" = /workspace/writable && "
        f'test "$(cat /workspace/readonly/proof.txt)" = {unique} && '
        "! touch /workspace/readonly/must-remain-readonly && "
        "touch app-task-ran.txt'''\n",
        encoding="utf-8",
    )
    container_name: str | None = None
    content_sha256: str | None = None
    try:
        client = bosn.Client(state_dir)
        with _production_daemon(state_dir) as (_, daemon):
            _wait_for_daemon(client, daemon)
            ensure_job = client.submit_manifest_ensure(
                workspace,
                "bosn.toml",
                "linux",
                deadline_ms=90_000,
                output_limit=1_048_576,
            )
            assert isinstance(_wait_for_success(client, ensure_job), tuple)
            container = next(
                record
                for record in client.registry_resources(limit=16).records
                if record.kind == "container"
            )
            content_sha256 = container.generation.removeprefix("sha256:")
            container_name = f"bosn-setup-{content_sha256}"
            assert (
                _docker(
                    "container",
                    "inspect",
                    "--format",
                    "{{.Config.WorkingDir}}",
                    container_name,
                ).stdout.strip()
                == "/workspace/writable"
            )
            observed = _inspect_container(container_name)
            assert observed is not None
            assert observed[2] == image_id

            task_job = client.submit_manifest_app_task(
                workspace,
                "bosn.toml",
                "linux",
                "prove",
                deadline_ms=90_000,
                output_limit=1_048_576,
            )
            assert isinstance(_wait_for_success(client, task_job), tuple)
            assert (writable / "app-task-ran.txt").is_file()
            assert not (readonly / "must-remain-readonly").exists()
    finally:
        if container_name is not None and content_sha256 is not None:
            _remove_exact_managed_container(container_name, content_sha256)


def test_native_manifest_dockerfile_context_is_private_content_addressed_and_rolls(
    tmp_path: Path,
) -> None:
    """A fresh wheel builds a manifest context and task-reuses its exact app.

    The selected workspace Dockerfile uses a digest-pinned base.  The test
    changes a copied source file, proves a new private build generation and
    managed container, then runs each declaration-only task in its matching
    persistent application.  Docker is used only as an external verifier and
    for exact-label container cleanup.
    """

    _pinned_image_id()
    state_dir = tmp_path / "state"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    manifest = workspace / "bosn.toml"
    dockerfile = workspace / "Dockerfile"
    payload = workspace / "payload.txt"
    containers: list[tuple[str, str]] = []

    def write_manifest(task_name: str, expected: str) -> None:
        manifest.write_text(
            "[stack.app]\n"
            "dockerfile = 'Dockerfile'\n"
            f"[task.{task_name}]\n"
            "stack = 'app'\n"
            f"cmd = '''test \"$(cat /payload.txt)\" = \"{expected}\"'''\n",
            encoding="utf-8",
        )

    dockerfile.write_text(
        f"FROM {PINNED_ALPINE}\n"
        "COPY payload.txt /payload.txt\n"
        'CMD ["sh", "-c", "while true; do sleep 30; done"]\n',
        encoding="utf-8",
    )
    payload.write_text("one\n", encoding="utf-8")
    write_manifest("one", "one")
    try:
        client = bosn.Client(state_dir)
        with _production_daemon(state_dir) as (_, daemon):
            _wait_for_daemon(client, daemon)
            first_job = client.submit_manifest_ensure(
                workspace, "bosn.toml", "app", deadline_ms=90_000, output_limit=1_048_576
            )
            assert "[manifest] preparing immutable application image" in _wait_for_success(
                client, first_job
            )
            first = next(
                record
                for record in client.registry_resources(limit=32).records
                if record.kind == "container" and record.state == "active"
            )
            first_content = first.generation.removeprefix("sha256:")
            first_name = f"bosn-setup-{first_content}"
            containers.append((first_name, first_content))
            first_observed = _inspect_container(first_name)
            assert first_observed is not None and first_observed[1]
            assert first_observed[3] == {
                MANAGED_LABEL: "v1",
                CONTENT_LABEL: first_content,
                NAME_LABEL: first_name,
            }
            _wait_for_success(
                client,
                client.submit_manifest_app_task(
                    workspace,
                    "bosn.toml",
                    "app",
                    "one",
                    deadline_ms=90_000,
                    output_limit=1_048_576,
                ),
            )

            payload.write_text("two\n", encoding="utf-8")
            write_manifest("two", "two")
            _wait_for_success(
                client,
                client.submit_manifest_ensure(
                    workspace,
                    "bosn.toml",
                    "app",
                    deadline_ms=90_000,
                    output_limit=1_048_576,
                ),
            )
            second = next(
                record
                for record in client.registry_resources(limit=32).records
                if record.kind == "container" and record.state == "active"
            )
            second_content = second.generation.removeprefix("sha256:")
            second_name = f"bosn-setup-{second_content}"
            assert second_name != first_name
            containers.append((second_name, second_content))
            assert _inspect_container(second_name) is not None
            _wait_for_success(
                client,
                client.submit_manifest_app_task(
                    workspace,
                    "bosn.toml",
                    "app",
                    "two",
                    deadline_ms=90_000,
                    output_limit=1_048_576,
                ),
            )

            dockerfile.write_text(
                f"FROM {PINNED_ALPINE}\n"
                "LABEL bosn.manifest-rollover=three\n"
                "COPY payload.txt /payload.txt\n"
                'CMD ["sh", "-c", "while true; do sleep 30; done"]\n',
                encoding="utf-8",
            )
            _wait_for_success(
                client,
                client.submit_manifest_ensure(
                    workspace,
                    "bosn.toml",
                    "app",
                    deadline_ms=90_000,
                    output_limit=1_048_576,
                ),
            )
            third = next(
                record
                for record in client.registry_resources(limit=32).records
                if record.kind == "container" and record.state == "active"
            )
            third_content = third.generation.removeprefix("sha256:")
            third_name = f"bosn-setup-{third_content}"
            assert third_name not in {first_name, second_name}
            containers.append((third_name, third_content))
            _wait_for_success(
                client,
                client.submit_manifest_app_task(
                    workspace,
                    "bosn.toml",
                    "app",
                    "two",
                    deadline_ms=90_000,
                    output_limit=1_048_576,
                ),
            )
    finally:
        for name, content in reversed(containers):
            _remove_exact_managed_container(name, content)


def test_native_manifest_tmpfs_is_typed_and_empty_after_generation_rollover(
    tmp_path: Path,
) -> None:
    """Fresh native wheel: tmpfs reaches Docker but never survives a rollover."""

    _pinned_image_id(PINNED_MANIFEST_MYSQL)
    state_dir = tmp_path / "state"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    manifest = workspace / "bosn.toml"
    containers: list[tuple[str, str]] = []

    def write_manifest(marker: str, task_name: str, command: str) -> None:
        manifest.write_text(
            "[stack.linux]\n"
            f"image = '{PINNED_MANIFEST_MYSQL}'\n"
            "tmpfs = ['/run/bosn-tmpfs:rw,size=1m']\n"
            "[stack.linux.env]\n"
            "MYSQL_ALLOW_EMPTY_PASSWORD = 'yes'\n"
            f"BOSN_TMPFS_GENERATION = '{marker}'\n"
            f"[task.{task_name}]\n"
            "stack = 'linux'\n"
            f"cmd = '{command}'\n",
            encoding="utf-8",
        )

    try:
        write_manifest("one", "seed", "test -d /run/bosn-tmpfs && touch /run/bosn-tmpfs/proof")
        client = bosn.Client(state_dir)
        with _production_daemon(state_dir) as (_, daemon):
            _wait_for_daemon(client, daemon)
            _wait_for_success(
                client,
                client.submit_manifest_ensure(
                    workspace, "bosn.toml", "linux", deadline_ms=90_000, output_limit=1_048_576
                ),
            )
            first = next(
                record
                for record in client.registry_resources(limit=16).records
                if record.kind == "container"
            )
            first_content = first.generation.removeprefix("sha256:")
            first_name = f"bosn-setup-{first_content}"
            containers.append((first_name, first_content))
            host_tmpfs = json.loads(
                _docker(
                    "container", "inspect", "--format", "{{json .HostConfig.Tmpfs}}", first_name
                ).stdout
            )
            assert "/run/bosn-tmpfs" in host_tmpfs
            _wait_for_success(
                client,
                client.submit_manifest_app_task(
                    workspace,
                    "bosn.toml",
                    "linux",
                    "seed",
                    deadline_ms=90_000,
                    output_limit=1_048_576,
                ),
            )

            write_manifest(
                "two", "verify", "test -d /run/bosn-tmpfs && ! test -e /run/bosn-tmpfs/proof"
            )
            _wait_for_success(
                client,
                client.submit_manifest_ensure(
                    workspace, "bosn.toml", "linux", deadline_ms=90_000, output_limit=1_048_576
                ),
            )
            current = next(
                record
                for record in client.registry_resources(limit=16).records
                if record.kind == "container" and record.state == "active"
            )
            current_content = current.generation.removeprefix("sha256:")
            current_name = f"bosn-setup-{current_content}"
            assert current_name != first_name
            containers.append((current_name, current_content))
            _wait_for_success(
                client,
                client.submit_manifest_app_task(
                    workspace,
                    "bosn.toml",
                    "linux",
                    "verify",
                    deadline_ms=90_000,
                    output_limit=1_048_576,
                ),
            )
    finally:
        for name, content in reversed(containers):
            _remove_exact_managed_container(name, content)


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
