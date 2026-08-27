"""Phase 6: GC safety properties. Fake engine, clock-injected, no Docker."""

from __future__ import annotations

import json
import os
import threading
from collections.abc import Callable
from pathlib import Path

import pytest

from bosn import gc as gc_mod
from bosn import labels
from bosn.accounting import StorageInventory, StorageProbe
from bosn.clock import FakeClock
from bosn.config import load as load_config
from bosn.engine import EngineResult
from bosn.gc import Collector, done_workspaces, mark_done, status
from bosn.registry import Registry
from bosn.retention import (
    COLLECT_SUPERSEDED_IMAGE,
    DAY,
    KEPT_CURRENT_IMAGE,
    KEPT_IMAGE_DEPENDENCY_UNKNOWN,
    KEPT_IMAGE_REFERENCED,
    KEPT_RUNNING,
)
from conftest import live_proc_start

OURS = "our-registry"


class _DefaultContainerImages:
    pass


_DEFAULT_CONTAINER_IMAGES = _DefaultContainerImages()


class FakeEngine:
    """Serves labels per resource name and records removals."""

    def __init__(
        self,
        label_map: dict[str, dict[str, str]],
        *,
        running: frozenset[str] | None = frozenset(),
        container_images: dict[str, str] | None | _DefaultContainerImages = (
            _DEFAULT_CONTAINER_IMAGES
        ),
    ):
        self.label_map = label_map
        self.commands: list[list[str]] = []
        self.fail_on: set[str] = set()
        self.on_inspect: Callable[[], None] | None = None
        self.lease_acquired = threading.Event()
        self.lease_was_active_when_stopped = False
        # `None` means "the engine could not answer `docker ps`" -- distinct from an empty
        # set, which means "the engine answered: nothing is running". See
        # `resources.running_container_names` for the contract this mirrors.
        self.running = running
        self.container_images: dict[str, str] | None
        if isinstance(container_images, _DefaultContainerImages):
            self.container_images = {}
        else:
            self.container_images = container_images

    def run(self, args: list[str], *, check: bool = False) -> EngineResult:
        self.commands.append(list(args))
        if args == ["ps", "--format", "{{json .}}"]:
            if self.running is None:
                return EngineResult(1, "", "engine unreachable")
            lines = "\n".join(json.dumps({"Names": name}) for name in self.running)
            return EngineResult(0, lines, "")
        if args == ["ps", "--all", "--quiet", "--no-trunc"]:
            if self.container_images is None:
                return EngineResult(1, "", "engine unreachable")
            return EngineResult(0, "\n".join(self.container_images), "")
        if args[:2] == ["container", "inspect"] and "--format" in args:
            if self.container_images is None:
                return EngineResult(1, "", "engine unreachable")
            rows = [
                json.dumps({"Name": f"/{name}", "Image": self.container_images[name]})
                for name in args[args.index("--format") + 2 :]
                if name in self.container_images
            ]
            return EngineResult(0, "\n".join(rows), "")
        if "inspect" in args:
            if self.on_inspect is not None:
                self.on_inspect()
            return EngineResult(0, json.dumps(self.label_map.get(args[-1], {})), "")
        if args[:2] == ["container", "stop"]:
            self.lease_was_active_when_stopped = self.lease_acquired.is_set()
            name = args[-1]
            if name in self.fail_on:
                return EngineResult(1, "", "stop failed")
            return EngineResult(0, name, "")
        if args[0] in {"rm", "volume", "image", "network"} and "rm" in args:
            name = args[-1]
            if name in self.fail_on:
                return EngineResult(1, "", "device or resource busy")
            if args[0] == "rm" and isinstance(self.container_images, dict):
                self.container_images.pop(name, None)
            return EngineResult(0, "", "")
        return EngineResult(0, "", "")

    def removals(self) -> list[str]:
        return [c[-1] for c in self.commands if "rm" in c]


def label_dict(registry: str = OURS, kind: str = "volume", **overrides: str) -> dict[str, str]:
    base = labels.ResourceLabels(
        registry=registry,
        kind=kind,
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


def add(registry: Registry, name: str, scope="spec", workspace="/w", kind="volume"):
    return registry.register_resource(
        kind=kind, name=name, stack="s", generation="g", scope=scope, workspace=workspace
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


def test_status_names_unproven_resources_and_execution_sessions(
    registry: Registry, monkeypatch: pytest.MonkeyPatch
) -> None:
    """#119/#120: neither protected state may be hidden behind aggregate counts."""
    from bosn.registry import ExecutionSession

    registry.save_execution_session(
        ExecutionSession("session", "immutable-id", "docker", 4242, 9.0, ("lease",))
    )
    engine = FakeEngine({})
    monkeypatch.setattr(gc_mod.resources, "process_alive", lambda *_args: False)
    from bosn.resources import DiscoveredResource, ScanResult

    monkeypatch.setattr(
        gc_mod.ResourceScanner,
        "scan",
        lambda *_args, **_kwargs: ScanResult(
            unlabeled=[
                DiscoveredResource("volume", "partial", {labels.REGISTRY: registry.registry_id})
            ],
            scanned_kinds={"volume"},
        ),
    )
    report = status(registry, engine)  # type: ignore[arg-type]

    assert report["unproven_resources"] == [
        {
            "kind": "volume",
            "name": "partial",
            "registry_id": registry.registry_id,
            "label_keys": [labels.REGISTRY],
            "decision": {
                "action": "protected",
                "eligible": False,
                "reason": "incomplete ownership labels; protected from automatic recovery",
                "recovery": "refused",
            },
        }
    ]
    assert report["execution_sessions"] == [
        {
            "id": "session",
            "container_id": "immutable-id",
            "engine": "docker",
            "client_pid": 4242,
            "client_start": 9.0,
            "client_alive": False,
            "lease_ids": ["lease"],
            "blocking_reason": "client is dead; awaiting safe exact-container reap",
        }
    ]


@pytest.mark.parametrize(
    ("extra_key", "extra_value"),
    [(labels.KIND, "volume"), (labels.CREATED, "2026-08-26T00:00:00Z")],
)
def test_gc_dry_run_does_not_offer_inspection_for_nonbinding_labels(
    registry: Registry, monkeypatch: pytest.MonkeyPatch, extra_key: str, extra_value: str
) -> None:
    """#120: apply and preview both protect incomplete engine resources."""
    from bosn.resources import DiscoveredResource, ScanResult

    partial = {labels.REGISTRY: registry.registry_id, extra_key: extra_value}
    monkeypatch.setattr(
        gc_mod.ResourceScanner,
        "scan",
        lambda *_args, **_kwargs: ScanResult(
            unlabeled=[DiscoveredResource("volume", "partial", partial)],
            scanned_kinds={"volume"},
        ),
    )
    engine = FakeEngine({"partial": partial})
    result = Collector(registry, engine).collect(dry_run=True)  # type: ignore[arg-type]

    assert result.removed == []
    assert result.unproven_resources == [
        {
            "kind": "volume",
            "name": "partial",
            "registry_id": registry.registry_id,
            "label_keys": sorted([extra_key, labels.REGISTRY]),
            "decision": {
                "action": "protected",
                "eligible": False,
                "reason": "incomplete ownership labels; protected from automatic recovery",
                "recovery": "refused",
            },
            "attachment": {"state": "detached", "containers": []},
        }
    ]
    assert "partial" not in engine.removals()


def test_gc_only_advertises_legacy_inspection_for_a_manifest_discriminator(
    registry: Registry, monkeypatch: pytest.MonkeyPatch
) -> None:
    from bosn.resources import DiscoveredResource, ScanResult

    partial = {labels.REGISTRY: registry.registry_id, labels.STACK: "perf"}
    monkeypatch.setattr(
        gc_mod.ResourceScanner,
        "scan",
        lambda *_args, **_kwargs: ScanResult(
            unlabeled=[DiscoveredResource("volume", "partial", partial)],
            scanned_kinds={"volume"},
        ),
    )
    collector = Collector(registry, FakeEngine({"partial": partial}))  # type: ignore[arg-type]
    result = collector.collect(dry_run=True)

    assert result.unproven_resources[0]["decision"] == {
        "action": "protected",
        "eligible": False,
        "reason": "incomplete ownership labels; protected from automatic recovery",
        "recovery": "explicit-reconcile-inspection-available",
    }


def test_gc_never_issues_a_system_prune(registry: Registry, clock: FakeClock) -> None:
    add(registry, "ours")
    engine = FakeEngine({"ours": label_dict(registry=registry.registry_id)})
    clock.advance(10 * DAY)

    Collector(registry, engine).collect(dry_run=False)  # type: ignore[arg-type]

    flat = [" ".join(cmd) for cmd in engine.commands]
    assert not any("prune" in cmd for cmd in flat)
    assert not any("system prune" in cmd for cmd in flat)


def test_gc_computes_configured_storage_pressure_and_stops_at_target(
    monkeypatch, registry: Registry
) -> None:
    first = add(registry, "first")
    second = add(registry, "second")
    engine = FakeEngine(
        {
            "first": label_dict(registry=registry.registry_id),
            "second": label_dict(registry=registry.registry_id),
        }
    )
    monkeypatch.setattr(
        gc_mod.StorageInventory,
        "collect",
        classmethod(
            lambda _cls, _engine: StorageInventory(
                {("volume", "first"): 10, ("volume", "second"): 10}
            )
        ),
    )
    config = load_config(flags={"shared_cache_ceiling": 10})

    result = Collector(registry, engine, config=config).collect(dry_run=False)  # type: ignore[arg-type]

    assert result.removed == [first.name]
    assert registry.get_resource(second.id) is not None


def test_pressure_dry_run_matches_bounded_apply_plan(monkeypatch, registry: Registry) -> None:
    first = add(registry, "first")
    add(registry, "second")
    engine = FakeEngine(
        {
            "first": label_dict(registry=registry.registry_id),
            "second": label_dict(registry=registry.registry_id),
        }
    )
    monkeypatch.setattr(
        gc_mod.StorageInventory,
        "collect",
        classmethod(
            lambda _cls, _engine: StorageInventory(
                {("volume", "first"): 10, ("volume", "second"): 10}
            )
        ),
    )
    config = load_config(flags={"shared_cache_ceiling": 10})

    preview = Collector(registry, engine, config=config).collect(dry_run=True)  # type: ignore[arg-type]
    applied = Collector(registry, engine, config=config).collect(dry_run=False)  # type: ignore[arg-type]

    assert preview.removed == applied.removed == [first.name]


@pytest.mark.parametrize("source", ["file", "environment", "flag"])
def test_storage_ceiling_overrides_change_gc_pressure_decision(
    monkeypatch, tmp_path: Path, registry: Registry, source: str
) -> None:
    resource = add(registry, "cache")
    engine = FakeEngine({"cache": label_dict(registry=registry.registry_id)})
    monkeypatch.setattr(
        gc_mod.StorageInventory,
        "collect",
        classmethod(lambda _cls, _engine: StorageInventory({("volume", "cache"): 10})),
    )
    if source == "file":
        path = tmp_path / "config.toml"
        path.write_text("[policy]\nshared_cache_ceiling = 1\n", encoding="utf-8")
        config = load_config(path=path)
    elif source == "environment":
        monkeypatch.setenv("BOSN_SHARED_CACHE_CEILING", "1")
        config = load_config(path=tmp_path / "missing.toml")
    else:
        config = load_config(path=tmp_path / "missing.toml", flags={"shared_cache_ceiling": 1})

    result = Collector(registry, engine, config=config).collect(dry_run=True)  # type: ignore[arg-type]

    assert result.removed == [resource.name]


def test_gc_advises_compaction_instead_of_eviction_when_vhdx_slack_dominates(
    monkeypatch, registry: Registry
) -> None:
    resource = add(registry, "warm")
    engine = FakeEngine({"warm": label_dict(registry=registry.registry_id)})
    monkeypatch.setattr(
        gc_mod.StorageInventory,
        "collect",
        classmethod(lambda _cls, _engine: StorageInventory({("volume", "warm"): 10})),
    )
    monkeypatch.setattr(
        gc_mod,
        "probe",
        lambda _engine, _path: StorageProbe(free_bytes=100, total_bytes=100, vhdx_slack_bytes=20),
    )
    config = load_config(flags={"shared_cache_ceiling": 1000})

    result = Collector(registry, engine, config=config).collect(dry_run=False)  # type: ignore[arg-type]

    assert result.removed == []
    assert result.advisories
    assert registry.get_resource(resource.id) is not None


def test_there_is_no_force_flag_on_gc() -> None:
    """Automatic deletion requires ownership proof, so no flag may skip it."""
    from bosn.cli import build_parser

    help_text = build_parser().parse_args(["gc"]).__dict__
    assert "force" not in help_text
    with pytest.raises(SystemExit):
        build_parser().parse_args(["gc", "--force"])


# -- network governance -----------------------------------------------------


def test_network_removal_is_ordered_after_container_removal(
    registry: Registry, clock: FakeClock
) -> None:
    """Docker refuses to remove a network with an attached endpoint.

    RED before this slice: `network` was not a governed kind at all -- absent from
    `labels.KINDS`, `resources._LIST_COMMANDS`/`_INSPECT_COMMANDS`, and
    `gc._REMOVE_COMMANDS` -- so a network resource could never be scanned, labeled, or
    removed by GC in the first place. GREEN now: a network is fully governed, and the
    dependency-ordered removal in `gc._REMOVAL_ORDER` places it after `container` (which
    may hold an attached endpoint on it) and before `volume`/`image`.
    """
    add(registry, "proj_net", kind="network")
    add(registry, "proj_ctr", kind="container")
    engine = FakeEngine(
        {
            "proj_net": label_dict(registry=registry.registry_id, kind="network"),
            "proj_ctr": label_dict(registry=registry.registry_id, kind="container"),
        }
    )
    clock.advance(10 * DAY)

    result = Collector(registry, engine).collect(dry_run=False)  # type: ignore[arg-type]

    removal_order = [c[-1] for c in engine.commands if "rm" in c and c[0] != "container"]
    assert removal_order == ["proj_ctr", "proj_net"], (
        "container must be removed before the network that was attached to it"
    )
    assert set(result.removed) == {"proj_net", "proj_ctr"}


def test_network_is_removable_via_gc_without_a_force_flag(
    registry: Registry, clock: FakeClock
) -> None:
    add(registry, "proj_net", kind="network")
    engine = FakeEngine({"proj_net": label_dict(registry=registry.registry_id, kind="network")})
    clock.advance(10 * DAY)

    result = Collector(registry, engine).collect(dry_run=False)  # type: ignore[arg-type]

    assert result.removed == ["proj_net"]
    assert ["network", "rm", "proj_net"] in engine.commands
    assert not any("--force" in cmd for cmd in engine.commands if cmd[0] == "network")
    assert registry.list_resources() == []
    assert any(row["kind"] == "gc.removed" for row in registry.events())


# -- dry run ---------------------------------------------------------------


def test_dry_run_removes_nothing(registry: Registry, clock: FakeClock) -> None:
    add(registry, "ours")
    engine = FakeEngine({"ours": label_dict(registry=registry.registry_id)})
    clock.advance(10 * DAY)

    result = Collector(registry, engine).collect(dry_run=True)  # type: ignore[arg-type]

    assert result.removed == ["ours"], "dry run still reports what would go"
    assert engine.removals() == []
    assert len(registry.list_resources()) == 1


def test_dry_run_plans_idle_stop_without_mutating_engine(
    registry: Registry, clock: FakeClock
) -> None:
    add(registry, "idle", kind="container")
    engine = FakeEngine({"idle": label_dict(registry=registry.registry_id, kind="container")})
    clock.advance(2 * 3600)

    result = Collector(registry, engine).collect(dry_run=True)  # type: ignore[arg-type]

    assert result.would_stop == ["idle"]
    assert result.stopped == []
    assert [cmd for cmd in engine.commands if cmd[:2] == ["container", "stop"]] == []


def test_live_lease_prevents_idle_stop_even_in_apply_mode(
    registry: Registry, clock: FakeClock
) -> None:
    resource = add(registry, "active", kind="container")
    registry.acquire_lease(resource.id, pid=os.getpid(), proc_start=live_proc_start())
    engine = FakeEngine({"active": label_dict(registry=registry.registry_id, kind="container")})
    clock.advance(2 * 3600)

    result = Collector(registry, engine).collect(dry_run=False)  # type: ignore[arg-type]

    assert result.kept == ["active"]
    assert result.stopped == []
    assert [cmd for cmd in engine.commands if cmd[:2] == ["container", "stop"]] == []


def test_apply_stops_an_unleased_idle_container_and_reports_failure(
    registry: Registry, clock: FakeClock
) -> None:
    add(registry, "idle", kind="container")
    engine = FakeEngine({"idle": label_dict(registry=registry.registry_id, kind="container")})
    clock.advance(2 * 3600)

    stopped = Collector(registry, engine).collect(dry_run=False)  # type: ignore[arg-type]
    assert stopped.stopped == ["idle"]
    assert any(row["kind"] == "container.stopped_idle" for row in registry.events())

    engine.fail_on.add("idle")
    failed = Collector(registry, engine).collect(dry_run=False)  # type: ignore[arg-type]
    assert failed.stopped == []
    assert failed.errors == ["idle: stop failed"]
    assert any(row["kind"] == "container.stop_error" for row in registry.events())


@pytest.mark.parametrize("engine_labels", [{}, label_dict(registry="foreign", kind="container")])
def test_idle_stop_requires_complete_current_ownership(
    registry: Registry, clock: FakeClock, engine_labels: dict[str, str]
) -> None:
    add(registry, "not-ours", kind="container")
    engine = FakeEngine({"not-ours": engine_labels})
    clock.advance(2 * 3600)

    result = Collector(registry, engine).collect(dry_run=False)  # type: ignore[arg-type]

    assert result.skipped_unproven == ["not-ours"]
    assert [cmd for cmd in engine.commands if cmd[:2] == ["container", "stop"]] == []


def test_lease_acquired_after_planning_is_seen_before_idle_stop(
    registry: Registry, clock: FakeClock, monkeypatch: pytest.MonkeyPatch
) -> None:
    import bosn.gc as gc_mod

    resource = add(registry, "active", kind="container")
    engine = FakeEngine({"active": label_dict(registry=registry.registry_id, kind="container")})
    clock.advance(2 * 3600)
    original_plan = gc_mod.plan

    def plan_then_lease(*args, **kwargs):
        verdicts = original_plan(*args, **kwargs)
        registry.acquire_lease(resource.id, pid=os.getpid(), proc_start=live_proc_start())
        return verdicts

    monkeypatch.setattr(gc_mod, "plan", plan_then_lease)
    result = Collector(registry, engine).collect(dry_run=False)  # type: ignore[arg-type]

    assert result.stopped == []
    assert [cmd for cmd in engine.commands if cmd[:2] == ["container", "stop"]] == []


def test_dependency_lease_acquired_after_planning_is_seen_before_removal(
    registry: Registry, clock: FakeClock, monkeypatch: pytest.MonkeyPatch
) -> None:
    import bosn.gc as gc_mod

    resource = add(registry, "active-volume")
    engine = FakeEngine({"active-volume": label_dict(registry=registry.registry_id, kind="volume")})
    clock.advance(10 * DAY)
    original_plan = gc_mod.plan

    def plan_then_lease(*args, **kwargs):
        verdicts = original_plan(*args, **kwargs)
        registry.acquire_lease(resource.id, pid=os.getpid(), proc_start=live_proc_start())
        return verdicts

    monkeypatch.setattr(gc_mod, "plan", plan_then_lease)
    result = Collector(registry, engine).collect(dry_run=False)  # type: ignore[arg-type]

    assert result.removed == []
    assert result.kept == ["active-volume"]
    assert engine.removals() == []


def test_dependency_reused_after_done_planning_is_seen_before_removal(
    registry: Registry, monkeypatch: pytest.MonkeyPatch
) -> None:
    import bosn.gc as gc_mod

    resource = add(registry, "reused-volume")
    assert mark_done(registry, "/w") == 1
    engine = FakeEngine({"reused-volume": label_dict(registry=registry.registry_id, kind="volume")})
    original_plan = gc_mod.plan

    def plan_then_reconcile(*args, **kwargs):
        verdicts = original_plan(*args, **kwargs)
        registry.reconcile_resource(
            kind=resource.kind,
            name=resource.name,
            stack=resource.stack,
            generation=resource.generation,
            scope=resource.scope,
            workspace=resource.workspace,
        )
        return verdicts

    monkeypatch.setattr(gc_mod, "plan", plan_then_reconcile)
    result = Collector(registry, engine).collect(dry_run=False)  # type: ignore[arg-type]

    assert result.removed == []
    assert result.kept == ["reused-volume"]
    assert engine.removals() == []


def test_idle_stop_serializes_concurrent_lease_acquisition(
    registry: Registry, clock: FakeClock
) -> None:
    resource = add(registry, "idle", kind="container")
    engine = FakeEngine({"idle": label_dict(registry=registry.registry_id, kind="container")})
    clock.advance(2 * 3600)
    other = Registry(registry.path, clock=clock)
    attempting = threading.Event()

    def acquire() -> None:
        attempting.set()
        other.acquire_lease(resource.id, pid=os.getpid(), proc_start=live_proc_start())
        engine.lease_acquired.set()

    thread = threading.Thread(target=acquire)

    def race_at_ownership_check() -> None:
        thread.start()
        assert attempting.wait(1)

    engine.on_inspect = race_at_ownership_check
    try:
        result = Collector(registry, engine).collect(dry_run=False)  # type: ignore[arg-type]
        thread.join(timeout=2)
    finally:
        other.close()

    assert result.stopped == ["idle"]
    assert not engine.lease_was_active_when_stopped
    assert engine.lease_acquired.is_set()


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


# -- eager superseded image retirement (issue #114) -----------------------


def _supersede(registry: Registry, resource_id: str) -> None:
    resource = registry.get_resource(resource_id)
    assert resource is not None
    registry.record_generation(resource.generation, resource.stack, resource.workspace)
    registry.record_generation("next", resource.stack, resource.workspace)
    registry.supersede_generations(resource.stack, "next", resource.workspace)


def test_gc_eagerly_removes_an_unreferenced_superseded_image(registry: Registry) -> None:
    old = add(registry, "sha256:old", kind="image")
    current = registry.register_resource(
        kind="image",
        name="sha256:current",
        stack="s",
        generation="next",
        scope="spec",
        workspace="/w",
    )
    _supersede(registry, old.id)
    engine = FakeEngine(
        {
            old.name: label_dict(registry=registry.registry_id, kind="image"),
            current.name: label_dict(
                registry=registry.registry_id, kind="image", generation="next"
            ),
        }
    )

    result = Collector(registry, engine).collect(dry_run=False)  # type: ignore[arg-type]

    assert result.removed == [old.name]
    assert registry.get_resource(old.id) is None
    assert registry.get_resource(current.id) is not None
    assert result.image_decisions == [
        {
            "name": current.name,
            "action": "kept",
            "eligible": False,
            "reason": KEPT_CURRENT_IMAGE,
        },
        {
            "name": old.name,
            "action": "removed",
            "eligible": True,
            "reason": COLLECT_SUPERSEDED_IMAGE,
        },
    ]


def test_gc_defers_image_referenced_by_any_container_without_logging_an_error(
    registry: Registry,
) -> None:
    old = add(registry, "sha256:old", kind="image")
    _supersede(registry, old.id)
    engine = FakeEngine(
        {old.name: label_dict(registry=registry.registry_id, kind="image")},
        container_images={"stopped-container": old.name},
    )

    result = Collector(registry, engine).collect(dry_run=False)  # type: ignore[arg-type]

    assert result.removed == []
    assert result.errors == []
    assert result.image_dependency_deferred == [old.name]
    assert result.image_decisions == [
        {
            "name": old.name,
            "action": "deferred",
            "eligible": False,
            "reason": KEPT_IMAGE_REFERENCED,
            "candidate_reason": COLLECT_SUPERSEDED_IMAGE,
            "referenced_by": ["stopped-container"],
        }
    ]
    assert registry.get_resource(old.id) is not None
    assert any(row["kind"] == "gc.image_dependency_deferred" for row in registry.events())


def test_gc_fails_closed_when_container_image_references_are_unknown(
    registry: Registry,
) -> None:
    old = add(registry, "sha256:old", kind="image")
    _supersede(registry, old.id)
    engine = FakeEngine(
        {old.name: label_dict(registry=registry.registry_id, kind="image")},
        container_images=None,
    )

    result = Collector(registry, engine).collect(dry_run=False)  # type: ignore[arg-type]

    assert result.removed == []
    assert result.errors == []
    assert result.image_dependency_deferred == [old.name]
    assert result.image_decisions == [
        {
            "name": old.name,
            "action": "deferred",
            "eligible": False,
            "reason": KEPT_IMAGE_DEPENDENCY_UNKNOWN,
            "candidate_reason": COLLECT_SUPERSEDED_IMAGE,
        }
    ]


def test_gc_removes_collectable_containers_before_rescanning_image_dependencies(
    registry: Registry, clock: FakeClock
) -> None:
    old_image = add(registry, "sha256:old", kind="image")
    old_container = add(registry, "old-container", kind="container")
    current = registry.register_resource(
        kind="image",
        name="sha256:current",
        stack="s",
        generation="next",
        scope="spec",
        workspace="/w",
    )
    _supersede(registry, old_image.id)
    clock.advance(2 * DAY)
    labels_by_name = {
        old_image.name: label_dict(registry=registry.registry_id, kind="image"),
        old_container.name: label_dict(registry=registry.registry_id, kind="container"),
        current.name: label_dict(registry=registry.registry_id, kind="image", generation="next"),
    }
    engine = FakeEngine(labels_by_name, container_images={old_container.name: old_image.name})

    preview = Collector(registry, engine).collect(dry_run=True)  # type: ignore[arg-type]
    applied = Collector(registry, engine).collect(dry_run=False)  # type: ignore[arg-type]

    assert preview.removed == applied.removed == [old_container.name, old_image.name]
    assert preview.image_dependency_deferred == applied.image_dependency_deferred == []


def test_status_distinguishes_current_eager_and_dependency_blocked_images(
    registry: Registry,
) -> None:
    old = add(registry, "sha256:old", kind="image")
    current = registry.register_resource(
        kind="image",
        name="sha256:current",
        stack="s",
        generation="next",
        scope="spec",
        workspace="/w",
    )
    _supersede(registry, old.id)
    labels_by_name = {
        old.name: label_dict(registry=registry.registry_id, kind="image"),
        current.name: label_dict(registry=registry.registry_id, kind="image", generation="next"),
    }

    unblocked = status(registry, FakeEngine(labels_by_name))  # type: ignore[arg-type]
    blocked = status(
        registry,
        FakeEngine(  # type: ignore[arg-type]
            labels_by_name, container_images={"old-container": old.name}
        ),
    )  # type: ignore[arg-type]
    unknown = status(
        registry,
        FakeEngine(labels_by_name, container_images=None),  # type: ignore[arg-type]
    )

    assert unblocked["by_reason"][COLLECT_SUPERSEDED_IMAGE] == 1
    assert unblocked["by_reason"][KEPT_CURRENT_IMAGE] == 1
    assert blocked["by_reason"][KEPT_IMAGE_REFERENCED] == 1
    assert unknown["by_reason"][KEPT_IMAGE_DEPENDENCY_UNKNOWN] == 1


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


# -- run state (issue #90) --------------------------------------------------
#
# GC must probe the engine's run state once per pass -- not once per resource, since a pass
# covers hundreds -- and a container it finds running must survive both the idle-stop path
# and the removal path, under both age and pressure, while a stopped container of identical
# age/scope is collected exactly as before.


def test_gc_protects_a_running_container_under_pressure_but_still_removes_a_stopped_one(
    monkeypatch, registry: Registry
) -> None:
    """RED before the fix: both were pressure-eligible regardless of run state."""
    running = add(registry, "running", kind="container")
    stopped = add(registry, "stopped", kind="container")
    engine = FakeEngine(
        {
            "running": label_dict(registry=registry.registry_id, kind="container"),
            "stopped": label_dict(registry=registry.registry_id, kind="container"),
        },
        running=frozenset({"running"}),
    )
    monkeypatch.setattr(
        gc_mod.StorageInventory,
        "collect",
        classmethod(
            lambda _cls, _engine: StorageInventory(
                {("container", "running"): 10, ("container", "stopped"): 10}
            )
        ),
    )
    config = load_config(flags={"shared_cache_ceiling": 10})

    result = Collector(registry, engine, config=config).collect(dry_run=False)  # type: ignore[arg-type]

    assert result.removed == [stopped.name]
    assert stopped.name in engine.removals()
    assert running.name not in engine.removals()
    assert registry.get_resource(running.id) is not None
    assert running.name in result.kept


def test_gc_never_idle_stops_a_running_container(registry: Registry, clock: FakeClock) -> None:
    add(registry, "busy", kind="container")
    engine = FakeEngine(
        {"busy": label_dict(registry=registry.registry_id, kind="container")},
        running=frozenset({"busy"}),
    )
    clock.advance(2 * 3600)  # past the 1h idle-stop clock

    result = Collector(registry, engine).collect(dry_run=False)  # type: ignore[arg-type]

    assert result.kept == ["busy"]
    assert result.stopped == []
    assert [cmd for cmd in engine.commands if cmd[:2] == ["container", "stop"]] == []


def test_gc_protects_every_container_when_the_engine_cannot_report_run_state(
    registry: Registry, clock: FakeClock
) -> None:
    """`None` (engine unreachable/unparseable) means protect, never means nothing is running."""
    add(registry, "mystery", kind="container")
    engine = FakeEngine(
        {"mystery": label_dict(registry=registry.registry_id, kind="container")}, running=None
    )
    clock.advance(2 * DAY)  # well past both the idle-stop and removal clocks

    result = Collector(registry, engine).collect(dry_run=False)  # type: ignore[arg-type]

    assert result.kept == ["mystery"]
    assert result.removed == []
    assert result.stopped == []


def test_status_reports_running_as_the_kept_reason(registry: Registry) -> None:
    add(registry, "busy", kind="container")
    engine = FakeEngine(
        {"busy": label_dict(registry=registry.registry_id, kind="container")},
        running=frozenset({"busy"}),
    )

    report = status(registry, engine)  # type: ignore[arg-type]

    assert report["decisions"][0]["reason"] == KEPT_RUNNING
    assert report["by_reason"].get(KEPT_RUNNING) == 1
