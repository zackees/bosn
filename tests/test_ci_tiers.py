"""The expensive CI lanes are selected only by explicit tier requests."""

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
    for name, tier in {
        "linux": "test",
        "native-wheel": "full",
        "darwin-cross-wheel": "full",
        "darwin-hosted-smoke": "full",
    }.items():
        assert "select-tier" in jobs[name]["needs"]
        assert f"needs.select-tier.outputs.{tier} == 'true'" in jobs[name]["if"]


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
        "select-tier", "lint-no-macos-runners", "rust", "linux",
        "native-wheel", "darwin-cross-wheel", "darwin-hosted-smoke",
    }
    assert any("ci/verify_full_coverage.py" in step.get("run", "") for step in job["steps"])
    timing = CI["jobs"]["ci-queue-timing"]
    assert timing["if"] == "always()"
    assert "full-coverage" in timing["needs"]
