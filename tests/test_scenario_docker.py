"""Phase 6 exit criteria: the full lifecycle against a real engine.

run -> lease held -> done -> TTL elapse (clock injection, not sleeps) -> gc reclaims
exactly the owned resources while foreign and unlabeled ones are provably untouched.

Docker-marked: Linux CI only.
"""

from __future__ import annotations

import os
import uuid
from collections.abc import Iterator
from pathlib import Path

import pytest

from bosn import labels
from bosn.clock import FakeClock
from bosn.converge import Converger
from bosn.engine import Engine
from bosn.gc import Collector, mark_done, status
from bosn.manifest import load
from bosn.registry import Registry
from bosn.resources import ResourceScanner
from bosn.retention import DAY

pytestmark = pytest.mark.docker

MANIFEST = """
[stack.test]
dockerfile = "Dockerfile"
family = "scenario"
default = true

[stack.test.volumes]
work  = { scope = "spec" }
cache = { scope = "machine" }
"""

FOREIGN_REGISTRY = f"foreign-{uuid.uuid4()}"


@pytest.fixture
def project(tmp_path: Path) -> Path:
    (tmp_path / "Dockerfile").write_text(
        f"FROM alpine:3.20\nRUN echo {uuid.uuid4().hex[:8]} > /m\n", encoding="utf-8"
    )
    (tmp_path / "bosn.toml").write_text(MANIFEST, encoding="utf-8")
    return tmp_path


@pytest.fixture
def clock() -> FakeClock:
    return FakeClock()


@pytest.fixture
def bystanders(engine: Engine) -> Iterator[dict[str, str]]:
    """A foreign-registry volume and a totally unlabeled one, created by 'someone else'."""
    names = {
        "foreign": f"bosn-foreign-{uuid.uuid4().hex[:8]}",
        "unlabeled": f"unrelated-{uuid.uuid4().hex[:8]}",
    }
    foreign_labels = labels.ResourceLabels(
        registry=FOREIGN_REGISTRY,
        kind="volume",
        stack="theirs",
        generation="g",
        scope="spec",
        workspace="/their/w",
        created="2026-08-13T00:00:00Z",
    )
    engine.run(["volume", "create", *foreign_labels.to_docker_args(), names["foreign"]], check=True)
    engine.run(["volume", "create", names["unlabeled"]], check=True)
    try:
        yield names
    finally:
        for name in names.values():
            engine.run(["volume", "rm", "--force", name])


@pytest.fixture
def registry(tmp_path: Path, clock: FakeClock) -> Iterator[Registry]:
    with Registry(tmp_path / "state" / "r.sqlite3", clock=clock) as reg:
        yield reg


def _cleanup(registry: Registry, engine: Engine) -> None:
    resources = registry.list_resources()
    for resource in resources:
        if resource.kind == "container":
            engine.run(["container", "rm", "--force", resource.name])
    for resource in resources:
        if resource.kind == "volume":
            engine.run(["volume", "rm", "--force", resource.name])
        elif resource.kind == "image":
            engine.run(["image", "rm", "--force", resource.name])


def test_full_lifecycle_run_lease_done_ttl_gc(
    project: Path,
    registry: Registry,
    engine: Engine,
    clock: FakeClock,
    bystanders: dict[str, str],
) -> None:
    converger = Converger(load(project), registry, engine)
    try:
        # -- run: converge, build, execute -----------------------------------
        result, code, output = converger.run(["echo", "scenario-ok"])
        assert code == 0, output
        assert "scenario-ok" in output

        spec_volumes = [r for r in registry.list_resources() if r.scope == "spec"]
        machine_volumes = [r for r in registry.list_resources() if r.scope == "machine"]
        assert spec_volumes and machine_volumes

        # -- lease held: nothing is collectable, however old ------------------
        target = next(r for r in spec_volumes if r.kind == "volume")
        # This process is the lease holder, so the liveness probe genuinely succeeds.
        lease = registry.acquire_lease(target.id, pid=os.getpid(), proc_start=1.0, ttl_seconds=900)
        clock.advance(10 * DAY)

        collector = Collector(registry, engine)
        held = collector.collect(dry_run=True)
        assert target.name not in held.removed, "a live lease must protect its resource"

        # -- release the lease, then mark the workspace done ------------------
        registry.release_lease(lease.id)
        assert mark_done(registry, str(load(project).root)) >= 1

        planned = collector.collect(dry_run=True)
        assert target.name in planned.removed
        assert all(m.name not in planned.removed for m in machine_volumes), (
            "done must never collect machine-scoped caches"
        )

        # -- apply: exactly the owned resources go ----------------------------
        applied = collector.collect(dry_run=False)
        assert target.name in applied.removed
        assert applied.errors == []

        # -- the bystanders are provably untouched ----------------------------
        scan = ResourceScanner(engine).scan(registry.registry_id, kinds=["volume"])
        surviving = {r.name for r in scan.foreign} | {r.name for r in scan.unlabeled}
        assert bystanders["foreign"] in surviving
        assert bystanders["unlabeled"] in surviving
        assert FOREIGN_REGISTRY in scan.foreign_registries, "foreign registries stay reported"

        # a removed resource is deregistered, not merely marked
        assert target.name not in {r.name for r in registry.list_resources()}
    finally:
        _cleanup(registry, engine)


def test_status_sees_the_engine_and_reports_foreign_registries(
    project: Path, registry: Registry, engine: Engine, bystanders: dict[str, str]
) -> None:
    converger = Converger(load(project), registry, engine)
    try:
        converger.run(["true"])
        report = status(registry, engine)

        assert report["registered"] > 0
        assert report["engine"]["foreign"] >= 1
        assert FOREIGN_REGISTRY in report["foreign_registries"]
    finally:
        _cleanup(registry, engine)


def test_a_lost_registry_rebuilds_from_labels(
    project: Path, registry: Registry, engine: Engine, tmp_path: Path, clock: FakeClock
) -> None:
    """Losing the database is survivable: ownership lives in the labels."""
    from bosn.resources import adopt

    converger = Converger(load(project), registry, engine)
    try:
        converger.run(["true"])
        original = {
            (resource.kind, resource.name)
            for resource in registry.list_resources()
            if resource.kind in {"container", "volume", "image"}
        }
        registry_id = registry.registry_id

        # wipe every row, keeping the registry id -- the database is "lost"
        for resource in registry.list_resources():
            registry.remove_resource(resource.id)
        assert registry.list_resources() == []

        scan = ResourceScanner(engine).scan(registry_id, kinds=["container", "volume", "image"])
        adopted = adopt(registry, scan, clock=clock)

        assert {name for _kind, name in original} <= set(adopted)
        assert {resource.kind for resource in registry.list_resources()} == {
            "container",
            "image",
            "volume",
        }
        _result, code, output = converger.run(["echo", "adopted-ok"])
        assert code == 0, output
    finally:
        _cleanup(registry, engine)
