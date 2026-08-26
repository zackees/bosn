"""Phase 5: converge semantics and volume-scope naming. No Docker needed."""

from __future__ import annotations

import hashlib
import json
import os
import sys
from dataclasses import replace
from pathlib import Path

import pytest

from bosn import labels
from bosn.converge import (
    REGISTERED,
    REUSED,
    ROLLED,
    Converger,
    generation_coalescing_key,
    resolved_generation,
    volume_name_for,
    workspace_of,
)
from bosn.engine import EngineError, EngineResult
from bosn.manifest import load
from bosn.registry import Registry
from conftest import live_proc_start

SAMPLE = """
[stack.test]
dockerfile = "Dockerfile"
family = "rust"
default = true

[stack.test.volumes]
target    = { scope = "spec" }
chef      = { scope = "stack" }
cargo-reg = { scope = "machine" }
"""


class FakeEngine:
    """Everything succeeds; inspect misses until a create happens."""

    def __init__(self) -> None:
        self.commands: list[list[str]] = []
        self.timeouts: list[float | None] = []
        self.existing: set[str] = set()
        self.image_ids: dict[str, str] = {"alpine": "sha256:alpine-v1"}
        self.image_platforms: dict[str, str] = {"alpine": "linux/amd64"}
        self.container_specs: dict[str, dict[str, object]] = {}
        self.resource_labels: dict[tuple[str, str], dict[str, str]] = {}
        self.container_serial = 0

    def run(
        self, args: list[str], *, check: bool = False, timeout: float | None = None
    ) -> EngineResult:
        self.commands.append(list(args))
        self.timeouts.append(timeout)
        if args[:2] == ["version", "--format"]:
            return EngineResult(0, "linux/amd64", "")
        if args[:2] == ["container", "inspect"] and "{{json .}}" in args:
            spec = self.container_specs.get(args[-1])
            return EngineResult(0 if spec else 1, json.dumps(spec) if spec else "", "")
        if args[:2] == ["volume", "inspect"] and "{{json .Labels}}" in args:
            raw = self.resource_labels.get(("volume", args[-1]))
            return EngineResult(0 if raw is not None else 1, json.dumps(raw or {}), "")
        if args[:2] == ["image", "inspect"] and "{{json .Config.Labels}}" in args:
            raw = self.resource_labels.get(("image", args[-1]))
            return EngineResult(0 if raw is not None else 1, json.dumps(raw or {}), "")
        if args[:2] == ["image", "inspect"] and "{{.Id}}" in args:
            requested_platform = (
                args[args.index("--platform") + 1] if "--platform" in args else None
            )
            identity = self.image_ids.get(args[-1])
            if identity is None and args[-1] in self.image_ids.values():
                identity = args[-1]
            platform_matches = requested_platform is None or (
                self.image_platforms.get(args[-1]) == requested_platform
            )
            return EngineResult(0 if identity and platform_matches else 1, identity or "", "")
        if "inspect" in args:
            return EngineResult(0 if args[-1] in self.existing else 1, "", "")
        if args[0] == "pull":
            self.image_ids.setdefault(
                args[-1], f"sha256:{hashlib.sha256(args[-1].encode()).hexdigest()}"
            )
            if "--platform" in args:
                self.image_platforms[args[-1]] = args[args.index("--platform") + 1]
            else:
                self.image_platforms.setdefault(args[-1], "linux/amd64")
        if args[:2] == ["volume", "create"]:
            self.existing.add(args[-1])
            self.resource_labels[("volume", args[-1])] = self._labels_from_args(args)
        if args[:3] == ["container", "rm", "--force"]:
            target = args[-1]
            name = next(
                (
                    candidate
                    for candidate, spec in self.container_specs.items()
                    if candidate == target or spec.get("Id") == target
                ),
                target,
            )
            self.existing.discard(name)
            self.container_specs.pop(name, None)
        if args[0] == "create":
            name = args[args.index("--name") + 1]
            self.existing.add(name)
            self.container_serial += 1
            identity = f"{name}\0{self.container_serial}"
            container_id = f"sha256:{hashlib.sha256(identity.encode()).hexdigest()}"
            raw_labels = {
                args[index + 1].split("=", 1)[0]: args[index + 1].split("=", 1)[1]
                for index, value in enumerate(args[:-1])
                if value == "--label"
            }
            mounts: list[dict[str, object]] = []
            tmpfs: dict[str, str] = {}
            for index, value in enumerate(args[:-1]):
                if value != "--volume":
                    continue
                rendered = args[index + 1]
                read_only = rendered.endswith(":ro")
                core = rendered[:-3] if read_only else rendered
                source, destination = core.rsplit(":", 1)
                # A named volume's "source" is a bare token; a bind source is always a
                # path. That's the only signal available here to tell them apart.
                is_path = "/" in source or "\\" in source
                mount_type = (
                    "bind" if destination == "/bosn-daemon/heartbeat" or is_path else "volume"
                )
                mounts.append(
                    {
                        "Type": mount_type,
                        "Name": source if mount_type == "volume" else "",
                        "Source": source,
                        "Destination": destination,
                        "RW": not read_only,
                    }
                )
            for index, value in enumerate(args[:-1]):
                if value != "--tmpfs":
                    continue
                rendered = args[index + 1]
                destination, _, options = rendered.partition(":")
                # Docker does not report `--tmpfs` entries in `.Mounts`; it round-trips
                # them through `.HostConfig.Tmpfs` as destination -> option string (#116).
                tmpfs[destination] = options
            image = args[-4]
            self.container_specs[name] = {
                "Id": container_id,
                "Config": {"Labels": raw_labels, "Image": image},
                "Image": self.image_ids.get(image, image),
                "Mounts": mounts,
                "HostConfig": {"Tmpfs": tmpfs},
            }
            return EngineResult(0, container_id, "")
        if args[0] == "build":
            tag = args[args.index("--tag") + 1]
            self.existing.add(tag)
            self.image_ids[tag] = f"sha256:{hashlib.sha256(tag.encode()).hexdigest()}"
            self.image_platforms[tag] = "linux/amd64"
            raw = self._labels_from_args(args)
            self.resource_labels[("image", tag)] = raw
            self.resource_labels[("image", self.image_ids[tag])] = raw
        return EngineResult(0, "ok", "")

    @staticmethod
    def _labels_from_args(args: list[str]) -> dict[str, str]:
        return {
            args[index + 1].split("=", 1)[0]: args[index + 1].split("=", 1)[1]
            for index, value in enumerate(args[:-1])
            if value == "--label"
        }

    def stream(
        self,
        args: list[str],
        *,
        on_line=None,
        cancelled=None,
    ) -> EngineResult:
        """Builds go through stream(); record them the same way so `ran` still sees them."""
        if on_line is not None:
            on_line(f"fake build: {' '.join(args[:2])}")
        return self.run(args)

    def interactive(self, args: list[str]) -> int:
        self.commands.append(list(args))
        return 7

    def ran(self, *prefix: str) -> list[list[str]]:
        return [c for c in self.commands if c[: len(prefix)] == list(prefix)]


@pytest.fixture
def project(tmp_path: Path) -> Path:
    (tmp_path / "Dockerfile").write_text("FROM alpine\n", encoding="utf-8")
    (tmp_path / "bosn.toml").write_text(SAMPLE, encoding="utf-8")
    return tmp_path


BIND_SAMPLE = """
[stack.test]
dockerfile = "Dockerfile"
family = "rust"
default = true

[stack.test.mounts]
repo = { source = "repo", destination = "/repo" }
"""


@pytest.fixture
def bind_project(tmp_path: Path) -> Path:
    """A stack with one declared bind mount and no volumes, so mount matching is isolated."""
    (tmp_path / "Dockerfile").write_text("FROM alpine\n", encoding="utf-8")
    (tmp_path / "repo").mkdir()
    (tmp_path / "repo" / "marker").write_text("hi", encoding="utf-8")
    (tmp_path / "bosn.toml").write_text(BIND_SAMPLE, encoding="utf-8")
    return tmp_path


@pytest.fixture
def registry(tmp_path: Path):
    with Registry(tmp_path / "r.sqlite3") as reg:
        yield reg


@pytest.fixture
def converger(project: Path, registry: Registry) -> Converger:
    return Converger(load(project), registry, FakeEngine())  # type: ignore[arg-type]


# -- convergence is idempotent ---------------------------------------------


def test_first_converge_registers_then_reuses(converger: Converger) -> None:
    """The same command is correct on the 1st and the 500th invocation."""
    first = converger.converge()
    assert first.action == REGISTERED

    second = converger.converge()
    assert second.action == REUSED
    assert second.digest == first.digest

    for _ in range(10):
        assert converger.converge().action == REUSED


def test_converge_creates_each_volume_once(converger: Converger) -> None:
    converger.converge()
    engine: FakeEngine = converger.engine  # type: ignore[assignment]
    assert len(engine.ran("volume", "create")) == 3

    converger.converge()
    assert len(engine.ran("volume", "create")) == 3, "volumes must not be recreated"


def test_converge_builds_the_image_once(converger: Converger) -> None:
    converger.converge()
    engine: FakeEngine = converger.engine  # type: ignore[assignment]
    assert len(engine.ran("build")) == 1
    converger.converge()
    assert len(engine.ran("build")) == 1


def test_editing_the_spec_rolls_the_generation(project: Path, registry: Registry) -> None:
    engine = FakeEngine()
    Converger(load(project), registry, engine).converge()  # type: ignore[arg-type]

    (project / "Dockerfile").write_text("FROM alpine:3.20\n", encoding="utf-8")
    rolled = Converger(load(project), registry, engine).converge()  # type: ignore[arg-type]

    assert rolled.action == ROLLED
    assert rolled.superseded == 1


def test_editing_a_copy_input_rolls_and_rebuilds(project: Path, registry: Registry) -> None:
    (project / "Dockerfile").write_text("FROM alpine\nCOPY payload /payload\n", encoding="utf-8")
    payload = project / "payload"
    payload.write_text("one", encoding="utf-8")
    engine = FakeEngine()
    first = Converger(load(project), registry, engine).converge()  # type: ignore[arg-type]

    payload.write_text("two", encoding="utf-8")
    second = Converger(load(project), registry, engine).converge()  # type: ignore[arg-type]

    assert second.digest != first.digest
    assert second.action == ROLLED
    assert len(engine.ran("build")) == 2


def test_changing_the_resolved_base_image_rolls_the_generation(
    project: Path, registry: Registry
) -> None:
    engine = FakeEngine()
    first = Converger(load(project), registry, engine).converge()  # type: ignore[arg-type]
    engine.image_ids["alpine"] = "sha256:alpine-v2"

    second = Converger(load(project), registry, engine).converge()  # type: ignore[arg-type]

    assert second.digest != first.digest
    assert second.action == ROLLED


def test_explicit_from_platform_is_used_to_resolve_the_image(
    project: Path, registry: Registry
) -> None:
    (project / "Dockerfile").write_text("FROM --platform=linux/arm64 alpine\n", encoding="utf-8")
    engine = FakeEngine()

    Converger(load(project), registry, engine).converge()  # type: ignore[arg-type]

    assert engine.ran(
        "image", "inspect", "--platform", "linux/arm64", "--format", "{{.Id}}", "alpine"
    )
    assert engine.ran("pull", "--platform", "linux/arm64", "alpine")


def test_matching_explicit_platform_does_not_pull_before_or_during_a_warm_converge(
    project: Path, registry: Registry
) -> None:
    (project / "Dockerfile").write_text("FROM --platform=linux/amd64 alpine\n", encoding="utf-8")
    engine = FakeEngine()
    manifest = load(project)

    generation_coalescing_key(manifest, manifest.stack(None), engine)  # type: ignore[arg-type]
    Converger(manifest, registry, engine).converge()  # type: ignore[arg-type]

    assert engine.ran("pull") == []


def test_coalescing_probe_never_pulls_a_missing_image(project: Path) -> None:
    engine = FakeEngine()
    engine.image_ids.clear()
    engine.image_platforms.clear()
    manifest = load(project)

    key = generation_coalescing_key(manifest, manifest.stack(None), engine)  # type: ignore[arg-type]

    assert key.startswith("sha256:")
    assert engine.ran("pull") == []


def test_automatic_from_platform_is_resolved_from_the_engine(project: Path) -> None:
    (project / "Dockerfile").write_text("FROM --platform=$BUILDPLATFORM alpine\n", encoding="utf-8")
    manifest = load(project)
    engine = FakeEngine()

    resolved_generation(manifest, manifest.stack(None), engine)  # type: ignore[arg-type]

    assert engine.ran("version", "--format")
    assert engine.ran("pull") == []


def test_stack_without_external_images_keeps_its_content_digest(
    project: Path, registry: Registry
) -> None:
    (project / "Dockerfile").write_text("FROM scratch\n", encoding="utf-8")
    manifest = load(project)

    result = Converger(manifest, registry, FakeEngine()).converge()  # type: ignore[arg-type]

    assert result.digest == manifest.digest()


def test_image_backed_stack_runs_the_resolved_declared_image(
    tmp_path: Path, registry: Registry
) -> None:
    (tmp_path / "bosn.toml").write_text(
        '[stack.app]\nimage = "alpine:3.20"\ndefault = true\n', encoding="utf-8"
    )
    engine = FakeEngine()
    engine.image_ids["alpine:3.20"] = "sha256:declared-image"
    converger = Converger(load(tmp_path), registry, engine)  # type: ignore[arg-type]

    result, code, _output = converger.run(["true"])

    assert code == 0
    assert result.image_tag == "sha256:declared-image"
    assert engine.ran("create")[0][-4] == "sha256:declared-image"


def test_the_old_generation_is_marked_superseded(project: Path, registry: Registry) -> None:
    engine = FakeEngine()
    manifest = load(project)
    workspace = workspace_of(manifest)
    first = Converger(manifest, registry, engine).converge()  # type: ignore[arg-type]
    (project / "Dockerfile").write_text("FROM debian\n", encoding="utf-8")
    second = Converger(load(project), registry, engine).converge()  # type: ignore[arg-type]

    assert (
        registry.generation_superseded_at(first.digest, stack="test", workspace=workspace)
        is not None
    )
    assert (
        registry.generation_superseded_at(second.digest, stack="test", workspace=workspace) is None
    )


def test_shared_volumes_rebind_to_the_current_generation_and_refresh_liveness(
    project: Path, tmp_path: Path
) -> None:
    from bosn.clock import FakeClock

    clock = FakeClock()
    engine = FakeEngine()
    with Registry(tmp_path / "liveness.sqlite3", clock=clock) as registry:
        first_converger = Converger(load(project), registry, engine)  # type: ignore[arg-type]
        first = first_converger.converge()
        shared = {
            resource.scope: resource
            for resource in registry.list_resources()
            if resource.kind == "volume" and resource.scope in {"stack", "machine"}
        }
        clock.advance(3600)

        (project / "Dockerfile").write_text("FROM alpine\nRUN echo changed\n", encoding="utf-8")
        second = Converger(load(project), registry, engine).converge()  # type: ignore[arg-type]

        assert second.digest != first.digest
        for scope, previous in shared.items():
            current = registry.get_resource_by_engine_identity("volume", previous.name)
            assert current is not None
            assert current.id == previous.id
            assert current.scope == scope
            assert current.generation == second.digest
            assert current.last_used == clock.now()
            assert (
                registry.generation_superseded_at(
                    current.generation,
                    stack=current.stack,
                    workspace=current.workspace,
                )
                is None
            )


def test_warm_converge_refreshes_each_reused_owned_dependency(
    project: Path, tmp_path: Path
) -> None:
    from bosn.clock import FakeClock

    clock = FakeClock()
    engine = FakeEngine()
    with Registry(tmp_path / "warm.sqlite3", clock=clock) as registry:
        converger = Converger(load(project), registry, engine)  # type: ignore[arg-type]
        first = converger.converge()
        image_id = engine.image_ids[first.image_tag]
        identities = [("image", image_id), *(("volume", name) for name in first.volumes)]
        before = {
            identity: registry.get_resource_by_engine_identity(*identity) for identity in identities
        }
        assert all(resource is not None for resource in before.values())
        clock.advance(3600)

        second = converger.converge()

        assert second.action == REUSED
        for identity, previous in before.items():
            current = registry.get_resource_by_engine_identity(*identity)
            assert previous is not None and current is not None
            assert current.id == previous.id
            assert current.last_used == clock.now()


def test_adopted_image_id_is_refreshed_and_leased_by_a_warm_run(
    project: Path, tmp_path: Path
) -> None:
    from bosn.clock import FakeClock
    from bosn.retention import KEPT_LEASED, Pressure, evaluate

    clock = FakeClock()
    engine = FakeEngine()
    with Registry(tmp_path / "adopted-image.sqlite3", clock=clock) as registry:
        converger = Converger(load(project), registry, engine)  # type: ignore[arg-type]
        first = converger.converge()
        image_id = engine.image_ids[first.image_tag]
        original = registry.get_resource_by_engine_identity("image", image_id)
        assert original is not None
        registry.remove_resource(original.id)
        adopted = registry.register_resource(
            kind="image",
            name=image_id,
            stack=first.stack,
            generation=first.digest,
            scope="spec",
            workspace=workspace_of(converger.manifest),
        )
        registry.set_resource_state(adopted.id, "adopted")
        clock.advance(3600)

        warm = converger.converge()
        refreshed = registry.get_resource(adopted.id)
        assert refreshed is not None
        assert refreshed.last_used == clock.now()
        assert refreshed.state == "active"
        assert registry.get_resource_by_engine_identity("image", first.image_tag) is None
        _container_id, leases = converger._acquire_execution_container(
            warm,
            stack_name=None,
            workspace=workspace_of(converger.manifest),
        )
        try:
            assert adopted.id in {lease.resource_id for lease in leases}
            clock.advance(4 * 24 * 3600)
            verdict = evaluate(
                registry,
                refreshed,
                superseded=True,
                workspace_done=True,
                pressure=Pressure(under_pressure=True),
                alive_probe=lambda _pid, _start: True,
            )
            assert not verdict.collect
            assert verdict.reason == KEPT_LEASED
        finally:
            for lease in leases:
                registry.release_lease(lease.id)


@pytest.mark.parametrize("kind", ["image", "volume"])
def test_owned_reused_resource_without_a_registry_row_is_restored(
    project: Path, tmp_path: Path, kind: str
) -> None:
    engine = FakeEngine()
    with Registry(tmp_path / "restore-owned.sqlite3") as registry:
        converger = Converger(load(project), registry, engine)  # type: ignore[arg-type]
        first = converger.converge()
        name = engine.image_ids[first.image_tag] if kind == "image" else first.volumes[0]
        original = registry.get_resource_by_engine_identity(kind, name)
        assert original is not None
        registry.remove_resource(original.id)

        converger.converge()

        restored = registry.get_resource_by_engine_identity(kind, name)
        assert restored is not None
        assert restored.id != original.id


@pytest.mark.parametrize("collision", ["foreign", "incomplete"])
def test_reused_volume_collision_is_refused_before_registry_reconciliation(
    project: Path, tmp_path: Path, collision: str
) -> None:
    engine = FakeEngine()
    with Registry(tmp_path / "foreign-volume.sqlite3") as registry:
        converger = Converger(load(project), registry, engine)  # type: ignore[arg-type]
        first = converger.converge()
        name = first.volumes[0]
        original = registry.get_resource_by_engine_identity("volume", name)
        assert original is not None
        registry.remove_resource(original.id)
        if collision == "foreign":
            engine.resource_labels[("volume", name)] = labels.ResourceLabels(
                registry="other-registry",
                kind="volume",
                stack="test",
                generation=first.digest,
                scope="spec",
                workspace="/other",
                created="2026-01-01T00:00:00Z",
            ).to_dict()
        else:
            engine.resource_labels[("volume", name)] = {labels.REGISTRY: registry.registry_id}

        with pytest.raises(EngineError, match="refusing to reuse"):
            converger.converge()

        assert registry.get_resource_by_engine_identity("volume", name) is None


def test_every_created_resource_is_registered(converger: Converger, registry: Registry) -> None:
    converger.converge()
    kinds = sorted(r.kind for r in registry.list_resources())
    assert kinds == ["image", "volume", "volume", "volume"]


def test_created_resources_carry_the_full_label_contract(converger: Converger) -> None:
    converger.converge()
    engine: FakeEngine = converger.engine  # type: ignore[assignment]
    from bosn import labels

    create = engine.ran("volume", "create")[0]
    rendered = {
        arg.split("=", 1)[0] for i, arg in enumerate(create) if i > 0 and create[i - 1] == "--label"
    }
    assert set(labels.REQUIRED_LABELS) <= rendered


# -- volume scope naming ---------------------------------------------------


def _name(scope: str, *, digest: str, workspace: str, family: str | None = "rust") -> str:
    from bosn.manifest import StackSpec

    stack = StackSpec(name="test", dockerfile="Dockerfile", family=family)
    return volume_name_for(stack, scope, "cache", digest=digest, workspace=workspace, family=family)


def test_spec_scoped_volumes_change_with_the_digest() -> None:
    a = _name("spec", digest="sha256:aaa", workspace="/w1")
    b = _name("spec", digest="sha256:bbb", workspace="/w1")
    assert a != b


def test_spec_scoped_volumes_differ_across_workspaces() -> None:
    a = _name("spec", digest="sha256:aaa", workspace="/w1")
    b = _name("spec", digest="sha256:aaa", workspace="/w2")
    assert a != b


def test_stack_scoped_volumes_survive_spec_edits_in_one_workspace() -> None:
    a = _name("stack", digest="sha256:aaa", workspace="/w1")
    b = _name("stack", digest="sha256:bbb", workspace="/w1")
    assert a == b


def test_stack_scoped_volumes_differ_across_workspaces() -> None:
    a = _name("stack", digest="sha256:aaa", workspace="/w1")
    b = _name("stack", digest="sha256:aaa", workspace="/w2")
    assert a != b


def test_machine_scoped_volumes_are_one_per_machine() -> None:
    """This is what kills the incident's dominant multiplier."""
    a = _name("machine", digest="sha256:aaa", workspace="/w1")
    b = _name("machine", digest="sha256:bbb", workspace="/w2")
    assert a == b


def test_machine_scope_is_shared_across_a_family() -> None:
    from bosn.manifest import StackSpec

    one = volume_name_for(
        StackSpec(name="alpha", image="x", family="rust"),
        "machine",
        "cargo-reg",
        digest="sha256:a",
        workspace="/w1",
        family="rust",
    )
    two = volume_name_for(
        StackSpec(name="beta", image="y", family="rust"),
        "machine",
        "cargo-reg",
        digest="sha256:b",
        workspace="/w2",
        family="rust",
    )
    assert one == two, "same family must share one machine-scoped volume"


# -- workspace identity ----------------------------------------------------
#
# The job table keys on `(workspace-id, stack)`, so this string decides what serializes and
# what runs in parallel. #1 blames per-worktree path-hashing for the original volume
# explosion, which is why it is the manifest root and never the cwd.


def test_workspace_id_is_the_manifest_root_not_the_cwd(project: Path, monkeypatch) -> None:
    """Two agents in different subdirectories of one worktree are one workspace."""
    from bosn.converge import workspace_of
    from bosn.manifest import find_manifest, load

    nested = project / "src" / "deep"
    nested.mkdir(parents=True)

    monkeypatch.chdir(project)
    from_root = workspace_of(load(find_manifest()))  # type: ignore[arg-type]
    monkeypatch.chdir(nested)
    from_nested = workspace_of(load(find_manifest()))  # type: ignore[arg-type]

    assert from_root == from_nested, "same worktree, different cwd -> one key, so they serialize"


def test_distinct_worktrees_get_distinct_workspace_ids(tmp_path: Path) -> None:
    """...and two worktrees must differ, or a slow build in A would block B."""
    from bosn.converge import workspace_of
    from bosn.manifest import load

    ids = []
    for name in ("wt-a", "wt-b"):
        root = tmp_path / name
        root.mkdir()
        (root / "Dockerfile").write_text("FROM alpine\n", encoding="utf-8")
        (root / "bosn.toml").write_text(SAMPLE, encoding="utf-8")
        ids.append(workspace_of(load(root)))

    assert ids[0] != ids[1]


def test_one_workspaces_roll_does_not_supersede_another_workspaces_generation(
    tmp_path: Path,
) -> None:
    roots = [tmp_path / "wt-a", tmp_path / "wt-b"]
    for root in roots:
        root.mkdir()
        (root / "Dockerfile").write_text("FROM alpine\n", encoding="utf-8")
        (root / "bosn.toml").write_text(SAMPLE, encoding="utf-8")
    engine = FakeEngine()
    with Registry(tmp_path / "workspace-generations.sqlite3") as registry:
        manifests = [load(root) for root in roots]
        shared = [
            Converger(manifest, registry, engine).converge()  # type: ignore[arg-type]
            for manifest in manifests
        ]
        assert shared[0].digest == shared[1].digest

        (roots[0] / "Dockerfile").write_text("FROM alpine\nRUN echo changed\n", encoding="utf-8")
        Converger(load(roots[0]), registry, engine).converge()  # type: ignore[arg-type]

        assert (
            registry.generation_superseded_at(
                shared[0].digest,
                stack=shared[0].stack,
                workspace=workspace_of(manifests[0]),
            )
            is not None
        )
        assert (
            registry.generation_superseded_at(
                shared[1].digest,
                stack=shared[1].stack,
                workspace=workspace_of(manifests[1]),
            )
            is None
        )


def test_shared_old_resources_stay_current_until_every_workspace_rolls(tmp_path: Path) -> None:
    from bosn.clock import FakeClock
    from bosn.retention import plan

    roots = [tmp_path / "consumer-a", tmp_path / "consumer-b"]
    for root in roots:
        root.mkdir()
        (root / "Dockerfile").write_text("FROM alpine\n", encoding="utf-8")
        (root / "bosn.toml").write_text(SAMPLE, encoding="utf-8")
    clock = FakeClock()
    engine = FakeEngine()
    with Registry(tmp_path / "shared-consumers.sqlite3", clock=clock) as registry:
        manifests = [load(root) for root in roots]
        original = [
            Converger(manifest, registry, engine).converge()  # type: ignore[arg-type]
            for manifest in manifests
        ]
        old_names = {
            engine.image_ids[original[0].image_tag],
            original[0].volumes[0],
        }

        (roots[1] / "Dockerfile").write_text("FROM alpine\nRUN echo b-rolled\n", encoding="utf-8")
        Converger(load(roots[1]), registry, engine).converge()  # type: ignore[arg-type]
        clock.advance(2 * 24 * 3600)

        verdicts = {verdict.name: verdict for verdict in plan(registry)}
        assert all(not verdicts[name].collect for name in old_names)

        (roots[0] / "Dockerfile").write_text("FROM alpine\nRUN echo a-rolled\n", encoding="utf-8")
        Converger(load(roots[0]), registry, engine).converge()  # type: ignore[arg-type]

        verdicts = {verdict.name: verdict for verdict in plan(registry)}
        assert all(verdicts[name].collect for name in old_names)


def test_a_non_canonical_path_to_the_same_root_is_the_same_workspace(project: Path) -> None:
    """`..` and `.` must not be able to split one worktree into two."""
    from bosn.converge import workspace_of
    from bosn.manifest import load

    scenic = project / "." / "src" / ".." / "bosn.toml"
    (project / "src").mkdir(exist_ok=True)

    assert workspace_of(load(project)) == workspace_of(load(scenic))


@pytest.mark.skipif(not sys.platform.startswith("win"), reason="case-insensitive paths")
def test_windows_path_case_does_not_split_a_workspace(project: Path) -> None:
    from bosn.converge import workspace_of
    from bosn.manifest import load

    shouted = Path(str(project).upper())
    assert workspace_of(load(project)) == workspace_of(load(shouted))


def test_converge_registers_resources_under_the_canonical_workspace(
    converger: Converger, registry: Registry
) -> None:
    """`bosn done` looks resources up by this id, so converge must write the same one."""
    from bosn.converge import workspace_of

    converger.converge()
    expected = workspace_of(converger.manifest)
    assert {r.workspace for r in registry.list_resources()} == {expected}


# -- a cancelled build leaves nothing behind -------------------------------


def test_a_failed_build_registers_no_image_and_no_generation(
    project: Path, registry: Registry
) -> None:
    """Registration happens only after `docker build` exits 0."""
    from bosn.engine import EngineError

    class FailingEngine(FakeEngine):
        def stream(self, args, *, on_line=None, cancelled=None) -> EngineResult:
            self.commands.append(list(args))
            return EngineResult(1, "", "build blew up")

    with pytest.raises(EngineError):
        Converger(load(project), registry, FailingEngine()).converge()  # type: ignore[arg-type]

    assert registry.list_resources() == []
    assert registry.conn.execute("SELECT 1 FROM generations").fetchall() == []


def test_a_failed_build_does_not_supersede_the_working_generation(
    project: Path, registry: Registry
) -> None:
    """Retention puts superseded generations on a 24h clock -- so this would collect the
    only image that actually works, in favor of one that never got built."""
    from bosn.engine import EngineError

    good = Converger(load(project), registry, FakeEngine()).converge()  # type: ignore[arg-type]

    class FailingEngine(FakeEngine):
        def stream(self, args, *, on_line=None, cancelled=None) -> EngineResult:
            self.commands.append(list(args))
            return EngineResult(1, "", "nope")

    (project / "Dockerfile").write_text("FROM debian\n", encoding="utf-8")
    with pytest.raises(EngineError):
        Converger(load(project), registry, FailingEngine()).converge()  # type: ignore[arg-type]

    assert (
        registry.generation_superseded_at(
            good.digest,
            stack=good.stack,
            workspace=workspace_of(load(project)),
        )
        is None
    )


def test_a_cancelled_build_reports_cancellation_not_a_generic_failure(
    project: Path, registry: Registry
) -> None:
    import threading

    from bosn.engine import EngineError

    cancelled = threading.Event()
    cancelled.set()

    class CancelledEngine(FakeEngine):
        def stream(self, args, *, on_line=None, cancelled=None) -> EngineResult:
            self.commands.append(list(args))
            return EngineResult(-1, "", "cancelled")

    converger = Converger(
        load(project),
        registry,
        CancelledEngine(),  # type: ignore[arg-type]
        cancelled=cancelled,
    )
    with pytest.raises(EngineError, match="was cancelled"):
        converger.converge()


# -- running ---------------------------------------------------------------


def test_run_converges_first_then_runs(converger: Converger) -> None:
    _result, code, _output = converger.run(["echo", "hi"])
    engine: FakeEngine = converger.engine  # type: ignore[assignment]
    assert code == 0
    assert engine.ran("build"), "converge must have happened before the run"
    exec_cmd = engine.ran("exec")[0]
    assert exec_cmd[-2:] == ["echo", "hi"]
    assert engine.ran("create"), "the first run creates the persistent container"


def test_run_mounts_every_declared_volume(converger: Converger) -> None:
    converger.run(["true"])
    engine: FakeEngine = converger.engine  # type: ignore[assignment]
    mounts = [a for a in engine.ran("create")[0] if a.startswith("bosn-") and ":/bosn/" in a]
    assert len(mounts) == 3


def test_run_mounts_declared_tmpfs(tmp_path: Path) -> None:
    """Issue #116: Docker's HostConfig.Tmpfs round trip must validate after create."""
    (tmp_path / "bosn.toml").write_text(
        '[stack.mysql]\nimage = "alpine"\ntmpfs = ["/var/lib/mysql"]\n',
        encoding="utf-8",
    )
    engine = FakeEngine()
    with Registry(tmp_path / "tmpfs.sqlite3") as registry:
        Converger(load(tmp_path), registry, engine).run(["true"])  # type: ignore[arg-type]

    create = engine.ran("create")[0]
    assert create[create.index("--tmpfs") + 1] == "/var/lib/mysql"


def test_read_only_tmpfs_reuses_the_persistent_container(tmp_path: Path) -> None:
    (tmp_path / "bosn.toml").write_text(
        '[stack.app]\nimage = "alpine"\ntmpfs = ["/run/scratch:ro,size=16m"]\n',
        encoding="utf-8",
    )
    engine = FakeEngine()
    with Registry(tmp_path / "readonly-tmpfs.sqlite3") as registry:
        converger = Converger(load(tmp_path), registry, engine)  # type: ignore[arg-type]
        converger.run(["true"])
        converger.run(["true"])

    assert len(engine.ran("create")) == 1
    assert engine.ran("create")[0][engine.ran("create")[0].index("--tmpfs") + 1] == (
        "/run/scratch:ro,size=16m"
    )


def test_conflicting_tmpfs_and_mount_inspect_data_fails_closed() -> None:
    expected = {
        "/scratch": {
            "type": "tmpfs",
            "source": "",
            "destination": "/scratch",
            "rw": True,
        }
    }
    contradictory_mounts = [
        {
            "Type": "bind",
            "Source": "/host/scratch",
            "Destination": "/scratch",
            "RW": True,
        }
    ]

    assert (
        Converger._container_mounts_match(contradictory_mounts, expected, {"/scratch": ""}) is False
    )
    matching_mounts = [{"Type": "tmpfs", "Destination": "/scratch", "RW": True}]
    assert Converger._container_mounts_match(matching_mounts, expected, {"/scratch": 123}) is False
    assert Converger._container_mounts_match(matching_mounts, expected, []) is False


def test_second_run_reuses_the_same_persistent_container(converger: Converger) -> None:
    converged, _code, _output = converger.run(["true"])
    engine: FakeEngine = converger.engine  # type: ignore[assignment]
    name = converger.container_name(workspace_of(converger.manifest), converged.stack)
    container_id = str(engine.container_specs[name]["Id"])

    converger.run(["true"])

    assert len(engine.ran("create")) == 1
    assert len(engine.ran("exec")) == 2
    assert {command[-1] for command in engine.ran("start")} == {container_id}
    assert {command[1] for command in engine.ran("exec")} == {container_id}


def test_active_execution_leases_image_container_and_all_mounted_volumes(
    project: Path, tmp_path: Path
) -> None:
    from bosn.clock import FakeClock
    from bosn.retention import KEPT_LEASED, Pressure, evaluate

    clock = FakeClock()
    engine = FakeEngine()
    with Registry(tmp_path / "dependencies.sqlite3", clock=clock) as registry:
        converger = Converger(load(project), registry, engine)  # type: ignore[arg-type]
        converged = converger.converge()
        clock.advance(3600)
        _container_id, leases = converger._acquire_execution_container(
            converged,
            stack_name=None,
            workspace=workspace_of(converger.manifest),
        )
        try:
            resources = registry.list_resources()
            assert {lease.resource_id for lease in leases} == {
                resource.id for resource in resources
            }
            assert {resource.kind for resource in resources} == {
                "container",
                "image",
                "volume",
            }
            assert len(resources) == 5
            assert {resource.last_used for resource in resources} == {clock.now()}

            clock.advance(4 * 24 * 3600)
            for resource in resources:
                verdict = evaluate(
                    registry,
                    resource,
                    superseded=True,
                    workspace_done=True,
                    pressure=Pressure(under_pressure=True),
                    alive_probe=lambda _pid, _start: True,
                )
                assert not verdict.collect
                assert verdict.reason == KEPT_LEASED
        finally:
            for lease in leases:
                registry.release_lease(lease.id)


def test_generation_roll_replaces_the_container_and_reconciles_its_row(
    project: Path, registry: Registry
) -> None:
    engine = FakeEngine()
    first_converger = Converger(load(project), registry, engine)  # type: ignore[arg-type]
    first, code, _output = first_converger.run(["true"])
    assert code == 0
    name = first_converger.container_name(workspace_of(first_converger.manifest), first.stack)
    first_container_id = str(engine.container_specs[name]["Id"])

    (project / "Dockerfile").write_text("FROM alpine\nRUN echo changed\n", encoding="utf-8")
    second_converger = Converger(load(project), registry, engine)  # type: ignore[arg-type]
    second, code, _output = second_converger.run(["true"])

    assert code == 0
    assert second.digest != first.digest
    assert len(engine.ran("container", "rm", "--force")) == 1
    assert engine.ran("container", "rm", "--force")[0][-1] == first_container_id
    assert len(engine.ran("create")) == 2
    assert engine.ran("create")[-1][-4] == second.image_tag
    second_container_id = str(engine.container_specs[name]["Id"])
    assert second_container_id != first_container_id
    assert engine.ran("start")[-1][-1] == second_container_id
    assert engine.ran("exec")[-1][1] == second_container_id
    container_rows = [row for row in registry.list_resources() if row.kind == "container"]
    assert container_rows
    assert {row.generation for row in container_rows} == {second.digest}


@pytest.mark.parametrize("collision", ["foreign", "incomplete"])
def test_foreign_and_incomplete_container_collisions_are_refused_untouched(
    converger: Converger, registry: Registry, collision: str
) -> None:
    engine: FakeEngine = converger.engine  # type: ignore[assignment]
    converged = converger.converge()
    workspace = workspace_of(converger.manifest)
    name = converger.container_name(workspace, converged.stack)
    if collision == "foreign":
        raw_labels = labels.ResourceLabels(
            registry="another-registry",
            kind="container",
            stack=converged.stack,
            generation=converged.digest,
            scope="stack",
            workspace=workspace,
            created="2026-01-01T00:00:00Z",
        ).to_dict()
    else:
        raw_labels = {labels.REGISTRY: registry.registry_id}
    engine.existing.add(name)
    engine.container_specs[name] = {
        "Id": "sha256:colliding-container",
        "Config": {"Labels": raw_labels},
        "Image": "sha256:collision",
        "Mounts": [],
    }

    with pytest.raises(EngineError, match="name collision"):
        converger.run_converged(converged, ["true"], stack_name=None, workspace=workspace)

    assert engine.ran("container", "rm", "--force") == []
    assert engine.ran("start") == []
    assert name in engine.container_specs


def test_changed_required_mount_recreates_an_owned_container(converger: Converger) -> None:
    converged, code, _output = converger.run(["true"])
    assert code == 0
    engine: FakeEngine = converger.engine  # type: ignore[assignment]
    name = converger.container_name(workspace_of(converger.manifest), converged.stack)
    mounts = engine.container_specs[name]["Mounts"]
    assert isinstance(mounts, list)
    mounts.pop()

    converger.run(["true"])

    assert len(engine.ran("container", "rm", "--force")) == 1
    assert len(engine.ran("create")) == 2


# -- bind mounts -------------------------------------------------------------


def test_volume_with_explicit_destination_mounts_there_not_under_bosn(
    tmp_path: Path, registry: Registry
) -> None:
    (tmp_path / "Dockerfile").write_text("FROM alpine\n", encoding="utf-8")
    (tmp_path / "bosn.toml").write_text(
        """
[stack.test]
dockerfile = "Dockerfile"
default = true

[stack.test.volumes]
target = { scope = "spec", destination = "/target" }
""",
        encoding="utf-8",
    )
    engine = FakeEngine()
    converger = Converger(load(tmp_path), registry, engine)  # type: ignore[arg-type]

    converger.run(["true"])

    create = engine.ran("create")[0]
    volume_args = [a for a in create if ":/target" in a]
    assert len(volume_args) == 1
    assert not any(":/bosn/target" in a for a in create)


def test_bind_mount_reaches_the_container_with_source_destination_and_readonly(
    tmp_path: Path, registry: Registry
) -> None:
    (tmp_path / "Dockerfile").write_text("FROM alpine\n", encoding="utf-8")
    (tmp_path / "secrets").mkdir()
    (tmp_path / "bosn.toml").write_text(
        """
[stack.test]
dockerfile = "Dockerfile"
default = true

[stack.test.mounts]
secrets = { source = "secrets", destination = "/etc/app", readonly = true }
""",
        encoding="utf-8",
    )
    engine = FakeEngine()
    converger = Converger(load(tmp_path), registry, engine)  # type: ignore[arg-type]

    converger.run(["true"])

    create = engine.ran("create")[0]
    source = str((tmp_path / "secrets").resolve())
    assert "--volume" in create
    assert f"{source}:/etc/app:ro" in create


def test_declared_env_reaches_docker_create_as_env_flags(
    tmp_path: Path, registry: Registry
) -> None:
    """The #105 gap: `converge.py` used to pass no `-e` at all, so a manifest `env` table
    was silently invisible to the container regardless of what it declared."""
    (tmp_path / "Dockerfile").write_text("FROM alpine\n", encoding="utf-8")
    (tmp_path / "bosn.toml").write_text(
        """
[stack.test]
dockerfile = "Dockerfile"
default = true

[stack.test.env]
CARGO_TARGET_DIR = "/target"
TMPDIR = "/target/tmp"
""",
        encoding="utf-8",
    )
    engine = FakeEngine()
    converger = Converger(load(tmp_path), registry, engine)  # type: ignore[arg-type]

    converger.run(["true"])

    create = engine.ran("create")[0]
    assert "--env" in create
    assert "CARGO_TARGET_DIR=/target" in create
    assert "TMPDIR=/target/tmp" in create


def test_no_env_table_means_no_env_flags(converger: Converger) -> None:
    converger.run(["true"])
    engine: FakeEngine = converger.engine  # type: ignore[assignment]

    assert "--env" not in engine.ran("create")[0]


def test_declared_workdir_reaches_docker_exec_as_a_workdir_flag(
    tmp_path: Path, registry: Registry
) -> None:
    """The #105 gap: `converge.py` used to pass no `-w` at all, so any command assuming it
    started inside a manifest-declared `workdir` would run from the image's own `WORKDIR`
    (or its default) instead."""
    (tmp_path / "Dockerfile").write_text("FROM alpine\n", encoding="utf-8")
    (tmp_path / "bosn.toml").write_text(
        """
[stack.test]
dockerfile = "Dockerfile"
default = true
workdir = "/repo"
""",
        encoding="utf-8",
    )
    engine = FakeEngine()
    converger = Converger(load(tmp_path), registry, engine)  # type: ignore[arg-type]

    converger.run(["cargo", "check"])

    exec_cmd = engine.ran("exec")[0]
    assert exec_cmd == ["exec", "--workdir", "/repo", exec_cmd[3], "cargo", "check"]


def test_declared_workdir_also_reaches_an_interactive_shell(
    tmp_path: Path, registry: Registry
) -> None:
    (tmp_path / "Dockerfile").write_text("FROM alpine\n", encoding="utf-8")
    (tmp_path / "bosn.toml").write_text(
        """
[stack.test]
dockerfile = "Dockerfile"
default = true
workdir = "/repo"
""",
        encoding="utf-8",
    )
    engine = FakeEngine()
    converger = Converger(load(tmp_path), registry, engine)  # type: ignore[arg-type]
    converged = converger.converge()

    converger.shell_converged(
        converged, stack_name=None, workspace=workspace_of(converger.manifest)
    )

    exec_cmd = engine.ran("exec")[0]
    assert exec_cmd[1:3] == ["-it", "--workdir"]
    assert exec_cmd[3] == "/repo"


def test_no_workdir_means_no_workdir_flag(converger: Converger) -> None:
    converger.run(["true"])
    engine: FakeEngine = converger.engine  # type: ignore[assignment]

    exec_cmd = engine.ran("exec")[0]
    assert "--workdir" not in exec_cmd


def test_bind_mount_source_is_never_registered_labeled_or_collected(
    bind_project: Path, registry: Registry
) -> None:
    """bosn owns volumes and may delete them; it only references a bind source."""
    engine = FakeEngine()
    converger = Converger(load(bind_project), registry, engine)  # type: ignore[arg-type]

    converger.run(["true"])

    resource_kinds = {r.kind for r in registry.list_resources()}
    assert resource_kinds <= {"image", "container"}
    repo_source = str((bind_project / "repo").resolve())
    assert all(r.name != repo_source for r in registry.list_resources())
    assert engine.ran("volume", "create") == []


def test_container_matching_its_full_declared_mount_set_is_reused(
    bind_project: Path, registry: Registry
) -> None:
    """The opposite failure: a bind-mounted container must not be needlessly replaced.

    RED before the Task 2 fix: the old filter only ever counted `/bosn/*` and the
    heartbeat as "managed", so a declared bind destination like `/repo` never appeared in
    that set. The equality check against the full expected set then failed unconditionally,
    and a container whose bind mount was correct in every respect still got torn down and
    recreated on every single run.
    """
    engine = FakeEngine()
    converger = Converger(load(bind_project), registry, engine)  # type: ignore[arg-type]
    converger.run(["true"])
    assert len(engine.ran("create")) == 1

    converger.run(["true"])

    assert len(engine.ran("create")) == 1, "an unchanged bind mount must not force a rebuild"
    assert engine.ran("container", "rm", "--force") == []


def test_bind_mount_source_drift_is_not_silently_reused() -> None:
    """RED before the Task 2 fix: `/repo` is outside `/bosn/*`, so the old filter dropped it
    from the "managed" set entirely and the per-entry source comparison below was never
    reached -- a drifted bind source was invisible to the match, not merely tolerated."""
    expected = {
        "/bosn-daemon/heartbeat": {
            "type": "bind",
            "source": "/state/heartbeat",
            "destination": "/bosn-daemon/heartbeat",
            "rw": False,
        },
        "/repo": {
            "type": "bind",
            "source": "/host/repo",
            "destination": "/repo",
            "rw": True,
        },
    }
    matching = [
        {
            "Type": "bind",
            "Source": "/state/heartbeat",
            "Destination": "/bosn-daemon/heartbeat",
            "RW": False,
        },
        {"Type": "bind", "Source": "/host/repo", "Destination": "/repo", "RW": True},
    ]
    drifted_source = [
        {
            "Type": "bind",
            "Source": "/state/heartbeat",
            "Destination": "/bosn-daemon/heartbeat",
            "RW": False,
        },
        {"Type": "bind", "Source": "/host/OLD-repo", "Destination": "/repo", "RW": True},
    ]
    drifted_destination = [
        {
            "Type": "bind",
            "Source": "/state/heartbeat",
            "Destination": "/bosn-daemon/heartbeat",
            "RW": False,
        },
        {"Type": "bind", "Source": "/host/repo", "Destination": "/moved", "RW": True},
    ]

    assert Converger._container_mounts_match(matching, expected) is True
    assert Converger._container_mounts_match(drifted_source, expected) is False
    assert Converger._container_mounts_match(drifted_destination, expected) is False


def test_changed_image_id_recreates_an_owned_container(converger: Converger) -> None:
    converged, code, _output = converger.run(["true"])
    assert code == 0
    engine: FakeEngine = converger.engine  # type: ignore[assignment]
    name = converger.container_name(workspace_of(converger.manifest), converged.stack)
    engine.container_specs[name]["Image"] = "sha256:not-the-converged-image"

    converger.run(["true"])

    assert len(engine.ran("container", "rm", "--force")) == 1
    assert len(engine.ran("create")) == 2


@pytest.mark.parametrize("failure", ["validation", "start"])
def test_failed_new_container_is_removed_by_immutable_id(
    project: Path, registry: Registry, failure: str
) -> None:
    class PostCreateFailingEngine(FakeEngine):
        def __init__(self) -> None:
            super().__init__()
            self.created_id = ""

        def run(
            self, args: list[str], *, check: bool = False, timeout: float | None = None
        ) -> EngineResult:
            result = super().run(args, check=check, timeout=timeout)
            if args[0] == "create":
                self.created_id = result.stdout
                if failure == "validation":
                    name = args[args.index("--name") + 1]
                    self.container_specs[name]["Mounts"] = []
            if args[0] == "start" and failure == "start":
                return EngineResult(1, "", "start failed")
            return result

    engine = PostCreateFailingEngine()
    converger = Converger(load(project), registry, engine)  # type: ignore[arg-type]

    with pytest.raises(EngineError, match="does not match|starting .* failed"):
        converger.run(["true"])

    assert engine.created_id
    assert engine.ran("container", "rm", "--force") == [
        ["container", "rm", "--force", engine.created_id]
    ]
    assert engine.container_specs == {}
    assert all(resource.kind != "container" for resource in registry.list_resources())


def test_new_container_is_removed_if_lease_acquisition_fails(
    project: Path, registry: Registry, monkeypatch: pytest.MonkeyPatch
) -> None:
    class TrackingEngine(FakeEngine):
        def __init__(self) -> None:
            super().__init__()
            self.created_id = ""

        def run(
            self, args: list[str], *, check: bool = False, timeout: float | None = None
        ) -> EngineResult:
            result = super().run(args, check=check, timeout=timeout)
            if args[0] == "create":
                self.created_id = result.stdout
            return result

    engine = TrackingEngine()
    converger = Converger(load(project), registry, engine)  # type: ignore[arg-type]
    converged = converger.converge()

    def fail_acquire(*_args: object, **_kwargs: object) -> None:
        raise RuntimeError("injected lease failure")

    monkeypatch.setattr(registry, "acquire_lease", fail_acquire)
    with pytest.raises(RuntimeError, match="injected lease failure"):
        converger.run_converged(
            converged,
            ["true"],
            workspace=workspace_of(converger.manifest),
        )

    assert engine.ran("container", "rm", "--force") == [
        ["container", "rm", "--force", engine.created_id]
    ]
    assert engine.container_specs == {}
    assert all(resource.kind != "container" for resource in registry.list_resources())


def test_reused_container_is_not_removed_if_lease_acquisition_fails(
    converger: Converger, registry: Registry, monkeypatch: pytest.MonkeyPatch
) -> None:
    converged, code, _output = converger.run(["true"])
    assert code == 0
    engine: FakeEngine = converger.engine  # type: ignore[assignment]
    name = converger.container_name(workspace_of(converger.manifest), converged.stack)
    container_id = engine.container_specs[name]["Id"]

    def fail_acquire(*_args: object, **_kwargs: object) -> None:
        raise RuntimeError("injected lease failure")

    monkeypatch.setattr(registry, "acquire_lease", fail_acquire)
    with pytest.raises(RuntimeError, match="injected lease failure"):
        converger.run_converged(
            converged,
            ["true"],
            workspace=workspace_of(converger.manifest),
        )

    assert engine.ran("container", "rm", "--force") == []
    assert engine.container_specs[name]["Id"] == container_id


def test_client_manifest_volume_drift_is_refused_before_container_mutation(
    converger: Converger,
) -> None:
    converged = converger.converge()
    stale_snapshot = replace(converged, volumes=("wrong-volume", *converged.volumes[1:]))
    engine: FakeEngine = converger.engine  # type: ignore[assignment]

    with pytest.raises(EngineError, match="volume contract changed"):
        converger.run_converged(
            stale_snapshot,
            ["true"],
            stack_name=None,
            workspace=workspace_of(converger.manifest),
        )

    assert engine.ran("container", "inspect") == []
    assert engine.ran("create") == []


def test_active_execution_lease_prevents_generation_replacement(
    project: Path, registry: Registry
) -> None:
    engine = FakeEngine()
    converger = Converger(load(project), registry, engine)  # type: ignore[arg-type]
    first, code, _output = converger.run(["true"])
    assert code == 0
    resource = next(row for row in registry.list_resources() if row.kind == "container")
    lease = registry.acquire_lease(resource.id, pid=os.getpid(), proc_start=live_proc_start())
    try:
        (project / "Dockerfile").write_text("FROM alpine\nRUN echo changed\n", encoding="utf-8")
        rolled_converger = Converger(load(project), registry, engine)  # type: ignore[arg-type]
        rolled = rolled_converger.converge()
        assert rolled.digest != first.digest

        with pytest.raises(EngineError, match="active execution lease"):
            rolled_converger.run_converged(
                rolled,
                ["true"],
                stack_name=None,
                workspace=workspace_of(rolled_converger.manifest),
            )
    finally:
        registry.release_lease(lease.id)

    assert engine.ran("container", "rm", "--force") == []


def test_shell_uses_an_interactive_tty_exec(converger: Converger) -> None:
    converged = converger.converge()
    assert (
        converger.shell_converged(
            converged,
            stack_name=None,
            workspace=workspace_of(converger.manifest),
        )
        == 7
    )
    engine: FakeEngine = converger.engine  # type: ignore[assignment]
    assert engine.ran("exec")[-1][1] == "-it"
    name = converger.container_name(workspace_of(converger.manifest), converged.stack)
    assert engine.ran("exec")[-1][2] == engine.container_specs[name]["Id"]


@pytest.mark.parametrize(
    ("actual", "expected", "matches"),
    [
        (r"C:\Users\Alice\state\heartbeat", r"C:\Users\Alice\state\heartbeat", True),
        (
            "/run/desktop/mnt/host/c/Users/Alice/state/heartbeat",
            r"C:\Users\Alice\state\heartbeat",
            True,
        ),
        (
            "/host_mnt/c/Users/Alice/state/heartbeat",
            r"C:\Users\Alice\state\heartbeat",
            True,
        ),
        (
            "/run/desktop/mnt/host/d/Users/Alice/state/heartbeat",
            r"C:\Users\Alice\state\heartbeat",
            False,
        ),
        ("/different/path", r"C:\Users\Alice\state\heartbeat", False),
    ],
)
def test_docker_desktop_bind_source_equivalence(actual: str, expected: str, matches: bool) -> None:
    assert Converger._bind_sources_match(actual, expected) is matches


def test_persistent_container_has_daemon_loss_and_run_duration_watchdogs(
    converger: Converger,
) -> None:
    converger.run(["true"])
    engine: FakeEngine = converger.engine  # type: ignore[assignment]
    create = engine.ran("create")[0]
    assert "--rm" in create
    assert any("daemon.heartbeat:/bosn-daemon/heartbeat:ro" in arg for arg in create)
    assert any("stat -c %Y" in arg for arg in create)
    assert "started=$(date +%s)" not in create[-1]


def test_execution_uses_configured_deadline_not_engine_control_timeout(
    converger: Converger,
) -> None:
    converger.run_max_duration = 123.0
    converger.run(["true"])
    engine: FakeEngine = converger.engine  # type: ignore[assignment]
    assert engine.timeouts[-1] == 123.0
