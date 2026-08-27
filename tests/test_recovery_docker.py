"""Real-engine safety boundary for the explicit #120 legacy volume recovery."""

from __future__ import annotations

import uuid

import pytest

from bosn import labels
from bosn.engine import Engine
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
