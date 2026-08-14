"""Phase 5: converge semantics and volume-scope naming. No Docker needed."""

from __future__ import annotations

import sys
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
        if args[0] == "create":
            self.existing.add(args[args.index("--name") + 1])
        if args[0] == "build":
            self.existing.add(args[args.index("--tag") + 1])
        return EngineResult(0, "ok", "")

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

    assert registry.generation_superseded_at(good.digest) is None


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


def test_second_run_reuses_the_same_persistent_container(converger: Converger) -> None:
    converger.run(["true"])
    converger.run(["true"])
    engine: FakeEngine = converger.engine  # type: ignore[assignment]
    assert len(engine.ran("create")) == 1
    assert len(engine.ran("exec")) == 2


def test_shell_uses_an_interactive_tty_exec(converger: Converger) -> None:
    converged = converger.converge()
    assert converger.shell_converged(converged, stack_name=None, workspace="/workspace") == 7
    engine: FakeEngine = converger.engine  # type: ignore[assignment]
    assert engine.ran("exec")[-1][1] == "-it"


def test_persistent_container_has_daemon_loss_and_run_duration_watchdogs(
    converger: Converger,
) -> None:
    converger.run(["true"])
    engine: FakeEngine = converger.engine  # type: ignore[assignment]
    create = engine.ran("create")[0]
    assert "--rm" in create
    assert any("daemon.heartbeat:/bosn-daemon/heartbeat:ro" in arg for arg in create)
    assert any("stat -c %Y" in arg and "started" in arg for arg in create)
