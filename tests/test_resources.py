"""Phase 4: enumeration, ownership bucketing, leases, adoption.

Unit tests drive a fake engine, so they run everywhere without Docker.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from bosn import labels, resources
from bosn.clock import FakeClock
from bosn.engine import EngineResult
from bosn.registry import Registry
from bosn.resources import DiscoveredResource, ResourceScanner, ScanResult

OURS = "our-registry-uuid"
THEIRS = "someone-elses-uuid"


def label_dict(registry: str = OURS, **overrides: str) -> dict[str, str]:
    base = labels.ResourceLabels(
        registry=registry,
        kind="volume",
        stack="test",
        generation="sha256:abc",
        scope="spec",
        workspace="/w",
        created="2026-08-13T00:00:00Z",
    ).to_dict()
    base.update(overrides)
    return base


class FakeEngine:
    """Records commands and replays canned output."""

    def __init__(self, listings: dict[str, list[dict]], inspects: dict[str, dict] | None = None):
        self.listings = listings
        self.inspects = inspects or {}
        self.commands: list[list[str]] = []

    def run(self, args: list[str], *, check: bool = False) -> EngineResult:
        self.commands.append(list(args))
        if "inspect" in args:
            name = args[-1]
            return EngineResult(0, json.dumps(self.inspects.get(name, {})), "")
        kind = (
            "volume"
            if args[0] == "volume"
            else "image"
            if args[0] == "images"
            else "container"
            if args[0] == "ps"
            else None
        )
        rows = self.listings.get(kind or "", [])
        return EngineResult(0, "\n".join(json.dumps(row) for row in rows), "")


# -- ownership bucketing ---------------------------------------------------


def test_scan_sorts_resources_into_owned_foreign_and_unlabeled() -> None:
    engine = FakeEngine(
        {
            "volume": [
                {"Name": "ours", "Labels": json.dumps(label_dict())},
                {"Name": "theirs", "Labels": json.dumps(label_dict(registry=THEIRS))},
                {"Name": "naked", "Labels": ""},
            ]
        }
    )
    scan = ResourceScanner(engine).scan(OURS, kinds=["volume"])  # type: ignore[arg-type]

    assert [r.name for r in scan.owned] == ["ours"]
    assert [r.name for r in scan.foreign] == ["theirs"]
    assert [r.name for r in scan.unlabeled] == ["naked"]
    assert scan.foreign_registries == {THEIRS}
    assert scan.counts() == {"owned": 1, "foreign": 1, "unlabeled": 1}


def test_partially_labeled_resources_are_unlabeled_not_owned() -> None:
    """An incomplete label set is never ownership proof, even with our registry id."""
    partial = label_dict()
    del partial[labels.STACK]
    engine = FakeEngine({"volume": [{"Name": "partial", "Labels": json.dumps(partial)}]})
    scan = ResourceScanner(engine).scan(OURS, kinds=["volume"])  # type: ignore[arg-type]

    assert scan.owned == []
    assert [r.name for r in scan.unlabeled] == ["partial"]


def test_a_bosn_name_prefix_alone_is_not_ownership() -> None:
    engine = FakeEngine({"volume": [{"Name": "bosn-target-cache", "Labels": ""}]})
    scan = ResourceScanner(engine).scan(OURS, kinds=["volume"])  # type: ignore[arg-type]
    assert scan.owned == []
    assert len(scan.unlabeled) == 1


def test_labels_are_confirmed_by_inspect_when_the_listing_truncates_them() -> None:
    engine = FakeEngine(
        {"volume": [{"Name": "ours", "Labels": ""}]},
        inspects={"ours": label_dict()},
    )
    scan = ResourceScanner(engine).scan(OURS, kinds=["volume"])  # type: ignore[arg-type]
    assert [r.name for r in scan.owned] == ["ours"]
    assert any("inspect" in cmd for cmd in engine.commands)


@pytest.mark.parametrize(
    "blob",
    ["", "null", "<no value>", "map[]", "not json at all {", "[1,2,3]"],
)
def test_unparseable_label_blobs_never_produce_ownership(blob: str) -> None:
    assert resources._parse_labels(blob) == {} or not labels.is_owned_by(
        resources._parse_labels(blob), OURS
    )


def test_comma_separated_label_format_is_parsed() -> None:
    raw = ",".join(f"{k}={v}" for k, v in label_dict().items())
    parsed = resources._parse_labels(raw)
    assert labels.is_owned_by(parsed, OURS)


def test_engine_failure_yields_no_resources_rather_than_guesses() -> None:
    class Failing:
        def run(self, args, *, check=False):
            return EngineResult(1, "", "engine down")

    scan = ResourceScanner(Failing()).scan(OURS, kinds=["volume"])  # type: ignore[arg-type]
    assert scan.counts() == {"owned": 0, "foreign": 0, "unlabeled": 0}


# -- leases ----------------------------------------------------------------


@pytest.fixture
def clock() -> FakeClock:
    return FakeClock()


@pytest.fixture
def registry(tmp_path: Path, clock: FakeClock):
    with Registry(tmp_path / "r.sqlite3", clock=clock) as reg:
        yield reg


def _resource(registry: Registry):
    return registry.register_resource(
        kind="volume", name="v", stack="s", generation="g", scope="spec", workspace="/w"
    )


def test_a_live_holder_keeps_its_lease_past_the_ttl(registry: Registry, clock: FakeClock) -> None:
    """A 40-minute build is never collected out from under itself."""
    resource = _resource(registry)
    lease = registry.acquire_lease(resource.id, pid=999, proc_start=1.0, ttl_seconds=60)
    clock.advance(10_000)

    assert lease.expired_by_time(clock.now())  # TTL elapsed...
    assert not resources.lease_is_expired(  # ...but the holder is alive
        lease, clock.now(), alive_probe=lambda pid, start=None: True
    )
    assert resources.resource_is_leased(
        registry, resource.id, alive_probe=lambda pid, start=None: True
    )


def test_a_dead_holder_releases_after_one_ttl(registry: Registry, clock: FakeClock) -> None:
    resource = _resource(registry)
    lease = registry.acquire_lease(resource.id, pid=999, proc_start=1.0, ttl_seconds=60)
    dead = lambda pid, start=None: False  # noqa: E731

    clock.advance(59)
    assert not resources.lease_is_expired(lease, clock.now(), alive_probe=dead)

    clock.advance(2)
    assert resources.lease_is_expired(lease, clock.now(), alive_probe=dead)
    assert not resources.resource_is_leased(registry, resource.id, alive_probe=dead)


def test_process_alive_probe_is_true_for_this_process() -> None:
    import os

    assert resources.process_alive(os.getpid())
    assert not resources.process_alive(2**31 - 1)
    assert not resources.process_alive(0)


# -- adoption --------------------------------------------------------------


def test_adoption_rebuilds_registry_rows_from_labels(registry: Registry, clock: FakeClock) -> None:
    """Losing the database is survivable: ownership lives in the labels."""
    scan = ScanResult(
        owned=[
            DiscoveredResource("volume", "ours-1", label_dict()),
            DiscoveredResource("volume", "ours-2", label_dict()),
        ],
        foreign=[DiscoveredResource("volume", "theirs", label_dict(registry=THEIRS))],
        unlabeled=[DiscoveredResource("volume", "naked", {})],
    )
    adopted = resources.adopt(registry, scan, clock=clock)

    assert sorted(adopted) == ["ours-1", "ours-2"]
    names = {r.name for r in registry.list_resources()}
    assert names == {"ours-1", "ours-2"}, "foreign and unlabeled must never be adopted"


def test_adoption_is_idempotent(registry: Registry, clock: FakeClock) -> None:
    scan = ScanResult(owned=[DiscoveredResource("volume", "ours-1", label_dict())])
    assert resources.adopt(registry, scan, clock=clock) == ["ours-1"]
    assert resources.adopt(registry, scan, clock=clock) == []
    assert len(registry.list_resources()) == 1


def test_adopted_resources_are_protected_by_the_quiet_period(clock: FakeClock) -> None:
    """Recovery is never followed by a mass age-out."""
    adopted_at = clock.now()
    assert resources.within_quiet_period(adopted_at, clock.advance(23 * 3600))
    assert not resources.within_quiet_period(adopted_at, clock.advance(2 * 3600))
