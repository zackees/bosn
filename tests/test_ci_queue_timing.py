"""Queue/execution timing report and SLO check for CI runs (issue #257).

The fixtures are trimmed GitHub Actions API responses: run 34865549628 is the
before-change trace whose final matrix job started 26 minutes after workflow
creation, and run 34876130390 is a main-branch run after the fleet-wide
concurrency policy landed.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "ci"))
import ci_queue_timing as timing  # noqa: E402

FIXTURES = Path(__file__).resolve().parent / "fixtures" / "ci_queue_timing"
BEFORE = FIXTURES / "run_34865549628.json"
AFTER = FIXTURES / "run_34876130390.json"


def load(path: Path) -> timing.Report:
    document = json.loads(path.read_text())
    return timing.analyze(document["run"], document["jobs"])


def by_name(report: timing.Report, prefix: str) -> timing.JobTiming:
    matches = [job for job in report.jobs if job.name.startswith(prefix)]
    assert len(matches) == 1, [job.name for job in report.jobs]
    return matches[0]


def test_before_change_trace_records_the_26_minute_final_start() -> None:
    report = load(BEFORE)
    assert report.run_id == 34865549628
    assert len(report.jobs) == 7
    last = max(report.jobs, key=lambda job: job.queue_minutes or 0)
    assert last.name.startswith("Darwin wheel (x86_64-apple-darwin")
    assert last.queue_minutes is not None and 26.0 < last.queue_minutes < 27.0
    # Execution time is reported separately from queue time, so the Windows
    # wheel's own build cost is never confused with scheduler delay.
    windows = by_name(report, "Native wheel (windows-latest)")
    assert windows.queue_minutes is not None and 6.0 < windows.queue_minutes < 7.0
    assert windows.execution_minutes is not None and 12.5 < windows.execution_minutes < 13.5


def test_before_change_trace_breaches_gate_and_matrix_slo() -> None:
    report = load(BEFORE)
    assert report.slo == timing.DEFAULT_SLO
    breached = {breach.job.name.split(" (")[0] for breach in report.breaches}
    # Both gates started long after the 5-minute gate SLO.
    assert by_name(report, "CI policy").is_gate
    assert by_name(report, "Rust workspace").is_gate
    assert "CI policy" in breached
    assert "Rust workspace" in breached
    # Matrix lanes that started inside 15 minutes are not breaches; the rest are.
    assert not any(b.job.name.startswith("Native wheel (windows") for b in report.breaches)
    assert not any(b.job.name.startswith("Darwin wheel (aarch64") for b in report.breaches)
    assert any(b.job.name.startswith("Darwin wheel (x86_64") for b in report.breaches)
    assert any(b.job.name.startswith("Linux (") for b in report.breaches)
    assert report.ok is False


def test_post_policy_main_run_meets_slo_with_seconds_of_queue() -> None:
    report = load(AFTER)
    assert report.ok is True
    assert report.breaches == []
    assert all(job.queue_minutes is not None and job.queue_minutes < 0.2 for job in report.jobs)
    assert by_name(report, "Native wheel (windows-latest)").execution_minutes is not None


def test_intentionally_skipped_tier_is_not_a_queue_breach() -> None:
    run = {"id": 1, "created_at": "2026-09-23T00:00:00Z", "head_branch": "main"}
    jobs = [{
        "name": "Native wheel (windows-latest)",
        "created_at": "2026-09-23T00:00:00Z",
        "started_at": None,
        "completed_at": "2026-09-23T00:00:02Z",
        "conclusion": "skipped",
    }]
    report = timing.analyze(run, jobs, now=timing.parse_timestamp("2026-09-23T01:00:00Z"))
    assert report.ok
    assert "skipped" in timing.render_markdown(report)


def test_unstarted_job_is_a_named_breach_not_a_silent_gap() -> None:
    document = json.loads(AFTER.read_text())
    stuck = dict(document["jobs"][0])
    stuck.update(
        name="Native wheel (windows-latest)",
        status="queued",
        conclusion=None,
        started_at=None,
        completed_at=None,
    )
    report = timing.analyze(
        document["run"], [stuck], now=timing.parse_timestamp("2026-09-14T18:00:01Z")
    )
    assert report.ok is False
    (breach,) = report.breaches
    assert breach.job.name == "Native wheel (windows-latest)"
    assert breach.job.started is None
    assert breach.kind == "never-started"
    assert breach.waited_minutes == pytest.approx(20.0, abs=0.1)


def test_markdown_summary_names_delayed_jobs_and_escalation_owner() -> None:
    text = timing.render_markdown(load(BEFORE))
    assert "| Job |" in text and "Queued" in text and "Executed" in text
    assert "Darwin wheel (x86_64-apple-darwin, Linux-hosted Soldr)" in text
    assert "26." in text
    assert "SLO breach" in text
    assert timing.ESCALATION_OWNER in text
    assert timing.ESCALATION_DOC in text
    ok_text = timing.render_markdown(load(AFTER))
    assert "SLO breach" not in ok_text
    assert "within SLO" in ok_text


def test_annotations_are_warnings_that_name_each_delayed_job() -> None:
    lines = timing.render_annotations(load(BEFORE))
    assert lines and all(line.startswith("::warning::") for line in lines)
    assert any("CI policy" in line and "17." in line for line in lines)
    assert timing.render_annotations(load(AFTER)) == []


def test_cli_writes_summary_and_only_fails_when_enforcing(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    summary = tmp_path / "summary.md"
    assert timing.main(["report", "--json", str(BEFORE), "--summary-file", str(summary)]) == 0
    assert "SLO breach" in summary.read_text()
    assert "::warning::" in capsys.readouterr().out
    assert timing.main(["report", "--json", str(BEFORE), "--enforce"]) == 1
    assert timing.main(["report", "--json", str(AFTER), "--enforce"]) == 0


def test_probe_table_reports_queue_and_execution_separately() -> None:
    reports = [load(AFTER), load(BEFORE)]
    text = timing.render_probe(reports)
    assert "34876130390" in text and "34865549628" in text
    assert "max queue" in text.lower() and "max exec" in text.lower()
    assert "breach" in text.lower()
