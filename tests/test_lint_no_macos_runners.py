"""Regression coverage for the #252 hosted-macOS-runner policy."""

from __future__ import annotations

import sys
from pathlib import Path

import yaml

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "ci"))
import lint_no_macos_runners as lint  # noqa: E402


def test_current_workflows_have_no_native_macos_runner() -> None:
    root = Path(__file__).resolve().parents[1]
    assert lint.check(root / ".github/workflows") == 0


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
