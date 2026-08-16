"""Issue #106: an unmeasurable byte inventory must not read as "zero bytes, no pressure".

`accounting.StorageInventory.collect` failing (or returning unusable output) used to come
back as an empty `sizes` dict indistinguishable from a genuinely empty, healthy host. That
fed `gc.Collector.collect`/`gc.status`'s `Pressure.assess` a `managed_bytes=0`, so byte
pressure silently read as absent on exactly the busy hosts most likely to need it detected.

These tests exercise the whole path -- fake engine -> `Collector`/`status` -> `Pressure` --
rather than just the accounting or retention units in isolation, since the bug was in how
those two pieces were wired together, not in either one alone. `FakeEngine` and `label_dict`
are reused from `test_gc.py` (same directory, no package boundary) rather than reimplemented,
since getting the label namespace (`com.zackees.bosn.*`) and ownership shape wrong here would
just be a second copy of a fixture that already exists and is exercised elsewhere.
"""

from __future__ import annotations

from pathlib import Path

import pytest

from bosn.accounting import StorageInventory
from bosn.clock import FakeClock
from bosn.config import load as load_config
from bosn.gc import Collector, status
from bosn.registry import Registry
from test_gc import OURS, FakeEngine, label_dict


@pytest.fixture
def clock() -> FakeClock:
    return FakeClock()


@pytest.fixture
def registry(tmp_path: Path, clock: FakeClock):
    with Registry(tmp_path / "r.sqlite3", clock=clock) as reg:
        yield reg


def add(registry: Registry, name: str = "cache"):
    return registry.register_resource(
        kind="volume", name=name, stack="s", generation="g", scope="spec", workspace="/w"
    )


def failing_df_engine() -> FakeEngine:
    """A `FakeEngine` whose `system df -v` call fails; every other call it needs succeeds
    via `FakeEngine`'s existing fallback (`return EngineResult(0, "", "")` for anything not
    explicitly handled), which for `system df -v` gives an *empty but ok* result -- exactly
    the "ok exit, zero parseable rows" shape `StorageInventory.collect` now also treats as
    unmeasured, so the default `FakeEngine` already exercises this bug without modification.
    """
    return FakeEngine({"cache": label_dict(registry=OURS)})


@pytest.fixture(autouse=True)
def _matching_registry_id(registry: Registry) -> None:
    # `label_dict`'s default owner is the fixed string `OURS` ("our-registry"); make the
    # registry under test agree, so ownership proof in `Collector._ownership_proven`
    # succeeds without every test having to thread a matching id through by hand.
    registry.set_meta("registry_id", OURS)


def test_inventory_collection_failure_is_logged_as_an_event(registry: Registry) -> None:
    """Visibility: an unmeasurable inventory must be visible in the event log, not inferred
    from GC quietly doing nothing."""
    add(registry)
    engine = failing_df_engine()

    Collector(registry, engine).collect(dry_run=True)  # type: ignore[arg-type]

    kinds = [row["kind"] for row in registry.events()]
    assert "gc.inventory_unmeasured" in kinds


def test_a_successful_pass_never_logs_the_unmeasured_event(registry: Registry, monkeypatch) -> None:
    """Regression guard for the event itself: a healthy measurement must stay silent."""
    add(registry)
    engine = failing_df_engine()
    monkeypatch.setattr(
        "bosn.gc.StorageInventory.collect",
        classmethod(lambda _cls, _engine: StorageInventory({("volume", "cache"): 10})),
    )

    Collector(registry, engine).collect(dry_run=True)  # type: ignore[arg-type]

    kinds = [row["kind"] for row in registry.events()]
    assert "gc.inventory_unmeasured" not in kinds


def test_an_unmeasurable_inventory_does_not_present_as_zero_bytes_no_pressure(
    registry: Registry,
) -> None:
    """The distinction survives into `Pressure`: assert on the resulting decision (via
    `gc.status`'s reported pressure), not just on the `StorageInventory` object -- the
    inventory is an internal detail, the decision is what actually governs deletion."""
    add(registry)
    engine = failing_df_engine()
    config = load_config(flags={"shared_cache_ceiling": 1})

    report = status(registry, engine, config=config)  # type: ignore[arg-type]

    # Not zero-confirmed-under-ceiling: the byte signal must read as abstained, not cleared.
    assert report["pressure"]["bytes_exceeded"] is False
    assert report["pressure"]["bytes_unknown"] is True
    assert report["managed_bytes"] == 0


def test_an_unmeasurable_inventory_never_triggers_a_deletion_a_measured_zero_would_not(
    registry: Registry,
) -> None:
    """The safe direction is "refuse to conclude no pressure", never "assume there is
    pressure and start deleting": with a tiny ceiling that a real byte measurement would
    have exceeded, an *unmeasured* inventory must not manufacture that same eviction."""
    add(registry)
    engine = failing_df_engine()
    config = load_config(flags={"shared_cache_ceiling": 1})

    result = Collector(registry, engine, config=config).collect(dry_run=False)  # type: ignore[arg-type]

    assert result.removed == []


def test_a_successful_measurement_over_ceiling_still_evicts_exactly_as_before(
    registry: Registry, monkeypatch
) -> None:
    """The regression that matters most: existing pressure-driven deletion on a genuinely
    successful measurement must be unchanged by threading `bytes_measured` through."""
    add(registry)
    engine = failing_df_engine()
    monkeypatch.setattr(
        "bosn.gc.StorageInventory.collect",
        classmethod(lambda _cls, _engine: StorageInventory({("volume", "cache"): 100})),
    )
    config = load_config(flags={"shared_cache_ceiling": 10})

    result = Collector(registry, engine, config=config).collect(dry_run=False)  # type: ignore[arg-type]

    assert result.removed == ["cache"]
    report = status(registry, engine, config=config)  # type: ignore[arg-type]
    assert report["pressure"]["bytes_unknown"] is False
