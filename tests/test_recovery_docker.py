"""Real-engine safety boundary for the explicit #120 legacy volume recovery."""

from __future__ import annotations

import uuid

import pytest

from bosn import labels
from bosn.converge import resolved_generation, volume_name_for, workspace_of
from bosn.daemon import Daemon
from bosn.engine import Engine
from bosn.manifest import load
from bosn.recovery import (
    apply_legacy_volume_reconciliation,
    legacy_expected_labels,
    plan_legacy_volume_reconciliation,
)
from bosn.resources import ResourceScanner

pytestmark = pytest.mark.docker


def test_exact_detached_partial_volume_is_recreated_but_attached_sibling_is_refused(
    engine: Engine,
) -> None:
    registry = f"recovery-{uuid.uuid4()}"
    name = f"bosn-recovery-{uuid.uuid4().hex[:12]}"
    sibling = f"bosn-recovery-attached-{uuid.uuid4().hex[:12]}"
    holder = f"recovery-holder-{uuid.uuid4().hex[:8]}"
    expected = legacy_expected_labels(
        registry_id=registry,
        stack="proof",
        generation="sha256:proof",
        scope="stack",
        workspace="/proof",
        raw_labels={},
    )
    partial = expected.to_dict()
    del partial[labels.CREATED]
    partial_args = [
        item for key, value in partial.items() for item in ("--label", f"{key}={value}")
    ]
    try:
        engine.run(
            ["volume", "create", *partial_args, name],
            check=True,
        )
        engine.run(
            [
                "volume",
                "create",
                *partial_args,
                sibling,
            ],
            check=True,
        )

        preview = plan_legacy_volume_reconciliation(
            name=name,
            raw_labels=ResourceScanner(engine).inspect_labels("volume", name),
            expected=expected,
            registry_id=registry,
            engine=engine,
        )
        assert preview.action == "would-recreate"
        assert ResourceScanner(engine).inspect_labels("volume", name) == partial
        apply_legacy_volume_reconciliation(engine, preview)
        assert labels.is_owned_by(ResourceScanner(engine).inspect_labels("volume", name), registry)

        # An attachment is deliberately a refusal boundary; no sibling is mutated by recovery.
        engine.run(
            ["create", "--name", holder, "--volume", f"{sibling}:/data", "alpine:3.20", "true"],
            check=True,
        )
        attached = plan_legacy_volume_reconciliation(
            name=sibling,
            raw_labels=ResourceScanner(engine).inspect_labels("volume", sibling),
            expected=expected,
            registry_id=registry,
            engine=engine,
        )
        assert attached.action == "refused"
        assert attached.attachment is not None and attached.attachment.state == "attached"
        assert ResourceScanner(engine).inspect_labels("volume", sibling) == partial
    finally:
        engine.run(["container", "rm", "--force", holder])
        for volume in (name, sibling):
            engine.run(["volume", "rm", "--force", volume])


def test_daemon_reconcile_volume_replans_registers_and_audits_real_engine(
    engine: Engine, tmp_path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The daemon path, rather than the helper alone, owns the mutation boundary."""
    manifest_path = tmp_path / "bosn.toml"
    manifest_path.write_text(
        "[stack.proof]\nimage = 'alpine:3.20'\n\n[stack.proof.volumes]\n"
        "target = { scope = 'stack', destination = '/target' }\n",
        encoding="utf-8",
    )
    daemon = Daemon(state_dir=tmp_path / "state")
    manifest = load(manifest_path)
    stack = manifest.stack("proof")
    workspace = workspace_of(manifest)
    digest, _ = resolved_generation(manifest, stack, engine)
    name = volume_name_for(
        stack, "stack", "target", digest=digest, workspace=workspace, family=stack.family
    )
    expected = legacy_expected_labels(
        registry_id=daemon.registry.registry_id,
        stack="proof",
        generation=digest,
        scope="stack",
        workspace=workspace,
        raw_labels={},
    )
    partial = expected.to_dict()
    del partial[labels.CREATED]
    partial_args = [
        item for key, value in partial.items() for item in ("--label", f"{key}={value}")
    ]
    request = {
        "manifest": str(manifest_path),
        "stack": "proof",
        "volume": "target",
        "engine": "docker",
    }
    monkeypatch.setattr("bosn.engine.Engine", lambda _binary: engine)
    holder = f"recovery-daemon-holder-{uuid.uuid4().hex[:8]}"
    try:
        engine.run(["volume", "create", *partial_args, name], check=True)
        preview = daemon._verb_reconcile_volume(request)
        assert preview["ok"] is True and preview["applied"] is False
        assert preview["plan"]["decision"]["action"] == "would-recreate"
        applied = daemon._verb_reconcile_volume({**request, "apply": True, "yes": True})
        assert applied["ok"] is True and applied["applied"] is True
        assert labels.is_owned_by(
            ResourceScanner(engine).inspect_labels("volume", name), daemon.registry.registry_id
        )
        assert daemon.registry.get_resource_by_engine_identity("volume", name) is not None
        assert any(
            row["kind"] == "volume.legacy_reconciled" and row["detail"] == name
            for row in daemon.registry.events()
        )

        engine.run(["volume", "rm", "--force", name], check=True)
        engine.run(["volume", "create", *partial_args, name], check=True)
        engine.run(
            ["create", "--name", holder, "--volume", f"{name}:/data", "alpine:3.20", "true"],
            check=True,
        )
        refused = daemon._verb_reconcile_volume({**request, "apply": True, "yes": True})
        assert refused["ok"] is False
        assert refused["plan"]["decision"]["action"] == "refused"
        assert ResourceScanner(engine).inspect_labels("volume", name) == partial
    finally:
        engine.run(["container", "rm", "--force", holder])
        engine.run(["volume", "rm", "--force", name])
        daemon.registry.close()
