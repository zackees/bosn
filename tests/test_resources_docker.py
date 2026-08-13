"""Phase 4: enumeration against a real engine. Docker-marked: Linux CI only.

Creates genuinely labeled volumes and proves the scanner sorts them correctly -- most
importantly that a foreign-registry volume and an unlabeled volume are both untouched by
every ownership decision.
"""

from __future__ import annotations

import uuid
from collections.abc import Iterator

import pytest

from bosn import labels
from bosn.engine import Engine
from bosn.resources import ResourceScanner

OURS = f"bosn-test-{uuid.uuid4()}"
THEIRS = f"bosn-test-{uuid.uuid4()}"

pytestmark = pytest.mark.docker


def _labels(registry: str) -> labels.ResourceLabels:
    return labels.ResourceLabels(
        registry=registry,
        kind="volume",
        stack="test",
        generation="sha256:deadbeef",
        scope="spec",
        workspace="/w/test",
        created="2026-08-13T00:00:00Z",
    )


@pytest.fixture
def volumes(engine: Engine) -> Iterator[dict[str, str]]:
    """Three volumes: ours, a foreign registry's, and one with no labels at all."""
    names = {
        "owned": f"bosn-owned-{uuid.uuid4().hex[:8]}",
        "foreign": f"bosn-foreign-{uuid.uuid4().hex[:8]}",
        "unlabeled": f"bosn-naked-{uuid.uuid4().hex[:8]}",
    }
    engine.run(["volume", "create", *_labels(OURS).to_docker_args(), names["owned"]], check=True)
    engine.run(
        ["volume", "create", *_labels(THEIRS).to_docker_args(), names["foreign"]], check=True
    )
    engine.run(["volume", "create", names["unlabeled"]], check=True)
    try:
        yield names
    finally:
        for name in names.values():
            engine.run(["volume", "rm", "--force", name])


def test_scan_finds_our_volume_and_isolates_the_others(
    engine: Engine, volumes: dict[str, str]
) -> None:
    scan = ResourceScanner(engine).scan(OURS, kinds=["volume"])

    owned = {r.name for r in scan.owned}
    foreign = {r.name for r in scan.foreign}
    unlabeled = {r.name for r in scan.unlabeled}

    assert volumes["owned"] in owned
    assert volumes["foreign"] in foreign
    assert volumes["unlabeled"] in unlabeled

    # the safety property: nothing but ours is ever eligible
    assert volumes["foreign"] not in owned
    assert volumes["unlabeled"] not in owned


def test_foreign_registries_are_reported_not_hidden(
    engine: Engine, volumes: dict[str, str]
) -> None:
    scan = ResourceScanner(engine).scan(OURS, kinds=["volume"])
    assert THEIRS in scan.foreign_registries


def test_labels_survive_a_round_trip_through_the_engine(
    engine: Engine, volumes: dict[str, str]
) -> None:
    raw = ResourceScanner(engine).inspect_labels("volume", volumes["owned"])
    assert labels.is_owned_by(raw, OURS)
    assert labels.ResourceLabels.from_dict(raw) == _labels(OURS)


def test_the_other_registrys_scan_sees_ours_as_foreign(
    engine: Engine, volumes: dict[str, str]
) -> None:
    """Ownership is symmetric: from their side, our volume is the foreign one."""
    scan = ResourceScanner(engine).scan(THEIRS, kinds=["volume"])
    assert volumes["foreign"] in {r.name for r in scan.owned}
    assert volumes["owned"] in {r.name for r in scan.foreign}
