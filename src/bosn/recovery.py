"""Fail-closed recovery plans for incomplete Bosn resources.

An incomplete label set is deliberately not ownership proof.  The durable creation
intent introduced in schema v3 is the only basis for automatic recovery.  This module
models the narrower legacy escape hatch: an operator may explicitly reconcile one
manifest-derived volume when the engine still carries corroborating Bosn labels.
"""

from __future__ import annotations

import datetime as dt
from dataclasses import dataclass

from bosn import labels
from bosn.engine import Engine
from bosn.resources import DiscoveredResource, TransferError, recreate_volume_with_labels


def _now_iso() -> str:
    return dt.datetime.now(dt.UTC).strftime("%Y-%m-%dT%H:%M:%SZ")


@dataclass(frozen=True)
class AttachmentReport:
    state: str
    containers: tuple[str, ...] = ()

    def to_dict(self) -> dict[str, object]:
        return {"state": self.state, "containers": list(self.containers)}


def volume_attachments(engine: Engine, name: str) -> AttachmentReport:
    """Return exact attached container ids, or ``unknown`` without guessing."""
    result = engine.run(["ps", "--all", "--filter", f"volume={name}", "--quiet"])
    if not result.ok:
        return AttachmentReport("unknown")
    containers = tuple(line.strip() for line in result.stdout.splitlines() if line.strip())
    return AttachmentReport("attached" if containers else "detached", containers)


@dataclass(frozen=True)
class VolumeRecoveryPlan:
    """A serializable, non-mutating decision for one incomplete engine resource."""

    name: str
    raw_labels: dict[str, str]
    action: str
    reason: str
    recovery: str
    attachment: AttachmentReport | None = None
    expected: labels.ResourceLabels | None = None

    def to_dict(self) -> dict[str, object]:
        decision: dict[str, object] = {
            "action": self.action,
            "eligible": self.action == "would-recreate",
            "reason": self.reason,
            "recovery": self.recovery,
        }
        result: dict[str, object] = {
            "kind": "volume",
            "name": self.name,
            "registry_id": self.raw_labels.get(labels.REGISTRY),
            "label_keys": sorted(
                key for key in self.raw_labels if key.startswith(labels.NAMESPACE)
            ),
            "decision": decision,
        }
        if self.attachment is not None:
            result["attachment"] = self.attachment.to_dict()
        return result


def plan_unproven_resource(
    resource: DiscoveredResource,
    registry_id: str,
    engine: Engine,
    *,
    include_attachment: bool = True,
) -> dict[str, object]:
    """Describe an incomplete resource for GC without granting ownership.

    GC intentionally does not derive a manifest identity: it only reports that an
    operator may inspect the exact resource through ``reconcile-volume``.  This keeps
    collection non-mutating and avoids a broad name-based recovery surface.
    """
    raw = resource.raw_labels
    namespaced = sorted(key for key in raw if key.startswith(labels.NAMESPACE))
    reason = "incomplete ownership labels; protected from automatic recovery"
    recovery = "refused"
    result: dict[str, object] = {
        "kind": resource.kind,
        "name": resource.name,
        "registry_id": raw.get(labels.REGISTRY),
        "label_keys": namespaced,
        "decision": {
            "action": "protected",
            "eligible": False,
            "reason": reason,
            "recovery": recovery,
        },
    }
    if resource.kind == "volume":
        if include_attachment:
            attachment = volume_attachments(engine, resource.name)
            result["attachment"] = attachment.to_dict()
        if (
            raw.get(labels.REGISTRY) == registry_id
            and any(key != labels.REGISTRY for key in namespaced)
        ):
            result["decision"] = {
                "action": "protected",
                "eligible": False,
                "reason": reason,
                "recovery": "explicit-reconcile-available",
            }
    return result


def plan_legacy_volume_reconciliation(
    *,
    name: str,
    raw_labels: dict[str, str],
    expected: labels.ResourceLabels,
    registry_id: str,
    engine: Engine,
) -> VolumeRecoveryPlan:
    """Plan one explicit legacy recovery, never inferring authority from a name alone."""
    if labels.is_owned_by(raw_labels, registry_id):
        if any(expected.to_dict().get(key) != value for key, value in raw_labels.items()):
            return VolumeRecoveryPlan(
                name,
                raw_labels,
                "refused",
                "complete ownership labels belong to another manifest identity",
                "refused",
            )
        return VolumeRecoveryPlan(
            name,
            raw_labels,
            "already-owned",
            "complete ownership labels already match this registry",
            "none",
        )

    namespaced = {
        key: value for key, value in raw_labels.items() if key.startswith(labels.NAMESPACE)
    }
    if not namespaced:
        return VolumeRecoveryPlan(
            name, raw_labels, "refused", "volume has no Bosn ownership labels", "refused"
        )
    if raw_labels.get(labels.REGISTRY) != registry_id:
        return VolumeRecoveryPlan(
            name,
            raw_labels,
            "refused",
            "volume registry label is absent or belongs to another registry",
            "refused",
        )
    if len(namespaced) < 2:
        return VolumeRecoveryPlan(
            name,
            raw_labels,
            "refused",
            "volume has only a registry label; no independent Bosn identity survives",
            "refused",
        )

    expected_values = expected.to_dict()
    if any(expected_values.get(key) != value for key, value in namespaced.items()):
        return VolumeRecoveryPlan(
            name,
            raw_labels,
            "refused",
            "surviving Bosn labels contradict the selected manifest identity",
            "refused",
        )

    attachment = volume_attachments(engine, name)
    if attachment.state == "unknown":
        return VolumeRecoveryPlan(
            name,
            raw_labels,
            "refused",
            "could not prove whether the volume is attached",
            "refused",
            attachment,
        )
    if attachment.state == "attached":
        return VolumeRecoveryPlan(
            name,
            raw_labels,
            "refused",
            "volume is attached to a live or stopped container",
            "refused",
            attachment,
        )
    return VolumeRecoveryPlan(
        name,
        raw_labels,
        "would-recreate",
        "explicit legacy reconciliation is safe for this detached exact candidate",
        "explicit-reconcile-required",
        attachment,
        expected,
    )


def legacy_expected_labels(
    *,
    registry_id: str,
    stack: str,
    generation: str,
    scope: str,
    workspace: str,
    raw_labels: dict[str, str],
) -> labels.ResourceLabels:
    """Preserve a surviving creation timestamp; otherwise mark this reconciliation now."""
    return labels.ResourceLabels(
        registry=registry_id,
        kind="volume",
        stack=stack,
        generation=generation,
        scope=scope,
        workspace=workspace,
        created=raw_labels.get(labels.CREATED) or _now_iso(),
    )


def apply_legacy_volume_reconciliation(engine: Engine, plan: VolumeRecoveryPlan) -> str:
    """Apply a previously planned candidate after its caller has re-planned it."""
    if plan.action != "would-recreate" or plan.expected is None:
        raise TransferError(plan.reason)
    return recreate_volume_with_labels(
        engine, DiscoveredResource("volume", plan.name, plan.raw_labels), plan.expected
    )
