"""#84: the Docker suite's own autouse teardown must not leak, and must not overreach.

Three things are proved against a real engine:

- precision: the sweep in `conftest._cleanup_docker_test_resources` removes a resource
  carrying a minted registry id and leaves everything else -- including a resource
  labeled with a registry id nobody in this process ever constructed -- untouched. This
  is the same convention `test_resources_docker.py` and `test_scenario_docker.py` use for
  a "foreign" registry: a bare uuid string, never a real `Registry()`.
- a removal failure is surfaced (visible), not swallowed and not raised (never fails the
  test being torn down).
- end to end: a Docker-marked test that creates a real labeled volume and does nothing
  else leaves no volume behind once its own teardown has run.

Docker-marked: Linux CI only.
"""

from __future__ import annotations

import uuid

import pytest

from bosn import labels
from bosn.engine import Engine, EngineResult
from bosn.registry import Registry
from conftest import _sweep_minted_registries

pytestmark = pytest.mark.docker


def _labels(registry: str) -> labels.ResourceLabels:
    return labels.ResourceLabels(
        registry=registry,
        kind="volume",
        stack="test",
        generation="sha256:deadbeef",
        scope="spec",
        workspace="/w/teardown",
        created="2026-08-14T00:00:00Z",
    )


def _volume_names(engine: Engine) -> set[str]:
    listed = engine.run(["volume", "ls", "--quiet"])
    return {line.strip() for line in listed.stdout.splitlines() if line.strip()}


# -- precision: only the named registry id is ever touched -------------------


def test_sweep_removes_only_the_named_registry_id(engine: Engine) -> None:
    swept_id = f"sweep-target-{uuid.uuid4()}"
    other_id = f"sweep-bystander-{uuid.uuid4()}"
    swept_name = f"bosn-sweep-target-{uuid.uuid4().hex[:8]}"
    other_name = f"bosn-sweep-bystander-{uuid.uuid4().hex[:8]}"
    engine.run(["volume", "create", *_labels(swept_id).to_docker_args(), swept_name], check=True)
    engine.run(["volume", "create", *_labels(other_id).to_docker_args(), other_name], check=True)
    try:
        errors = _sweep_minted_registries(engine, {swept_id})
        assert errors == []

        names = _volume_names(engine)
        assert swept_name not in names, "the named registry's volume must be removed"
        assert other_name in names, "a registry id not in the set must never be touched"
    finally:
        engine.run(["volume", "rm", "--force", other_name])


def test_sweep_with_no_minted_ids_is_a_no_op(engine: Engine) -> None:
    """An empty minted set (a test that never constructed a writable Registry) removes nothing."""
    untouched_id = f"sweep-untouched-{uuid.uuid4()}"
    untouched_name = f"bosn-sweep-untouched-{uuid.uuid4().hex[:8]}"
    engine.run(
        ["volume", "create", *_labels(untouched_id).to_docker_args(), untouched_name],
        check=True,
    )
    try:
        errors = _sweep_minted_registries(engine, set())
        assert errors == []
        assert untouched_name in _volume_names(engine)
    finally:
        engine.run(["volume", "rm", "--force", untouched_name])


# -- a removal failure is surfaced, never raised ------------------------------


def test_sweep_reports_a_removal_failure_without_raising(
    engine: Engine, monkeypatch: pytest.MonkeyPatch
) -> None:
    failing_id = f"sweep-failing-{uuid.uuid4()}"
    failing_name = f"bosn-sweep-failing-{uuid.uuid4().hex[:8]}"
    engine.run(
        ["volume", "create", *_labels(failing_id).to_docker_args(), failing_name], check=True
    )
    real_run = Engine.run

    def _failing_run(self: Engine, args: list[str], **kwargs: object) -> EngineResult:
        if args[:2] == ["volume", "rm"]:
            return EngineResult(returncode=1, stdout="", stderr="simulated removal failure")
        return real_run(self, args, **kwargs)  # type: ignore[arg-type]

    try:
        with monkeypatch.context() as patch:
            patch.setattr(Engine, "run", _failing_run)
            errors = _sweep_minted_registries(engine, {failing_id})
        assert errors, "a failed removal must be reported, not silently dropped"
        assert "simulated removal failure" in errors[0]
        assert failing_name in _volume_names(engine), "a failed removal must not be pretended"
    finally:
        engine.run(["volume", "rm", "--force", failing_name])


# -- end to end: RED -> GREEN --------------------------------------------------
#
# A Docker-marked test that constructs a registry and creates one labeled volume, and does
# nothing else, must leave no volume behind once ITS OWN autouse teardown has run. That
# teardown runs after this test function returns, so the follow-up assertion has to live in
# a second test ordered after it in the same module -- pytest collects a file's tests in
# source order, so `_b` genuinely runs once `_a`'s teardown has already fired.

_minted_volume: str | None = None
_live_volume: str | None = None
_live_registry_id: str | None = None


def test_a_a_docker_marked_test_creates_one_labeled_volume(tmp_path, engine: Engine) -> None:
    global _minted_volume, _live_volume, _live_registry_id
    with Registry(tmp_path / "state" / "r.sqlite3") as registry:
        _minted_volume = f"bosn-teardown-e2e-{uuid.uuid4().hex[:8]}"
        engine.run(
            ["volume", "create", *_labels(registry.registry_id).to_docker_args(), _minted_volume],
            check=True,
        )
        assert engine.run(["volume", "inspect", _minted_volume]).ok

    # A "different, live" registry, simulated the same way the rest of the docker suite
    # simulates a foreign registry: a bare id, never passed through `Registry()`, so it can
    # never land in this test's minted set no matter what this test does.
    _live_registry_id = f"live-{uuid.uuid4()}"
    _live_volume = f"bosn-teardown-e2e-live-{uuid.uuid4().hex[:8]}"
    engine.run(
        ["volume", "create", *_labels(_live_registry_id).to_docker_args(), _live_volume],
        check=True,
    )


def test_b_that_volume_is_gone_and_the_live_one_survived(engine: Engine) -> None:
    if _minted_volume is None or _live_volume is None:
        pytest.skip("must run after test_a_a_docker_marked_test_creates_one_labeled_volume")
    try:
        names = _volume_names(engine)
        assert _minted_volume not in names, (
            "the autouse teardown must have removed the previous test's own volume"
        )
        assert _live_volume in names, "a live registry's volume must never be swept"
    finally:
        engine.run(["volume", "rm", "--force", _live_volume])
