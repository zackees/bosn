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
        # Fail closed when the selector fails, but a superseded (cancelled)
        # run must never leave FAILURE required checks behind (#373).
        assert job["if"] == "${{ !cancelled() }}", "required checks must run after selector failure"
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
        assert job["if"] == "${{ !cancelled() }}", "matrix must expand even if selector fails"
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
    # It always runs, except that an attested, trusted PR head may skip it
    # (GATE-008/010, ci-attestations.yml); a skipped required check passes.
    assert jobs["rust"]["if"] == "needs.verify.outputs.skip_rust != 'true'"


def test_native_backend_and_compiler_fixtures_have_provisioned_tools() -> None:
    release = yaml.safe_load((ROOT / ".github/workflows/auto-release.yml").read_text())
    wheel_jobs = [CI["jobs"]["native-wheel"]]
    wheel_jobs.extend(
        job
        for job in release["jobs"].values()
        if any(step.get("run") == "uv build --wheel --out-dir dist" for step in job["steps"])
        and "os" in job.get("strategy", {}).get("matrix", {})
    )
    assert len(wheel_jobs) == 2
    for job in [CI["jobs"]["rust"], *wheel_jobs]:
        steps = job["steps"]
        soldr = next(
            index
            for index, step in enumerate(steps)
            if step.get("uses", "").startswith("zackees/setup-soldr@")
        )
        uv = next(
            index
            for index, step in enumerate(steps)
            if step.get("uses", "").startswith("astral-sh/setup-uv@")
        )
        consumers = [
            index
            for index, step in enumerate(steps)
            if "soldr cargo" in step.get("run", "") or "uv build --wheel" in step.get("run", "")
        ]
        assert consumers and max(soldr, uv) < min(consumers)
        assert steps[soldr]["with"]["version"] == "0.9.27"
        assert steps[soldr]["with"]["toolchain"] == "1.95.0"


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
    # A reusable-workflow call (ci-pre) has no steps of its own.
    for job in (job for job in CI["jobs"].values() if "uses" not in job):
        checkout = next(
            step for step in job["steps"] if step.get("uses", "").startswith("actions/checkout@")
        )
        assert checkout["with"]["ref"] == (
            "${{ inputs.commit_sha || github.event.pull_request.head.sha || github.sha }}"
        )


def test_full_coverage_sentinel_waits_for_every_lane() -> None:
    job = CI["jobs"]["full-coverage"]
    assert "!cancelled()" in job["if"]
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
    assert timing["if"] == "${{ !cancelled() }}"
    assert "full-coverage" in timing["needs"]


def test_only_run_introspection_jobs_are_remote_only() -> None:
    # GATE-012 (#400): a job that asks the GitHub API about its own run
    # (github.run_id) cannot run under act, whose run ID GitHub answers with
    # 404. It declares CI_REMOTE_ONLY, so a local `bosn ci run` reports it
    # remote_only with the reason instead of failing. No other job may: that
    # would silently drop local coverage.
    def asks_github_about_this_run(job: dict) -> bool:
        return any("github.run_id" in step.get("run", "") for step in job.get("steps", []))

    remote_only = {
        name for name, job in CI["jobs"].items() if "CI_REMOTE_ONLY" in job.get("env", {})
    }
    assert remote_only == {
        name for name, job in CI["jobs"].items() if asks_github_about_this_run(job)
    }
    assert remote_only == {"full-coverage", "ci-queue-timing"}
    for name in remote_only:
        assert CI["jobs"][name]["env"]["CI_REMOTE_ONLY"].strip(), name


def test_remote_only_declaration_leaves_the_required_check_unchanged_on_github() -> None:
    # Branch protection requires this check by name; on GitHub the marker is
    # an unused variable, so the job still verifies every cell and fails
    # closed exactly as before.
    job = CI["jobs"]["full-coverage"]
    assert job["name"] == "Full CI coverage (exact candidate SHA)"
    assert set(job["env"]) == {"CI_REMOTE_ONLY"}
    verify = next(step for step in job["steps"] if "verify_full_coverage.py" in step.get("run", ""))
    assert verify["env"]["GITHUB_TOKEN"] == "${{ github.token }}"
    assert "continue-on-error" not in job and "continue-on-error" not in verify


def test_no_job_runs_unconditionally_under_a_cancelled_run() -> None:
    # A bare always() job still starts when its run is cancelled and then
    # fails, leaving a FAILURE required check on the head that blocks the
    # merge even though the live run is green (#373).
    for name, job in CI["jobs"].items():
        assert "always()" not in str(job.get("if", "")), name
