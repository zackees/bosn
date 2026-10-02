"""The local gate (zackees/ci.yml GATE-001..007, bosn#361) stays wired: every
lane `local-gate.toml` declares exists in ci/local_gate.py, the isolation guards
refuse a bare host and the isolated image sets the marker, and every CI job
waits for the verify job."""

from __future__ import annotations

import importlib.util
import sys
from pathlib import Path

import tomllib
import yaml

ROOT = Path(__file__).resolve().parent.parent

SPEC = importlib.util.spec_from_file_location("local_gate", ROOT / "ci" / "local_gate.py")
assert SPEC is not None and SPEC.loader is not None
local_gate = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = local_gate
SPEC.loader.exec_module(local_gate)

GATE = tomllib.loads((ROOT / "local-gate.toml").read_text(encoding="utf-8"))["gate"]
WORKFLOW = yaml.safe_load((ROOT / ".github" / "workflows" / "ci.yml").read_text(encoding="utf-8"))


def test_every_declared_lane_runs_its_own_local_gate_lane() -> None:
    lanes = GATE["lanes"]
    assert set(lanes) == set(local_gate.LANES)
    for name, lane in lanes.items():
        assert lane["run"][-2:] == ["--lane", name], name
        assert lane["run"][:-2] == GATE["run"], f"{name} is the gate command plus its lane"


def test_the_isolation_guards_refuse_a_bare_host_and_the_image_lifts_them() -> None:
    marker = GATE["isolation"]["marker"]
    for guard in (ROOT / GATE["isolation"]["guard"], ROOT / "ci" / "test_guard.sh"):
        text = guard.read_text(encoding="utf-8")
        assert marker in text and ('"CI"' in text or "CI:-" in text), guard
    assert f"ENV {marker}=1" in (ROOT / "docker" / "test.Dockerfile").read_text(encoding="utf-8")
    assert 'runner = "ci/test_guard.sh"' in (ROOT / ".cargo" / "config.toml").read_text(
        encoding="utf-8"
    )
    bosn = tomllib.loads((ROOT / "bosn.toml").read_text(encoding="utf-8"))
    for task in ("test", "rust-test"):
        assert bosn["task"][task]["stack"] == "test", task


def test_every_ci_job_waits_for_the_verify_job() -> None:
    jobs = WORKFLOW["jobs"]
    assert "if" not in jobs["verify"], "the verify job always runs (GATE-002)"

    def needs(job: str) -> list[str]:
        value = jobs[job].get("needs", [])
        return [value] if isinstance(value, str) else list(value)

    def reaches_verify(job: str) -> bool:
        return job == "verify" or any(reaches_verify(parent) for parent in needs(job))

    assert [job for job in jobs if not reaches_verify(job)] == []


def test_ci_verifies_with_the_ci_lint_the_gate_runs() -> None:
    pin = local_gate.CI_LINT.rsplit("@", 1)[1]
    checkout = next(
        step
        for step in WORKFLOW["jobs"]["verify"]["steps"]
        if step.get("with", {}).get("repository") == "zackees/ci.yml"
    )
    assert checkout["with"]["ref"] == pin
    assert pin in (ROOT / "local-gate.toml").read_text(encoding="utf-8")
