"""Phase 5: converge semantics and volume-scope naming. No Docker needed."""

from __future__ import annotations

from pathlib import Path

import pytest

from bosn.converge import (
    REGISTERED,
    REUSED,
    ROLLED,
    Converger,
    volume_name_for,
)
from bosn.engine import EngineResult
from bosn.manifest import load
from bosn.registry import Registry

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
        self.existing: set[str] = set()

    def run(self, args: list[str], *, check: bool = False) -> EngineResult:
        self.commands.append(list(args))
        if "inspect" in args:
            return EngineResult(0 if args[-1] in self.existing else 1, "", "")
        if args[:2] == ["volume", "create"]:
            self.existing.add(args[-1])
        if args[0] == "build":
            self.existing.add(args[args.index("--tag") + 1])
        return EngineResult(0, "ok", "")

    def ran(self, *prefix: str) -> list[list[str]]:
        return [c for c in self.commands if c[: len(prefix)] == list(prefix)]


@pytest.fixture
def project(tmp_path: Path) -> Path:
    (tmp_path / "Dockerfile").write_text("FROM alpine\n", encoding="utf-8")
    (tmp_path / "bosn.toml").write_text(SAMPLE, encoding="utf-8")
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


def test_the_old_generation_is_marked_superseded(project: Path, registry: Registry) -> None:
    engine = FakeEngine()
    first = Converger(load(project), registry, engine).converge()  # type: ignore[arg-type]
    (project / "Dockerfile").write_text("FROM debian\n", encoding="utf-8")
    second = Converger(load(project), registry, engine).converge()  # type: ignore[arg-type]

    assert registry.generation_superseded_at(first.digest) is not None
    assert registry.generation_superseded_at(second.digest) is None


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


# -- running ---------------------------------------------------------------


def test_run_converges_first_then_runs(converger: Converger) -> None:
    _result, code, _output = converger.run(["echo", "hi"])
    engine: FakeEngine = converger.engine  # type: ignore[assignment]
    assert code == 0
    assert engine.ran("build"), "converge must have happened before the run"
    run_cmd = engine.ran("run")[0]
    assert run_cmd[-2:] == ["echo", "hi"]
    assert "--rm" in run_cmd


def test_run_mounts_every_declared_volume(converger: Converger) -> None:
    converger.run(["true"])
    engine: FakeEngine = converger.engine  # type: ignore[assignment]
    mounts = [a for a in engine.ran("run")[0] if a.startswith("bosn-")]
    assert len(mounts) == 3
