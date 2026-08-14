"""Phase 6: GC safety properties. Fake engine, clock-injected, no Docker."""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from bosn import labels
from bosn.clock import FakeClock
from bosn.engine import EngineResult
from bosn.gc import Collector, done_workspaces, mark_done, status
from bosn.registry import Registry
from bosn.retention import DAY

OURS = "our-registry"


class FakeEngine:
    """Serves labels per resource name and records removals."""

    def __init__(self, label_map: dict[str, dict[str, str]]):
        self.label_map = label_map
        self.commands: list[list[str]] = []
        self.fail_on: set[str] = set()

    def run(self, args: list[str], *, check: bool = False) -> EngineResult:
        self.commands.append(list(args))
        if "inspect" in args:
            return EngineResult(0, json.dumps(self.label_map.get(args[-1], {})), "")
        if args[0] in {"rm", "volume", "image"} and "rm" in args:
            name = args[-1]
            if name in self.fail_on:
                return EngineResult(1, "", "device or resource busy")
            return EngineResult(0, "", "")
        return EngineResult(0, "", "")

    def removals(self) -> list[str]:
        return [c[-1] for c in self.commands if "rm" in c]


def label_dict(registry: str = OURS, **overrides: str) -> dict[str, str]:
    base = labels.ResourceLabels(
        registry=registry,
        kind="volume",
        stack="s",
        generation="g",
        scope="spec",
        workspace="/w",
        created="2026-08-13T00:00:00Z",
    ).to_dict()
    base.update(overrides)
    return base


@pytest.fixture
def clock() -> FakeClock:
    return FakeClock()


@pytest.fixture
def registry(tmp_path: Path, clock: FakeClock):
    with Registry(tmp_path / "r.sqlite3", clock=clock) as reg:
        yield reg


def add(registry: Registry, name: str, scope="spec", workspace="/w"):
    return registry.register_resource(
        kind="volume", name=name, stack="s", generation="g", scope=scope, workspace=workspace
    )


# -- ownership proof is required at delete time ----------------------------


def test_gc_reconfirms_ownership_from_the_engine_labels(
    registry: Registry, clock: FakeClock
) -> None:
    """The registry is a hint; the labels are the authority."""
    add(registry, "ours")
    add(registry, "not-really-ours")
    engine = FakeEngine(
        {
            "ours": label_dict(registry=registry.registry_id),
            "not-really-ours": label_dict(registry="someone-else"),
        }
    )
    clock.advance(10 * DAY)

    result = Collector(registry, engine).collect(dry_run=False)  # type: ignore[arg-type]

    assert result.removed == ["ours"]
    assert result.skipped_unproven == ["not-really-ours"]
    assert "not-really-ours" not in engine.removals()


def test_a_resource_with_incomplete_labels_is_never_removed(
    registry: Registry, clock: FakeClock
) -> None:
    add(registry, "partial")
    partial = label_dict(registry=registry.registry_id)
    del partial[labels.STACK]
    engine = FakeEngine({"partial": partial})
    clock.advance(10 * DAY)

    result = Collector(registry, engine).collect(dry_run=False)  # type: ignore[arg-type]
    assert result.removed == []
    assert result.skipped_unproven == ["partial"]


def test_gc_never_issues_a_system_prune(registry: Registry, clock: FakeClock) -> None:
    add(registry, "ours")
    engine = FakeEngine({"ours": label_dict(registry=registry.registry_id)})
    clock.advance(10 * DAY)

    Collector(registry, engine).collect(dry_run=False)  # type: ignore[arg-type]

    flat = [" ".join(cmd) for cmd in engine.commands]
    assert not any("prune" in cmd for cmd in flat)
    assert not any("system" in cmd for cmd in flat)


def test_there_is_no_force_flag_on_gc() -> None:
    """Automatic deletion requires ownership proof, so no flag may skip it."""
    from bosn.cli import build_parser

    help_text = build_parser().parse_args(["gc"]).__dict__
    assert "force" not in help_text
    with pytest.raises(SystemExit):
        build_parser().parse_args(["gc", "--force"])


# -- dry run ---------------------------------------------------------------


def test_dry_run_removes_nothing(registry: Registry, clock: FakeClock) -> None:
    add(registry, "ours")
    engine = FakeEngine({"ours": label_dict(registry=registry.registry_id)})
    clock.advance(10 * DAY)

    result = Collector(registry, engine).collect(dry_run=True)  # type: ignore[arg-type]

    assert result.removed == ["ours"], "dry run still reports what would go"
    assert engine.removals() == []
    assert len(registry.list_resources()) == 1


# -- errors are observable -------------------------------------------------


def test_removal_errors_are_recorded_not_discarded(registry: Registry, clock: FakeClock) -> None:
    """A discarded error is how you come to believe storage is bounded when it is not."""
    add(registry, "stubborn")
    engine = FakeEngine({"stubborn": label_dict(registry=registry.registry_id)})
    engine.fail_on.add("stubborn")
    clock.advance(10 * DAY)

    result = Collector(registry, engine).collect(dry_run=False)  # type: ignore[arg-type]

    assert result.removed == []
    assert len(result.errors) == 1
    assert "busy" in result.errors[0]
    assert any(row["kind"] == "gc.error" for row in registry.events())
    assert len(registry.list_resources()) == 1, "a failed removal must not deregister"


# -- done ------------------------------------------------------------------


def test_done_marks_only_this_workspace(registry: Registry) -> None:
    add(registry, "mine", workspace="/w1")
    add(registry, "theirs", workspace="/w2")
    add(registry, "shared", scope="machine", workspace="/w1")

    assert mark_done(registry, "/w1") == 1
    assert done_workspaces(registry) == {"/w1"}
    states = {r.name: r.state for r in registry.list_resources()}
    assert states["mine"] == "done"
    assert states["theirs"] == "active"
    assert states["shared"] == "active", "machine caches are never workspace-owned"


# -- status ----------------------------------------------------------------


def test_status_reports_counts_and_foreign_registries(registry: Registry) -> None:
    add(registry, "ours")
    engine = FakeEngine({"ours": label_dict(registry=registry.registry_id)})
    report = status(registry, engine)  # type: ignore[arg-type]

    assert report["registry_id"] == registry.registry_id
    assert report["registered"] == 1
    assert "by_reason" in report
    assert isinstance(report["foreign_registries"], list)
    assert report["managed_bytes"] == 0
    assert report["attribution"] == [
        {"workspace": "/w", "stack": "s", "role": "volume", "count": 1, "bytes": 0, "unmeasured": 1}
    ]
    assert report["decisions"][0]["reason"] == "warm"
