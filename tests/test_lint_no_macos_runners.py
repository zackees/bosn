"""Regression coverage for the #252 hosted-macOS-runner policy."""

from __future__ import annotations

import sys
from pathlib import Path

import yaml

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "ci"))
import lint_no_macos_runners as lint  # noqa: E402


def test_current_workflows_allow_only_full_and_release_macos_runner() -> None:
    root = Path(__file__).resolve().parents[1]
    assert lint.check(root / ".github/workflows") == 0


def test_full_ci_hosted_job_requires_explicit_full_mode() -> None:
    root = Path(__file__).resolve().parents[1]
    ci = yaml.safe_load((root / ".github/workflows/ci.yml").read_text())
    assert "labeled" in ci[True]["pull_request"]["types"]
    assert ci["jobs"]["lint-no-macos-runners"]["name"] == "CI policy (no hosted macOS runners)"
    job = ci["jobs"]["darwin-hosted-smoke"]
    assert "needs.select-tier.outputs.full == 'true'" == job["if"]
    assert "select-tier" in job["needs"]
    assert "darwin-cross-wheel" in job["needs"]


def test_release_hosted_job_gates_wheel_collection() -> None:
    root = Path(__file__).resolve().parents[1]
    release = yaml.safe_load((root / ".github/workflows/auto-release.yml").read_text())
    assert "darwin-hosted-smoke" in release["jobs"]["collect"]["needs"]


def test_lint_rejects_runs_on_and_matrix_macos_labels() -> None:
    document = yaml.safe_load(
        """jobs:
  direct:
    runs-on: macos-latest
  indirect:
    runs-on: ${{ matrix.runner }}
    strategy:
      matrix:
        runner: [ubuntu-latest, macos-15-intel]
"""
    )
    assert list(lint.offenders(document)) == [
        ("jobs.direct.runs-on", "macos-latest"),
        ("jobs.indirect.strategy.matrix", "macos-15-intel"),
    ]


def test_lint_ignores_macos_prose_and_conditions() -> None:
    document = yaml.safe_load(
        """jobs:
  cross:
    name: Darwin wheel (not macos-latest)
    runs-on: ubuntu-latest
    if: runner.os != 'macOS'
"""
    )
    assert list(lint.offenders(document)) == []


def test_lint_rejects_hosted_runner_outside_full_or_release(tmp_path: Path) -> None:
    workflow = tmp_path / "ci.yml"
    workflow.write_text("jobs:\n  ordinary:\n    runs-on: macos-15\n")
    assert lint.check(workflow) == 1
    workflow.write_text(
        "jobs:\n  darwin-hosted-smoke:\n    if: true\n"
        "    needs: darwin-cross-wheel\n    runs-on: macos-15\n"
    )
    assert lint.check(workflow) == 1
