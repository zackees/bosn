#!/usr/bin/env python3
"""Report GitHub Actions queue and execution timing for Bosn CI (issue #257).

The hosted runner pool is shared across every repository under the account, so
a CI run can sit behind other repositories' work for longer than its own jobs
take to execute.  That delay is invisible from inside a run: the workflow just
looks stuck.  This script makes it visible and checks it against the queue SLO
documented in docs/ci-queue-slo.md.

    # Inside a workflow run, as the last job (writes the step summary):
    python ci/ci_queue_timing.py report --run-id "$GITHUB_RUN_ID" \
        --summary-file "$GITHUB_STEP_SUMMARY"

    # Offline, from a saved API document (used by the tests):
    python ci/ci_queue_timing.py report --json tests/fixtures/ci_queue_timing/run_34865549628.json

    # Operational probe over recent main-branch runs:
    python ci/ci_queue_timing.py probe --branch main --count 3

Queue time is measured per job as ``started_at - created_at``; execution time
is ``completed_at - started_at``.  They are always reported separately so a
slow build (for example the Windows wheel) is never mistaken for scheduler
delay, and vice versa.  A breach never fails the workflow unless ``--enforce``
is passed: capacity is an account-level condition, and failing an otherwise
green run would only hide the result behind the same queue.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import urllib.request
from collections.abc import Iterable, Sequence
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, cast

REPO = "zackees/bosn"
ESCALATION_OWNER = "@zackees"
ESCALATION_DOC = "docs/ci-queue-slo.md"
# Jobs that gate everything else and must not wait behind long wheel builds.
GATE_JOB_PREFIXES = ("CI policy", "Rust workspace")
# The report job itself is excluded: it necessarily starts after every lane.
SELF_JOB_PREFIX = "CI queue timing"


@dataclass(frozen=True)
class Slo:
    gate_start_minutes: float
    matrix_start_minutes: float


DEFAULT_SLO = Slo(gate_start_minutes=5.0, matrix_start_minutes=15.0)


@dataclass(frozen=True)
class JobTiming:
    name: str
    labels: tuple[str, ...]
    created: datetime
    started: datetime | None
    completed: datetime | None
    conclusion: str | None
    is_gate: bool

    @property
    def queue_minutes(self) -> float | None:
        if self.started is None:
            return None
        return (self.started - self.created).total_seconds() / 60

    @property
    def execution_minutes(self) -> float | None:
        if self.started is None or self.completed is None:
            return None
        return (self.completed - self.started).total_seconds() / 60


@dataclass(frozen=True)
class Breach:
    job: JobTiming
    kind: str  # "late-start" or "never-started"
    waited_minutes: float
    limit_minutes: float


@dataclass
class Report:
    run_id: int
    run_url: str
    run_created: datetime
    branch: str
    conclusion: str | None
    slo: Slo
    jobs: list[JobTiming] = field(default_factory=list)
    breaches: list[Breach] = field(default_factory=list)

    @property
    def ok(self) -> bool:
        return not self.breaches

    @property
    def max_queue_minutes(self) -> float:
        return max((job.queue_minutes or 0.0 for job in self.jobs), default=0.0)

    @property
    def max_execution_minutes(self) -> float:
        return max((job.execution_minutes or 0.0 for job in self.jobs), default=0.0)


def parse_timestamp(value: str) -> datetime:
    return datetime.fromisoformat(value.replace("Z", "+00:00"))


def _optional_timestamp(value: object) -> datetime | None:
    return parse_timestamp(value) if isinstance(value, str) and value else None


def limit_for(job: JobTiming, slo: Slo) -> float:
    return slo.gate_start_minutes if job.is_gate else slo.matrix_start_minutes


def analyze(
    run: dict[str, Any],
    jobs: Iterable[dict[str, Any]],
    slo: Slo = DEFAULT_SLO,
    now: datetime | None = None,
) -> Report:
    """Build a timing report from an Actions run document and its jobs list."""

    report = Report(
        run_id=int(run["id"]),
        run_url=str(run.get("html_url") or f"https://github.com/{REPO}/actions/runs/{run['id']}"),
        run_created=parse_timestamp(run["created_at"]),
        branch=str(run.get("head_branch") or ""),
        conclusion=cast("str | None", run.get("conclusion")),
        slo=slo,
    )
    clock = now or datetime.now(timezone.utc)
    for raw in jobs:
        name = str(raw["name"])
        if name.startswith(SELF_JOB_PREFIX):
            continue
        job = JobTiming(
            name=name,
            labels=tuple(str(label) for label in raw.get("labels") or ()),
            created=parse_timestamp(str(raw.get("created_at") or run["created_at"])),
            started=_optional_timestamp(raw.get("started_at")),
            completed=_optional_timestamp(raw.get("completed_at")),
            conclusion=cast("str | None", raw.get("conclusion")),
            is_gate=name.startswith(GATE_JOB_PREFIXES),
        )
        report.jobs.append(job)
        limit = limit_for(job, slo)
        if job.started is None:
            waited = (clock - job.created).total_seconds() / 60
            if job.conclusion in (None, "cancelled") and waited > limit:
                report.breaches.append(Breach(job, "never-started", waited, limit))
            elif job.conclusion is None:
                report.breaches.append(Breach(job, "never-started", waited, limit))
        elif (queued := job.queue_minutes) is not None and queued > limit:
            report.breaches.append(Breach(job, "late-start", queued, limit))
    report.jobs.sort(key=lambda job: (job.started or clock, job.name))
    return report


def _minutes(value: float | None) -> str:
    return "-" if value is None else f"{value:.1f} min"


def render_markdown(report: Report) -> str:
    lines = [
        f"### CI queue timing — run [{report.run_id}]({report.run_url})",
        "",
        f"Workflow created {report.run_created:%Y-%m-%d %H:%M:%S} UTC on `{report.branch}`. "
        "Queued = job start minus job creation (scheduler delay); "
        "Executed = job completion minus job start (our own cost). "
        f"SLO: gates start within {report.slo.gate_start_minutes:g} min, "
        f"every lane within {report.slo.matrix_start_minutes:g} min.",
        "",
        "| Job | Runner | Queued | Executed | Result |",
        "|---|---|---:|---:|---|",
    ]
    for job in report.jobs:
        kind = " (gate)" if job.is_gate else ""
        runner = ", ".join(job.labels) or "-"
        lines.append(
            f"| {job.name}{kind} | {runner} | {_minutes(job.queue_minutes)} | "
            f"{_minutes(job.execution_minutes)} | {job.conclusion or 'not finished'} |"
        )
    lines.append("")
    if report.ok:
        lines.append(
            f"**All lanes started within SLO** (max queue {report.max_queue_minutes:.1f} min, "
            f"max execution {report.max_execution_minutes:.1f} min)."
        )
    else:
        lines.append(f"**SLO breach — {len(report.breaches)} job(s) waited longer than allowed:**")
        lines.append("")
        for breach in report.breaches:
            what = "never started" if breach.kind == "never-started" else "started"
            lines.append(
                f"- `{breach.job.name}` on `{', '.join(breach.job.labels) or '?'}` {what} after "
                f"{breach.waited_minutes:.1f} min (limit {breach.limit_minutes:g} min)"
            )
        lines.append("")
        lines.append(
            "This is scheduler delay in the shared account-wide runner pool, not a Bosn "
            f"failure. Escalation owner: {ESCALATION_OWNER}. Follow {ESCALATION_DOC}: check the "
            "global queue for superseded or orphaned runs in other repositories, cancel them, "
            "and re-run this workflow only if a lane never started."
        )
    return "\n".join(lines) + "\n"


def render_annotations(report: Report) -> list[str]:
    return [
        f"::warning::CI queue SLO breach: {breach.job.name} "
        f"{'never started' if breach.kind == 'never-started' else 'started'} after "
        f"{breach.waited_minutes:.1f} min (limit {breach.limit_minutes:g} min); "
        f"see {ESCALATION_DOC} ({ESCALATION_OWNER})"
        for breach in report.breaches
    ]


def render_probe(reports: Sequence[Report]) -> str:
    lines = [
        "| Run | Branch | Created (UTC) | Result | Max queue | Max exec | Breaches |",
        "|---|---|---|---|---:|---:|---|",
    ]
    for report in reports:
        names = ", ".join(breach.job.name for breach in report.breaches) or "none"
        lines.append(
            f"| [{report.run_id}]({report.run_url}) | {report.branch} | "
            f"{report.run_created:%Y-%m-%d %H:%M} | {report.conclusion or 'running'} | "
            f"{report.max_queue_minutes:.1f} min | {report.max_execution_minutes:.1f} min | "
            f"{names} |"
        )
    return "\n".join(lines) + "\n"


def _api(path: str, token: str | None) -> Any:
    request = urllib.request.Request(
        f"https://api.github.com/{path}",
        headers={
            "Accept": "application/vnd.github+json",
            "X-GitHub-Api-Version": "2022-11-28",
            **({"Authorization": f"Bearer {token}"} if token else {}),
        },
    )
    with urllib.request.urlopen(request, timeout=30) as response:  # noqa: S310 - fixed https host
        return json.load(response)


def fetch_run(
    repo: str, run_id: int, token: str | None
) -> tuple[dict[str, Any], list[dict[str, Any]]]:
    run = _api(f"repos/{repo}/actions/runs/{run_id}", token)
    jobs: list[dict[str, Any]] = []
    page = 1
    while True:
        document = _api(f"repos/{repo}/actions/runs/{run_id}/jobs?per_page=100&page={page}", token)
        batch = cast("list[dict[str, Any]]", document.get("jobs", []))
        jobs.extend(batch)
        if len(batch) < 100:
            return run, jobs
        page += 1


def load_document(path: Path) -> tuple[dict[str, Any], list[dict[str, Any]]]:
    document = json.loads(path.read_text())
    return document["run"], document["jobs"]


def _token() -> str | None:
    return os.environ.get("GITHUB_TOKEN") or os.environ.get("GH_TOKEN")


def cmd_report(args: argparse.Namespace) -> int:
    if args.json:
        run, jobs = load_document(Path(args.json))
    else:
        run, jobs = fetch_run(args.repo, int(args.run_id), _token())
    report = analyze(run, jobs)
    markdown = render_markdown(report)
    if args.summary_file:
        with Path(args.summary_file).open("a") as handle:
            handle.write(markdown)
    print(markdown)
    for line in render_annotations(report):
        print(line)
    if args.enforce and not report.ok:
        return 1
    return 0


def cmd_probe(args: argparse.Namespace) -> int:
    token = _token()
    listing = _api(
        f"repos/{args.repo}/actions/runs?branch={args.branch}&per_page={args.count}"
        f"&event={args.event}",
        token,
    )
    reports: list[Report] = []
    for run in cast("list[dict[str, Any]]", listing.get("workflow_runs", [])):
        if run.get("name") != args.workflow:
            continue
        full_run, jobs = fetch_run(args.repo, int(run["id"]), token)
        reports.append(analyze(full_run, jobs))
    print(render_probe(reports))
    return 0 if reports and all(report.ok for report in reports) else 1


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("--repo", default=REPO)
    commands = parser.add_subparsers(dest="command", required=True)

    report = commands.add_parser("report", help="report one run and check it against the SLO")
    source = report.add_mutually_exclusive_group(required=True)
    source.add_argument("--run-id", help="Actions run id to fetch")
    source.add_argument("--json", help="saved {run, jobs} document instead of the API")
    report.add_argument(
        "--summary-file", help="append the markdown report here ($GITHUB_STEP_SUMMARY)"
    )
    report.add_argument("--enforce", action="store_true", help="exit 1 on an SLO breach")
    report.set_defaults(func=cmd_report)

    probe = commands.add_parser("probe", help="report the most recent runs of a branch")
    probe.add_argument("--branch", default="main")
    probe.add_argument("--count", type=int, default=3)
    probe.add_argument("--event", default="push")
    probe.add_argument("--workflow", default="CI")
    probe.set_defaults(func=cmd_probe)

    args = parser.parse_args(argv)
    return int(args.func(args))


if __name__ == "__main__":
    sys.exit(main())
