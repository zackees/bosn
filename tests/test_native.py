"""The native extension surface, re-exported by the ``bosn`` package."""

import hashlib
from pathlib import Path

import pytest

import bosn
from bosn import _native as native


def test_native_extension_is_reexported_by_python_package(tmp_path: Path) -> None:
    assert bosn.Client is native.Client
    assert bosn.ComposePlan is native.ComposePlan
    assert bosn.DoctorReport is native.DoctorReport
    assert bosn.Status is native.Status
    assert bosn.RegistryResourcePage is native.RegistryResourcePage
    assert bosn.SetupEnsureEventPage is native.SetupEnsureEventPage
    assert bosn.native_version() == bosn.__version__
    assert bosn.protocol_version() == 1

    compose_plan = bosn.plan_compose_yaml("services:\n  api:\n    image: alpine:3.21\n")
    assert compose_plan.applied is False
    assert compose_plan.version == 1
    assert compose_plan.digest.startswith("sha256:")
    assert "alpine:3.21" in compose_plan.document_json
    with pytest.raises(AttributeError):
        compose_plan.applied = True

    client = bosn.Client(tmp_path / "state")
    assert client.state_dir == str(tmp_path / "state")
    with pytest.raises(RuntimeError, match="Io"):
        client.status()
    report = client.doctor()
    assert report.daemon == "unavailable"
    assert report.registry == "unavailable"
    assert report.engine == "unavailable"
    assert not (tmp_path / "state").exists()


def test_native_registry_diagnostics_reject_bad_pages_without_initializing_state(
    tmp_path: Path,
) -> None:
    state = tmp_path / "state"
    client = bosn.Client(state)
    with pytest.raises(ValueError, match="limit must"):
        client.registry_resources(limit=0)
    with pytest.raises(ValueError, match="limit must"):
        client.setup_ensure_events(limit=65)
    assert not state.exists()
    with pytest.raises(RuntimeError, match="daemon"):
        client.registry_resources(limit=1)
    assert not state.exists()


def test_native_setup_plan_is_structured_and_requires_an_explicit_policy(tmp_path: Path) -> None:
    state_dir = tmp_path / "state"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    config = tmp_path / "setup.toml"
    document = (
        "version = 1\n"
        "[app]\n"
        "image = 'registry.example/demo@sha256:"
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'\n"
        "[task.check]\n"
        "command = 'echo check'\n"
        "[task.lint]\n"
        "command = 'echo lint'\n"
    )
    config.write_text(document)

    client = bosn.Client(state_dir)
    plan = client.plan_setup(workspace, str(config), policy="online_refresh")

    assert plan.source_kind == "local_file"
    assert plan.content_sha256 == hashlib.sha256(document.encode()).hexdigest()
    assert plan.schema_version == 1
    assert plan.workspace == str(workspace.resolve())
    assert plan.asset_root is None
    assert plan.task_names == ("check", "lint")
    assert plan.app_source_kind == "pinned_image"
    assert plan.image == "registry.example/demo@sha256:" + "a" * 64
    assert plan.dockerfile_path is None
    assert plan.applied is False
    with pytest.raises(AttributeError):
        plan.applied = True
    with pytest.raises(ValueError, match="policy must"):
        client.plan_setup(workspace, str(config), policy="refresh")
    secret_locator = "https://user:top-secret@example.test/setup.toml"
    with pytest.raises(RuntimeError, match="setup config locator is invalid") as error:
        client.plan_setup(workspace, secret_locator, policy="online_refresh")
    assert "top-secret" not in str(error.value)


def test_native_setup_plan_reuses_local_inline_document_offline_without_docker(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    state_dir = tmp_path / "state"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    config = tmp_path / "setup.toml"
    config.write_text(
        "version = 1\n"
        "[app]\n"
        "dockerfile = 'FROM scratch'\n"
        "[task.check]\n"
        "command = 'echo check'\n"
        "[[file]]\n"
        "path = 'scripts/check.sh'\n"
        "content = '#!/bin/sh\\necho check\\n'\n"
    )
    monkeypatch.setenv("DOCKER_HOST", "tcp://127.0.0.1:1")

    client = bosn.Client(state_dir)
    first = client.plan_setup(workspace, str(config), policy="online_refresh")
    config.unlink()
    offline = client.plan_setup(workspace, str(config), policy="offline_cache_only")

    assert offline.content_sha256 == first.content_sha256
    assert offline.app_source_kind == "inline_dockerfile"
    assert offline.image is None
    assert offline.asset_root == first.asset_root
    assert offline.dockerfile_path == first.dockerfile_path
    assert offline.asset_root is not None
    assert Path(offline.asset_root).is_relative_to(state_dir)
    assert Path(offline.dockerfile_path).read_text() == "FROM scratch"
    assert list(workspace.iterdir()) == []
    assert offline.applied is False


def test_native_setup_prepare_rejects_bad_input_and_missing_daemon_without_docker(
    tmp_path: Path,
) -> None:
    client = bosn.Client(tmp_path / "state")
    workspace = tmp_path / "workspace"
    workspace.mkdir()

    with pytest.raises(ValueError, match="policy must"):
        client.submit_setup_prepare(
            workspace,
            "https://example.invalid/setup.toml",
            policy="refresh",
            deadline_ms=1_000,
            output_limit=4 * 1024,
        )
    secret_locator = "https://user:top-secret@example.invalid/setup.toml"
    with pytest.raises(ValueError, match="setup config locator is invalid") as error:
        client.submit_setup_prepare(
            workspace,
            secret_locator,
            policy="online_refresh",
            deadline_ms=1_000,
            output_limit=4 * 1024,
        )
    assert "top-secret" not in str(error.value)
    with pytest.raises(ValueError, match="deadline_ms must"):
        client.submit_setup_prepare(
            workspace,
            "https://example.invalid/setup.toml",
            policy="online_refresh",
            deadline_ms=0,
            output_limit=4 * 1024,
        )
    with pytest.raises(ValueError, match="limit must"):
        client.job_logs(1, limit=0)
    with pytest.raises(RuntimeError, match="daemon"):
        client.submit_setup_prepare(
            workspace,
            "https://example.invalid/setup.toml",
            policy="online_refresh",
            deadline_ms=1_000,
            output_limit=4 * 1024,
        )


def test_native_setup_task_rejects_bad_input_and_missing_daemon_without_docker(
    tmp_path: Path,
) -> None:
    client = bosn.Client(tmp_path / "state")
    workspace = tmp_path / "workspace"
    workspace.mkdir()

    with pytest.raises(ValueError, match="policy must"):
        client.submit_setup_task(
            workspace,
            "https://example.invalid/setup.toml",
            policy="refresh",
            task_name="check",
            deadline_ms=1_000,
            output_limit=4 * 1024,
        )
    secret_locator = "https://user:top-secret@example.invalid/setup.toml"
    with pytest.raises(ValueError, match="setup config locator is invalid") as error:
        client.submit_setup_task(
            workspace,
            secret_locator,
            policy="online_refresh",
            task_name="check",
            deadline_ms=1_000,
            output_limit=4 * 1024,
        )
    assert "top-secret" not in str(error.value)
    with pytest.raises(ValueError, match="task_name is invalid"):
        client.submit_setup_task(
            workspace,
            "https://example.invalid/setup.toml",
            policy="online_refresh",
            task_name="-invalid",
            deadline_ms=1_000,
            output_limit=4 * 1024,
        )
    with pytest.raises(ValueError, match="deadline_ms must"):
        client.submit_setup_task(
            workspace,
            "https://example.invalid/setup.toml",
            policy="online_refresh",
            task_name="check",
            deadline_ms=0,
            output_limit=4 * 1024,
        )
    with pytest.raises(RuntimeError, match="daemon") as error:
        client.submit_setup_task(
            workspace,
            "https://example.invalid/setup.toml",
            policy="online_refresh",
            task_name="check",
            deadline_ms=1_000,
            output_limit=4 * 1024,
        )
    assert "example.invalid" not in str(error.value)


def test_native_setup_ensure_rejects_bad_input_and_missing_daemon_without_docker(
    tmp_path: Path,
) -> None:
    client = bosn.Client(tmp_path / "state")
    workspace = tmp_path / "workspace"
    workspace.mkdir()

    with pytest.raises(ValueError, match="policy must"):
        client.submit_setup_ensure(
            workspace,
            "https://example.invalid/setup.toml",
            policy="refresh",
            deadline_ms=1_000,
            output_limit=4 * 1024,
        )
    secret_locator = "https://user:top-secret@example.invalid/setup.toml"
    with pytest.raises(ValueError, match="setup config locator is invalid") as error:
        client.submit_setup_ensure(
            workspace,
            secret_locator,
            policy="online_refresh",
            deadline_ms=1_000,
            output_limit=4 * 1024,
        )
    assert "top-secret" not in str(error.value)
    with pytest.raises(ValueError, match="deadline_ms must"):
        client.submit_setup_ensure(
            workspace,
            "https://example.invalid/setup.toml",
            policy="online_refresh",
            deadline_ms=0,
            output_limit=4 * 1024,
        )
    with pytest.raises(ValueError, match="output_limit must"):
        client.submit_setup_ensure(
            workspace,
            "https://example.invalid/setup.toml",
            policy="online_refresh",
            deadline_ms=1_000,
            output_limit=0,
        )
    with pytest.raises(RuntimeError, match="daemon") as error:
        client.submit_setup_ensure(
            workspace,
            "https://example.invalid/setup.toml",
            policy="online_refresh",
            deadline_ms=1_000,
            output_limit=4 * 1024,
        )
    assert "example.invalid" not in str(error.value)
