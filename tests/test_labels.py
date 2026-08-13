"""Phase 2: the label contract is ownership proof; nothing weaker is."""

from __future__ import annotations

import pytest

from bosn import labels
from bosn.labels import LabelError, ResourceLabels


def make_labels(**overrides: str) -> ResourceLabels:
    base = dict(
        registry="reg-uuid-1",
        kind="volume",
        stack="test",
        generation="sha256:abc",
        scope="spec",
        workspace="/w/one",
        created="2026-08-13T00:00:00Z",
    )
    base.update(overrides)
    return ResourceLabels(**base)  # type: ignore[arg-type]


def test_round_trip_through_dict() -> None:
    original = make_labels()
    assert ResourceLabels.from_dict(original.to_dict()) == original


def test_docker_args_render_every_required_label() -> None:
    args = make_labels().to_docker_args()
    assert args.count("--label") == len(labels.REQUIRED_LABELS)
    rendered = " ".join(args)
    for key in labels.REQUIRED_LABELS:
        assert f"{key}=" in rendered


@pytest.mark.parametrize("field", ["kind", "scope"])
def test_unknown_enum_values_are_rejected(field: str) -> None:
    with pytest.raises(LabelError):
        make_labels(**{field: "nonsense"})


def test_empty_registry_id_is_rejected() -> None:
    with pytest.raises(LabelError):
        make_labels(registry="")


@pytest.mark.parametrize("missing", labels.REQUIRED_LABELS)
def test_incomplete_label_set_is_never_ownership_proof(missing: str) -> None:
    raw = make_labels().to_dict()
    del raw[missing]
    assert not labels.is_complete(raw)
    # even with a matching registry id, an incomplete set is not proof
    assert not labels.is_owned_by(raw, "reg-uuid-1")
    with pytest.raises(LabelError):
        ResourceLabels.from_dict(raw)


def test_foreign_registry_is_not_owned() -> None:
    raw = make_labels(registry="someone-elses-uuid").to_dict()
    assert labels.is_complete(raw)
    assert not labels.is_owned_by(raw, "reg-uuid-1")


def test_name_prefix_is_not_ownership_proof() -> None:
    assert not labels.is_owned_by({"com.zackees.bosn.name": "bosn-cache-x"}, "reg-uuid-1")


def test_own_complete_labels_are_owned() -> None:
    assert labels.is_owned_by(make_labels().to_dict(), "reg-uuid-1")
