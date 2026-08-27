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

# ``kind`` and ``created`` describe an object but do not bind it to the selected manifest
# target.  Legacy reconciliation needs one surviving identity discriminator in addition to
# the registry id; names are never evidence of ownership.
_MANIFEST_DISCRIMINATORS = (
    labels.STACK,
    labels.GENERATION,
    labels.SCOPE,
    labels.WORKSPACE,
)


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


def has_legacy_corroboration(
    raw_labels: dict[str, str], registry_id: str, expected: labels.ResourceLabels | None = None
) -> bool:
    """Return whether surviving labels carry the minimum legacy recovery evidence.

    With a manifest-derived expected contract this is the strict mutation predicate:
    registry match, all surviving Bosn labels agree, and one surviving manifest-binding
    discriminator agrees.  GC deliberately has no expected contract, so it can only
    advertise that explicit inspection is available when a discriminator key survives.
    """
    if raw_labels.get(labels.REGISTRY) != registry_id:
        return False
    if expected is None:
        return any(raw_labels.get(key) for key in _MANIFEST_DISCRIMINATORS)
    expected_values = expected.to_dict()
    surviving = {
        key: value for key, value in raw_labels.items() if key.startswith(labels.NAMESPACE)
    }
    return (
        bool(surviving)
        and all(expected_values.get(key) == value for key, value in surviving.items())
        and any(raw_labels.get(key) == expected_values[key] for key in _MANIFEST_DISCRIMINATORS)
    )


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
        if has_legacy_corroboration(raw, registry_id):
            result["decision"] = {
                "action": "protected",
                "eligible": False,
                "reason": reason,
                "recovery": "explicit-reconcile-inspection-available",
            }
    return result


def plan_manifest_volume_collision(
    name: str, raw_labels: dict[str, str], registry_id: str, engine: Engine
) -> dict[str, object] | None:
    """Describe an existing manifest-derived volume without treating its name as proof.

    The caller has already proved that this exact name exists in the engine.  A complete
    label contract belongs to normal ownership/foreign-registry reporting; only incomplete
    and unlabeled collisions appear here, and they remain protected in both GC modes.
    """
    if labels.is_complete(raw_labels):
        return None
    if raw_labels:
        return plan_unproven_resource(
            DiscoveredResource("volume", name, raw_labels), registry_id, engine
        )
    return {
        "kind": "volume",
        "name": name,
        "registry_id": None,
        "label_keys": [],
        "decision": {
            "action": "protected",
            "eligible": False,
            "reason": "manifest-derived name collision has no Bosn ownership labels",
            "recovery": "refused",
        },
        "attachment": volume_attachments(engine, name).to_dict(),
    }


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
        if not has_legacy_corroboration(raw_labels, registry_id, expected):
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
    if not has_legacy_corroboration(raw_labels, registry_id, expected):
        return VolumeRecoveryPlan(
            name,
            raw_labels,
            "refused",
            "surviving Bosn labels lack a matching manifest-binding identity or contradict it",
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
