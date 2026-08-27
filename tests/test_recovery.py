"""Regression coverage for #120's explicit legacy-volume recovery boundary."""

from __future__ import annotations

import json

from bosn import labels
from bosn.engine import EngineResult
from bosn.recovery import (
    apply_legacy_volume_reconciliation,
    legacy_expected_labels,
    plan_legacy_volume_reconciliation,
)


class FakeEngine:
    def __init__(
        self, raw: dict[str, str], *, attached: str = "", attachment_ok: bool = True
    ) -> None:
        self.raw = raw
        self.attached = attached
        self.attachment_ok = attachment_ok
        self.commands: list[list[str]] = []

    def run(
        self, args: list[str], *, check: bool = False, timeout: float | None = None
    ) -> EngineResult:
        self.commands.append(list(args))
        if args[:3] == ["ps", "--all", "--filter"]:
            return EngineResult(0 if self.attachment_ok else 1, self.attached, "unavailable")
        if args[:2] == ["volume", "inspect"]:
            return EngineResult(0, json.dumps(self.raw), "")
        if args[:2] == ["volume", "create"]:
            self.raw = {
                args[index + 1].split("=", 1)[0]: args[index + 1].split("=", 1)[1]
                for index, item in enumerate(args[:-1])
                if item == "--label"
            }
        return EngineResult(0, "", "")


def expected(raw: dict[str, str]) -> labels.ResourceLabels:
    return legacy_expected_labels(
        registry_id="ours",
        stack="perf",
        generation="sha256:current",
        scope="stack",
        workspace="/workspace",
        raw_labels=raw,
    )


def partial() -> dict[str, str]:
    raw = expected({}).to_dict()
    del raw[labels.CREATED]
    return raw


def test_explicit_legacy_preview_then_apply_recreates_matching_detached_volume() -> None:
    raw = partial()
    engine = FakeEngine(raw)
    plan = plan_legacy_volume_reconciliation(
        name="bosn-s-perf-target",
        raw_labels=raw,
        expected=expected(raw),
        registry_id="ours",
        engine=engine,  # type: ignore[arg-type]
    )
    assert plan.action == "would-recreate"
    assert plan.to_dict()["attachment"] == {"state": "detached", "containers": []}

    apply_legacy_volume_reconciliation(engine, plan)  # type: ignore[arg-type]
    assert labels.is_owned_by(engine.raw, "ours")


def test_legacy_recovery_allows_no_registry_history_but_refuses_unlabeled_or_conflicting() -> None:
    raw = partial()
    matching = plan_legacy_volume_reconciliation(
        name="target",
        raw_labels=raw,
        expected=expected(raw),
        registry_id="ours",
        engine=FakeEngine(raw),  # type: ignore[arg-type]
    )
    assert matching.action == "would-recreate"

    unlabeled = plan_legacy_volume_reconciliation(
        name="target",
        raw_labels={},
        expected=expected({}),
        registry_id="ours",
        engine=FakeEngine({}),  # type: ignore[arg-type]
    )
    assert unlabeled.action == "refused"

    conflicting_raw = partial()
    conflicting_raw[labels.STACK] = "other"
    conflicting = plan_legacy_volume_reconciliation(
        name="target",
        raw_labels=conflicting_raw,
        expected=expected(conflicting_raw),
        registry_id="ours",
        engine=FakeEngine(conflicting_raw),  # type: ignore[arg-type]
    )
    assert conflicting.action == "refused"


def test_legacy_recovery_requires_a_manifest_binding_discriminator() -> None:
    registry_and_kind = {labels.REGISTRY: "ours", labels.KIND: "volume"}
    kind_only = plan_legacy_volume_reconciliation(
        name="target",
        raw_labels=registry_and_kind,
        expected=expected(registry_and_kind),
        registry_id="ours",
        engine=FakeEngine(registry_and_kind),  # type: ignore[arg-type]
    )
    assert kind_only.action == "refused"

    registry_and_created = {
        labels.REGISTRY: "ours",
        labels.CREATED: "2026-08-26T00:00:00Z",
    }
    created_only = plan_legacy_volume_reconciliation(
        name="target",
        raw_labels=registry_and_created,
        expected=expected(registry_and_created),
        registry_id="ours",
        engine=FakeEngine(registry_and_created),  # type: ignore[arg-type]
    )
    assert created_only.action == "refused"

    registry_and_stack = {labels.REGISTRY: "ours", labels.STACK: "perf"}
    stack_bound = plan_legacy_volume_reconciliation(
        name="target",
        raw_labels=registry_and_stack,
        expected=expected(registry_and_stack),
        registry_id="ours",
        engine=FakeEngine(registry_and_stack),  # type: ignore[arg-type]
    )
    assert stack_bound.action == "would-recreate"


def test_legacy_recovery_refuses_attached_or_unknown_volume() -> None:
    raw = partial()
    attached = plan_legacy_volume_reconciliation(
        name="target",
        raw_labels=raw,
        expected=expected(raw),
        registry_id="ours",
        engine=FakeEngine(raw, attached="container-id\n"),  # type: ignore[arg-type]
    )
    assert attached.action == "refused"
    assert attached.attachment is not None and attached.attachment.state == "attached"

    unknown = plan_legacy_volume_reconciliation(
        name="target",
        raw_labels=raw,
        expected=expected(raw),
        registry_id="ours",
        engine=FakeEngine(raw, attachment_ok=False),  # type: ignore[arg-type]
    )
    assert unknown.action == "refused"
    assert unknown.attachment is not None and unknown.attachment.state == "unknown"


def test_already_owned_legacy_candidate_is_idempotent() -> None:
    raw = expected({}).to_dict()
    plan = plan_legacy_volume_reconciliation(
        name="target",
        raw_labels=raw,
        expected=expected(raw),
        registry_id="ours",
        engine=FakeEngine(raw),  # type: ignore[arg-type]
    )
    assert plan.action == "already-owned"

    wrong_identity = dict(raw)
    wrong_identity[labels.STACK] = "other"
    refused = plan_legacy_volume_reconciliation(
        name="target",
        raw_labels=wrong_identity,
        expected=expected(wrong_identity),
        registry_id="ours",
        engine=FakeEngine(wrong_identity),  # type: ignore[arg-type]
    )
    assert refused.action == "refused"
