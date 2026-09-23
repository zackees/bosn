"""The expensive CI lanes are selected only by explicit tier requests."""

import os
import subprocess
import sys
from pathlib import Path

import pytest
import yaml

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "ci"))
import select_ci_tier  # noqa: E402

ROOT = Path(__file__).resolve().parents[1]
CI = yaml.safe_load((ROOT / ".github/workflows/ci.yml").read_text())


def test_expensive_jobs_require_selector() -> None:
    jobs = CI["jobs"]
    for name, tier in {"darwin-hosted-smoke": "full"}.items():
        assert "select-tier" in jobs[name]["needs"]
        assert f"needs.select-tier.outputs.{tier} == 'true'" in jobs[name]["if"]


def test_required_tiered_jobs_fail_closed_when_selector_fails() -> None:
    for name in ("linux", "native-wheel", "darwin-cross-wheel"):
        job = CI["jobs"][name]
        assert job["if"] == "always()", "required checks must run after selector failure"
        assert "select-tier" in job["needs"]
        verify = job["steps"][0]
        assert verify["name"] == "Verify selected CI tier"
        assert verify["shell"] == "bash"
        assert verify["if"] == "always()"
        assert verify["env"]["SELECT_RESULT"] == "${{ needs.select-tier.result }}"
        assert "minimal:false:false|test:true:false|full:true:true" in verify["run"]
    linux = CI["jobs"]["linux"]
    assert linux["steps"][1]["name"] == "Minimal tier required-check placeholder"
    assert linux["steps"][1]["if"] == "needs.select-tier.outputs.test != 'true'"
    for step in linux["steps"][2:]:
        assert step["if"] == "needs.select-tier.outputs.test == 'true'", step


@pytest.mark.parametrize(
    ("result", "tier", "test", "full", "expected"),
    [
        ("success", "minimal", "false", "false", 0),
        ("success", "test", "true", "false", 0),
        ("success", "full", "true", "true", 0),
        ("failure", "minimal", "false", "false", 1),
        ("success", "", "", "", 1),
        ("success", "full", "false", "true", 1),
    ],
)
def test_required_check_tier_guard(
    result: str, tier: str, test: str, full: str, expected: int
) -> None:
    guard = CI["jobs"]["linux"]["steps"][0]["run"]
    env = os.environ | {
        "SELECT_RESULT": result,
        "CI_TIER": tier,
        "RUN_TESTS": test,
        "RUN_FULL": full,
    }
    outcome = subprocess.run(["bash", "-e", "-c", guard], env=env, capture_output=True)
    assert outcome.returncode == expected


def test_required_wheel_matrix_cells_exist_on_every_tier() -> None:
    jobs = CI["jobs"]
    for name, expected_cells in {
        "native-wheel": {"ubuntu-latest", "windows-latest"},
        "darwin-cross-wheel": {"x86_64-apple-darwin", "aarch64-apple-darwin"},
    }.items():
        job = jobs[name]
        assert "select-tier" in job["needs"]
        assert job["if"] == "always()", "matrix must expand even if selector fails"
        matrix = job["strategy"]["matrix"]
        cells = set(matrix.get("os", [])) or {cell["target"] for cell in matrix["include"]}
        assert cells == expected_cells
        assert job["steps"][1]["name"] == "Minimal tier required-check placeholder"
        assert job["steps"][1]["if"] == "needs.select-tier.outputs.full != 'true'"
        for step in job["steps"][2:]:
            assert "needs.select-tier.outputs.full == 'true'" in step.get("if", ""), step


def test_required_status_names_remain() -> None:
    jobs = CI["jobs"]
    assert jobs["lint-no-macos-runners"]["name"] == "CI policy (no hosted macOS runners)"
    assert jobs["rust"]["name"] == "Rust workspace (locked tests)"
    assert "if" not in jobs["rust"]


def test_label_changes_reselect_same_sha() -> None:
    payload = {"pull_request": {"head": {"sha": "fixed"}, "labels": []}}
    assert select_ci_tier.select("pull_request", payload) == "minimal"
    payload["pull_request"]["labels"] = [{"name": "ci-test"}]
    assert select_ci_tier.select("pull_request", payload) == "test"
    payload["pull_request"]["labels"].append({"name": "ci-full"})
    assert select_ci_tier.select("pull_request", payload) == "full"
    payload["pull_request"]["labels"] = []
    assert select_ci_tier.select("pull_request", payload) == "minimal"


def test_manual_and_main_tiers() -> None:
    assert select_ci_tier.select("push", {}) == "minimal"
    assert select_ci_tier.select("workflow_dispatch", {}, "full", "a" * 40) == "full"
    with pytest.raises(ValueError, match="40-character"):
        select_ci_tier.select("workflow_dispatch", {}, "full", "not-a-sha")


def test_every_job_checks_out_exact_candidate() -> None:
    for job in CI["jobs"].values():
        checkout = next(
            step for step in job["steps"] if step.get("uses", "").startswith("actions/checkout@")
        )
        assert checkout["with"]["ref"] == (
            "${{ inputs.commit_sha || github.event.pull_request.head.sha || github.sha }}"
        )


def test_full_coverage_sentinel_waits_for_every_lane() -> None:
    job = CI["jobs"]["full-coverage"]
    assert "always()" in job["if"]
    assert "needs.select-tier.outputs.full == 'true'" in job["if"]
    assert set(job["needs"]) >= {
        "select-tier",
        "lint-no-macos-runners",
        "rust",
        "linux",
        "native-wheel",
        "darwin-cross-wheel",
        "darwin-hosted-smoke",
    }
    assert any("ci/verify_full_coverage.py" in step.get("run", "") for step in job["steps"])
    timing = CI["jobs"]["ci-queue-timing"]
    assert timing["if"] == "always()"
    assert "full-coverage" in timing["needs"]
