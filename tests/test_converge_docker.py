"""Phase 5 end-to-end: a real bosn.toml builds, runs, labels, and registers.

Docker-marked: Linux CI only.
"""

from __future__ import annotations

import uuid
from collections.abc import Iterator
from pathlib import Path

import pytest

from bosn import labels
from bosn.converge import Converger, run_task
from bosn.engine import Engine
from bosn.manifest import load
from bosn.registry import Registry
from bosn.resources import ResourceScanner

pytestmark = pytest.mark.docker

MANIFEST = """
[stack.test]
dockerfile = "Dockerfile"
family = "alpinetest"
default = true

[stack.test.volumes]
work = { scope = "spec" }

[task.hello]
stack = "test"
cmd = "echo task-ran"
"""


@pytest.fixture
def project(tmp_path: Path) -> Path:
    (tmp_path / "Dockerfile").write_text(
        f"FROM alpine:3.20\nRUN echo {uuid.uuid4().hex[:8]} > /marker\n", encoding="utf-8"
    )
    (tmp_path / "bosn.toml").write_text(MANIFEST, encoding="utf-8")
    return tmp_path


@pytest.fixture
def registry(tmp_path: Path) -> Iterator[Registry]:
    with Registry(tmp_path / "state" / "r.sqlite3") as reg:
        yield reg


@pytest.fixture
def converger(project: Path, registry: Registry, engine: Engine) -> Iterator[Converger]:
    conv = Converger(load(project), registry, engine)
    try:
        yield conv
    finally:
        for resource in registry.list_resources():
            if resource.kind == "container":
                engine.run(["rm", "--force", resource.name])
            elif resource.kind == "volume":
                engine.run(["volume", "rm", "--force", resource.name])
            elif resource.kind == "image":
                engine.run(["image", "rm", "--force", resource.name])


def test_run_builds_and_executes_end_to_end(converger: Converger) -> None:
    result, code, output = converger.run(["echo", "ok-from-container"])
    assert code == 0, output
    assert "ok-from-container" in output
    assert result.digest.startswith("sha256:")


def test_everything_created_is_labeled_and_registered(
    converger: Converger, registry: Registry, engine: Engine
) -> None:
    converger.run(["true"])

    registered = {r.name for r in registry.list_resources()}
    assert registered, "converge registered nothing"

    scan = ResourceScanner(engine).scan(registry.registry_id, kinds=["volume"])
    owned = {r.name for r in scan.owned}
    volumes = {r.name for r in registry.list_resources() if r.kind == "volume"}
    assert volumes <= owned, "every created volume must carry complete ownership labels"


def test_created_volume_labels_round_trip(
    converger: Converger, registry: Registry, engine: Engine
) -> None:
    converger.run(["true"])
    volume = next(r for r in registry.list_resources() if r.kind == "volume")
    raw = ResourceScanner(engine).inspect_labels("volume", volume.name)
    assert labels.is_owned_by(raw, registry.registry_id)
    assert labels.ResourceLabels.from_dict(raw).stack == "test"


def test_second_run_reuses_and_does_not_rebuild(converger: Converger) -> None:
    first, code, _ = converger.run(["true"])
    assert code == 0
    second, code, _ = converger.run(["true"])
    assert code == 0
    assert second.digest == first.digest
    assert second.action == "reused"


def test_editing_a_copy_input_rolls_and_builds_a_new_image(
    project: Path, converger: Converger, engine: Engine
) -> None:
    (project / "Dockerfile").write_text(
        "FROM alpine:3.20\nCOPY payload /payload\n", encoding="utf-8"
    )
    payload = project / "payload"
    payload.write_text("one", encoding="utf-8")
    first = converger.converge()

    payload.write_text("two", encoding="utf-8")
    second = converger.converge()

    assert second.digest != first.digest
    assert second.action == "rolled"
    assert second.image_tag != first.image_tag
    assert engine.run(["image", "inspect", first.image_tag]).ok
    assert engine.run(["image", "inspect", second.image_tag]).ok


def test_a_manifest_task_runs(project: Path, registry: Registry, engine: Engine) -> None:
    _result, code, output = run_task(load(project), registry, "hello", engine=engine)
    try:
        assert code == 0, output
        assert "task-ran" in output
    finally:
        for resource in registry.list_resources():
            if resource.kind == "container":
                engine.run(["rm", "--force", resource.name])
            elif resource.kind == "volume":
                engine.run(["volume", "rm", "--force", resource.name])
            elif resource.kind == "image":
                engine.run(["image", "rm", "--force", resource.name])


def test_a_failing_command_propagates_its_exit_code(converger: Converger) -> None:
    _result, code, _output = converger.run(["sh", "-c", "exit 7"])
    assert code == 7


def test_an_image_backed_stack_runs_the_declared_image(
    tmp_path: Path, registry: Registry, engine: Engine
) -> None:
    (tmp_path / "bosn.toml").write_text(
        '[stack.image]\nimage = "alpine:3.20"\ndefault = true\n', encoding="utf-8"
    )
    converger = Converger(load(tmp_path), registry, engine)
    try:
        result, code, output = converger.run(["cat", "/etc/alpine-release"])
        assert code == 0, output
        assert result.image_tag.startswith("sha256:")
        assert output.strip()
    finally:
        for resource in registry.list_resources():
            if resource.kind == "container":
                engine.run(["rm", "--force", resource.name])
