"""Live-Docker proofs for native manifest stacks: restart autostart, converge order,
workspace binds, Dockerfile contexts, tmpfs and declared tasks."""

from __future__ import annotations

import json
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
    docker,
    inspect_container,
    live_docker_enabled,
    pinned_image_id,
    production_daemon,
    read_manifest_success_events,
    remove_exact_managed_container,
    wait_for_daemon,
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


def test_native_default_manifest_stack_autostarts_after_daemon_restart(tmp_path: Path) -> None:
    """A fresh installed wheel restarts only a selected manifest app.

    The daemon is deliberately stopped before Docker is asked to stop the
    already-proven exact container. Starting a new bundled daemon must use the
    durable default-stack intent and exact source/registry/label/image proof;
    this test never asks the public API to start a raw container.
    """

    image_id = pinned_image_id(PINNED_MANIFEST_MYSQL)
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
        with production_daemon(state_dir) as (_, daemon):
            wait_for_daemon(client, daemon)
            job = client.submit_manifest_ensure(
                workspace,
                "bosn.toml",
                "app",
                deadline_ms=90_000,
                output_limit=1_048_576,
            )
            wait_for_success(client, job)
            container = next(
                record
                for record in client.registry_resources(limit=16).records
                if record.kind == "container" and record.stack == "app"
            )
            content_sha256 = container.generation.removeprefix("sha256:")
            container_name = container.name
            assert container_name is not None
            observed = inspect_container(container_name)
            assert observed is not None and observed[1]
            assert observed[2] == image_id

        assert container_name is not None and content_sha256 is not None
        docker("container", "stop", container_name)
        stopped = inspect_container(container_name)
        assert stopped is not None and not stopped[1]

        with production_daemon(state_dir) as (_, daemon):
            wait_for_daemon(client, daemon)
            restarted = inspect_container(container_name)
            assert restarted is not None and restarted[1]
            assert restarted[2] == image_id
            assert any(
                event.kind == "manifest.recovery.started"
                for event in client.setup_ensure_events(limit=64).records
            )
    finally:
        if container_name is not None and content_sha256 is not None:
            remove_exact_managed_container(container_name, content_sha256)


def test_native_python_client_converges_all_manifest_stacks_in_order(tmp_path: Path) -> None:
    """One installed-wheel job safely converges every declared stack.

    The legacy TOML shape deliberately has no dependency edge. This proof uses
    two independently valid pinned Linux stacks and verifies the all-stack
    client surface, durable records, deterministic progress logs, and real
    managed containers without adding a caller-selected Docker control.
    """

    image_id = pinned_image_id(PINNED_MANIFEST_MYSQL)
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
        with production_daemon(state_dir) as (_, daemon):
            wait_for_daemon(client, daemon)
            job = client.submit_manifest_converge(
                workspace,
                "bosn.toml",
                deadline_ms=90_000,
                output_limit=1_048_576,
            )
            logs = wait_for_success(client, job)
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
                observed = inspect_container(name)
                assert observed is not None
                assert observed[1]
                assert observed[2] == image_id
                assert observed[3][CONTENT_LABEL] == content
            assert read_manifest_success_events(state_dir) == [
                ("manifest.ensure.succeeded", f"job_id={job}"),
                ("manifest.ensure.succeeded", f"job_id={job}"),
            ]
    finally:
        for name, content in reversed(managed_containers):
            remove_exact_managed_container(name, content)


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

    image_id = pinned_image_id(PINNED_MANIFEST_MYSQL)
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
        with production_daemon(state_dir) as (_, daemon):
            wait_for_daemon(client, daemon)
            ensure_job = client.submit_manifest_ensure(
                workspace,
                "bosn.toml",
                "linux",
                deadline_ms=90_000,
                output_limit=1_048_576,
            )
            assert isinstance(wait_for_success(client, ensure_job), tuple)
            container = next(
                record
                for record in client.registry_resources(limit=16).records
                if record.kind == "container"
            )
            content_sha256 = container.generation.removeprefix("sha256:")
            container_name = f"bosn-setup-{content_sha256}"
            assert (
                docker(
                    "container",
                    "inspect",
                    "--format",
                    "{{.Config.WorkingDir}}",
                    container_name,
                ).stdout.strip()
                == "/workspace/writable"
            )
            observed = inspect_container(container_name)
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
            assert isinstance(wait_for_success(client, task_job), tuple)
            assert (writable / "app-task-ran.txt").is_file()
            assert not (readonly / "must-remain-readonly").exists()
    finally:
        if container_name is not None and content_sha256 is not None:
            remove_exact_managed_container(container_name, content_sha256)


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

    pinned_image_id()
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
        with production_daemon(state_dir) as (_, daemon):
            wait_for_daemon(client, daemon)
            first_job = client.submit_manifest_ensure(
                workspace, "bosn.toml", "app", deadline_ms=90_000, output_limit=1_048_576
            )
            assert "[manifest] preparing immutable application image" in wait_for_success(
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
            first_observed = inspect_container(first_name)
            assert first_observed is not None and first_observed[1]
            assert first_observed[3] == {
                MANAGED_LABEL: "v1",
                CONTENT_LABEL: first_content,
                NAME_LABEL: first_name,
            }
            wait_for_success(
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
            wait_for_success(
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
            assert inspect_container(second_name) is not None
            wait_for_success(
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
            wait_for_success(
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
            wait_for_success(
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
            remove_exact_managed_container(name, content)


def test_native_manifest_tmpfs_is_typed_and_empty_after_generation_rollover(
    tmp_path: Path,
) -> None:
    """Fresh native wheel: tmpfs reaches Docker but never survives a rollover."""

    pinned_image_id(PINNED_MANIFEST_MYSQL)
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
        with production_daemon(state_dir) as (_, daemon):
            wait_for_daemon(client, daemon)
            wait_for_success(
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
                docker(
                    "container", "inspect", "--format", "{{json .HostConfig.Tmpfs}}", first_name
                ).stdout
            )
            assert "/run/bosn-tmpfs" in host_tmpfs
            wait_for_success(
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
            wait_for_success(
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
            wait_for_success(
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
            remove_exact_managed_container(name, content)


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

    image_id = pinned_image_id()
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
    assert inspect_container(container_name) is None, "refusing an existing deterministic app"

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
        with production_daemon(state_dir) as (_, daemon):
            wait_for_daemon(client, daemon)
            ensure_job = client.submit_setup_ensure(
                workspace,
                str(config),
                policy="online_refresh",
                deadline_ms=90_000,
                output_limit=1_048_576,
            )
            assert isinstance(wait_for_success(client, ensure_job), tuple)

            before = inspect_container(container_name)
            assert before is not None
            before_id, before_running, before_image, before_labels = before
            assert before_running
            assert before_image == image_id
            assert before_labels == {
                MANAGED_LABEL: "v1",
                CONTENT_LABEL: plan.content_sha256,
                NAME_LABEL: container_name,
            }
            expected_hostname = docker(
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
            logs = wait_for_success(client, app_task_job)
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

            after = inspect_container(container_name)
            assert after is not None
            after_id, after_running, after_image, after_labels = after
            assert after_id == before_id, "app task replaced the ensured managed app"
            assert after_running
            assert after_image == before_image
            assert after_labels == before_labels
            assert client.status().sessions == 0, "known app-task completion retained a session"
    finally:
        remove_exact_managed_container(container_name, plan.content_sha256)
