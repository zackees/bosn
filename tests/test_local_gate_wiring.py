"""The local gate uses the shared CI tool and source workflow checks. Isolation guards
refuse a bare host and the isolated image sets the marker, and every CI job
waits for the verify job."""

from __future__ import annotations

from pathlib import Path

import tomllib
import yaml

ROOT = Path(__file__).resolve().parent.parent

GATE = tomllib.loads((ROOT / "local-gate.toml").read_text(encoding="utf-8"))["gate"]
WORKFLOW = yaml.safe_load((ROOT / ".github" / "workflows" / "ci.yml").read_text(encoding="utf-8"))


def test_every_declared_lane_runs_a_source_bound_isolated_replay() -> None:
    lanes = GATE["lanes"]
    assert set(lanes) == {"rust", "tests"}
    assert GATE["run"][:3] == ["bosn", "ci", "run"]
    replay = GATE["replay"]
    assert replay["qualified"] and replay["report-source"] == "stdout"
    assert replay["provider-query"] == ["bosn", "ci", "runners", "list", "--json"]
    for name, lane in lanes.items():
        command = lane["run"]
        assert command[:3] == ["bosn", "ci", "run"]
        selection = next(item for item in replay["selections"] if item["lane"] == name)
        assert command[command.index("--job") + 1] == selection["job"]
        assert command[command.index("--event") + 1] == selection["event"]
        assert command[command.index("--input") + 1] == f"tier={selection['inputs']['tier']}"
        assert "--ci-output" in command and "verify:skip_rust" in command
    assert all("steps" not in job and "key" not in job for job in replay["jobs"])
    requested = {
        command[index + 1]
        for lane in lanes.values()
        for command in [lane["run"]]
        for index, token in enumerate(command)
        if token == "--ci-output"
    }
    full = GATE["run"]
    assert requested <= {
        full[index + 1] for index, token in enumerate(full) if token == "--ci-output"
    }


def test_python_guards_and_policy_validation_remain_in_the_test_lane() -> None:
    linux = WORKFLOW["jobs"]["linux"]
    commands = [step.get("run", "") for step in linux["steps"]]
    assert "./lint" in commands and "./test" in commands
    assert any("ci-lint local-gate lint" in command for command in commands)
    definition = yaml.safe_load((ROOT / "ci-attestations.yml").read_text(encoding="utf-8"))
    assert definition["gates"]["python/all/static"]["lane"] == "tests"
    assert definition["gates"]["general/all/ci-policy"]["lane"] == "tests"


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

    exempt = set(GATE.get("verify-exempt", []))
    assert [job for job in jobs if job not in exempt and not reaches_verify(job)] == []
    assert all("needs" not in jobs[job] for job in exempt), "exempt jobs wait on nothing"


def test_every_trusted_skip_maps_to_attested_gates_and_is_wired() -> None:
    definition = yaml.safe_load((ROOT / "ci-attestations.yml").read_text(encoding="utf-8"))
    lanes = set(GATE["lanes"])
    assert {gate["lane"] for gate in definition["gates"].values()} <= lanes
    skip = GATE["trust"]["skip"]
    assert sorted(definition["jobs"]) == sorted(skip), "skip exactly the mapped jobs"
    for ref in skip:
        gates = definition["jobs"][ref]
        assert gates and set(gates) <= set(definition["gates"]), ref
        covered = set(GATE["trust"]["covered-by"][ref])
        assert {definition["gates"][gate]["lane"] for gate in gates} <= covered, ref
        job = WORKFLOW["jobs"][ref.split(":", 1)[1]]
        assert f"needs.verify.outputs.skip_{ref.split(':', 1)[1]} != 'true'" in job["if"], ref


def test_ci_verifies_with_the_ci_lint_the_gate_runs() -> None:
    pin = next(
        step["with"]["ref"]
        for step in WORKFLOW["jobs"]["verify"]["steps"]
        if step.get("with", {}).get("repository") == "zackees/ci.yml"
    )
    checkout = next(
        step
        for step in WORKFLOW["jobs"]["verify"]["steps"]
        if step.get("with", {}).get("repository") == "zackees/ci.yml"
    )
    assert checkout["with"]["ref"] == pin
    assert pin in (ROOT / "local-gate.toml").read_text(encoding="utf-8")
    assert any(
        pin in step.get("run", "") and "ci-lint local-gate lint" in step.get("run", "")
        for step in WORKFLOW["jobs"]["linux"]["steps"]
    )
