"""Opt-in live-Docker proof for the installed PyO3 setup-ensure boundary.

The Python client submits and observes the job exclusively through the public
native extension.  Docker appears here only as an external verifier and to
remove the one container whose complete Bosn ownership tuple is rechecked.
"""

from __future__ import annotations

import os
import time
from pathlib import Path

import pytest

import bosn
from setup_docker_support import (
    CONTENT_LABEL,
    MANAGED_LABEL,
    NAME_LABEL,
    PINNED_ALPINE,
    PINNED_MANIFEST_MYSQL,
    container_environment,
    docker,
    inspect_container,
    live_docker_enabled,
    pinned_image_id,
    production_daemon,
    read_exact_resource_uses,
    read_manifest_success_events,
    remove_exact_managed_container,
    remove_exact_managed_volume,
    wait_for_daemon,
    wait_for_failure,
    wait_for_success,
)

pytestmark = [
    pytest.mark.docker,
    pytest.mark.slow,
    pytest.mark.skipif(
        not live_docker_enabled(),
        reason="set BOSN_RUN_LIVE_DOCKER=1 to run the real Docker acceptance",
    ),
]


def test_native_python_client_ensures_and_reuses_one_managed_app(tmp_path: Path) -> None:
    """One-file setup ensure stays semantic across two daemon processes."""

    pinned_image_id()
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
    assert inspect_container(container_name) is None, "refusing an existing deterministic app"

    try:
        with production_daemon(state_dir) as (_, first_daemon):
            wait_for_daemon(client, first_daemon)
            first_job = client.submit_setup_ensure(
                workspace,
                str(config),
                policy="online_refresh",
                deadline_ms=90_000,
                output_limit=1_048_576,
            )
            first_logs = wait_for_success(client, first_job)
            assert isinstance(first_logs, tuple)
            first = inspect_container(container_name)
            assert first is not None
            first_id, first_running, first_image, first_labels = first
            assert first_running
            assert first_image == pinned_image_id()
            assert first_labels[MANAGED_LABEL] == "v1"
            assert first_labels[CONTENT_LABEL] == plan.content_sha256
            assert first_labels[NAME_LABEL] == container_name

        # A distinct production daemon and a fresh Python binding client must
        # discover and reuse the matching app rather than replacing it.
        second_client = bosn.Client(state_dir)
        with production_daemon(state_dir) as (_, second_daemon):
            wait_for_daemon(second_client, second_daemon)
            second_job = second_client.submit_setup_ensure(
                workspace,
                str(config),
                policy="offline_cache_only",
                deadline_ms=90_000,
                output_limit=1_048_576,
            )
            second_logs = wait_for_success(second_client, second_job)
            assert isinstance(second_logs, tuple)
            second = inspect_container(container_name)
            assert second is not None
            second_id, second_running, second_image, second_labels = second
            assert second_id == first_id, "Python ensure replaced the matching app"
            assert second_running
            assert second_image == first_image
            assert second_labels == first_labels

        assert not any(workspace.iterdir()), "ensure wrote into the selected workspace"
    finally:
        remove_exact_managed_container(container_name, plan.content_sha256)


def test_native_python_client_ensures_supported_compose_yaml_source(tmp_path: Path) -> None:
    """A lossless one-service Compose YAML source uses the normal setup lifecycle.

    The test deliberately speaks only the typed Python setup API.  It neither
    invokes nor requires a Compose executable, and the YAML carries no raw
    Docker arguments: the fixed setup engine receives only the translated
    pinned-image, environment, and fixed-shell command semantics.
    """

    image_id = pinned_image_id()
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
    assert inspect_container(container_name) is None, "refusing an existing deterministic app"

    try:
        with production_daemon(state_dir) as (_, daemon):
            wait_for_daemon(client, daemon)
            first_job = client.submit_setup_ensure(
                workspace,
                str(config),
                policy="online_refresh",
                deadline_ms=90_000,
                output_limit=1_048_576,
            )
            assert isinstance(wait_for_success(client, first_job), tuple)

            first = inspect_container(container_name)
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
            assert isinstance(wait_for_success(client, second_job), tuple)
            second = inspect_container(container_name)
            assert second is not None
            second_id, second_running, second_image, second_labels = second
            assert second_id == first_id, "Compose YAML ensure replaced the matching app"
            assert second_running
            assert second_image == first_image
            assert second_labels == first_labels
            assert client.status().resources == 2

        assert not any(workspace.iterdir()), "ensure wrote into the selected workspace"
    finally:
        remove_exact_managed_container(container_name, plan.content_sha256)


def test_native_python_client_ensures_and_reuses_manifest_stack(tmp_path: Path) -> None:
    """A manifest stack becomes one verified, daemon-owned Linux application.

    This covers the Rust manifest bridge's supported runtime subset through
    the installed PyO3 Client.  It deliberately has no raw image, container,
    Docker, or command argument: image and environment are declaration data
    in the workspace-contained ``bosn.toml`` only.
    """

    image_id = pinned_image_id(PINNED_MANIFEST_MYSQL)
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
        with production_daemon(state_dir) as (_, daemon):
            wait_for_daemon(client, daemon)

            # An image-only workdir remains outside the supported typed shape
            # and is refused before it reaches Docker.
            rejected = client.submit_manifest_ensure(
                workspace,
                "unsupported.toml",
                "rejected",
                deadline_ms=90_000,
                output_limit=1_048_576,
            )
            assert "not covered by a declared workspace mount" in wait_for_failure(client, rejected)

            first_job = client.submit_manifest_ensure(
                workspace,
                "bosn.toml",
                "linux",
                deadline_ms=90_000,
                output_limit=1_048_576,
            )
            first_logs = wait_for_success(client, first_job)
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
                docker(
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

            first = inspect_container(container_name)
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
            } <= container_environment(container_name)

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
            assert read_exact_resource_uses(state_dir) == expected_uses
            assert read_manifest_success_events(state_dir) == [
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
            assert isinstance(wait_for_success(client, seed_job), tuple)

            # Reusing the unchanged declaration must keep both the exact app
            # container and the declared Bosn-managed volume intact.
            second_job = client.submit_manifest_ensure(
                workspace,
                "bosn.toml",
                "linux",
                deadline_ms=90_000,
                output_limit=1_048_576,
            )
            assert isinstance(wait_for_success(client, second_job), tuple)
            second = inspect_container(container_name)
            assert second is not None
            assert second[0] == first_id
            assert (
                docker(
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
            assert isinstance(wait_for_success(client, rollover_job), tuple)
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
            assert inspect_container(container_name) is not None
            rollover_observed = inspect_container(rollover_name)
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
            assert isinstance(wait_for_success(client, volume_proof_job), tuple)
            rollover_uses = read_exact_resource_uses(state_dir)
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
            assert read_manifest_success_events(state_dir) == [
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
            task_logs = wait_for_success(client, task_job)
            assert "[manifest-app-task] proving exact managed application ownership" in task_logs
            assert "[manifest-app-task] running declared task prove" in task_logs
            assert client.status().sessions == 0
    finally:
        for name, content in reversed(managed_containers):
            remove_exact_managed_container(name, content)
        for name, content in reversed(managed_volumes):
            remove_exact_managed_volume(name, content)
