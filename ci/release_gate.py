#!/usr/bin/env python3
"""The pretag release gate: nothing tags or publishes before full CI passed on the SHA.

A release is requested by dispatching ``auto-release.yml`` with an exact candidate
SHA on ``main`` and the ID of a successful full CI run of that same SHA (the fleet
pattern of zackees/soldr#3343, zackees/soldr#3345). An optional release-request
issue names the same SHA and tag, and the release posts its result there.

``resolve`` runs in the release workflow's guard job, on a checkout of the
candidate, and refuses unless:

  - the candidate is a full 40-character lowercase SHA, is what is checked out,
    and is reachable from ``origin/main``;
  - the release tag ``v<[workspace.package].version>`` does not exist yet, or
    already names the candidate (a resumed attempt), never another commit;
  - the CI run is an explicit ``workflow_dispatch`` of ``ci.yml`` from ``main``
    whose run name is ``CI full <candidate>`` (ci.yml's ``run-name``), completed
    successfully, with every full-tier cell (``verify_full_coverage.REQUIRED``)
    and the ``Full CI coverage`` job itself successful;
  - a named issue is open and its body carries ``candidate_sha: <candidate>``
    and ``tag: <tag>`` lines.

Only then does it write ``release=true``, ``tag``, ``sha`` and ``dry_run`` to
``$GITHUB_OUTPUT``. The tag itself is created later by the publishing jobs,
which all depend on this one.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import tempfile
import urllib.request
from collections.abc import Sequence
from dataclasses import dataclass
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from verify_full_coverage import REQUIRED
from verify_release import release_version

SHA = re.compile(r"^[0-9a-f]{40}$")
CI_WORKFLOW = ".github/workflows/ci.yml"
COVERAGE_JOB = "Full CI coverage (exact candidate SHA)"
# A release-request issue's directive lines: `candidate_sha: <SHA>`, `tag: vX.Y.Z`.
DIRECTIVE = re.compile(r"^\s*(candidate_sha|tag)\s*:\s*`?([^`\s]+)`?\s*$", re.M)


class GateError(RuntimeError):
    """The candidate has no acceptable release request or full-CI proof."""


@dataclass(frozen=True)
class CiJob:
    job_id: int
    name: str
    status: str
    conclusion: str


@dataclass(frozen=True)
class CiRun:
    run_id: int
    event: str
    path: str
    head_branch: str
    display_title: str
    status: str
    conclusion: str
    html_url: str
    jobs: tuple[CiJob, ...]


@dataclass(frozen=True)
class ReleaseIssue:
    number: int
    state: str
    body: str


@dataclass(frozen=True)
class Decision:
    tag: str
    sha: str
    dry_run: bool


def _text(document: dict[str, object], key: str) -> str:
    value = document.get(key)
    return value if isinstance(value, str) else ""


def _int(document: dict[str, object], key: str) -> int:
    value = document.get(key)
    return value if isinstance(value, int) else 0


def parse_run(run: dict[str, object], jobs: Sequence[dict[str, object]]) -> CiRun:
    return CiRun(
        run_id=_int(run, "id"),
        event=_text(run, "event"),
        path=_text(run, "path").split("@", 1)[0],
        head_branch=_text(run, "head_branch"),
        display_title=_text(run, "display_title"),
        status=_text(run, "status"),
        conclusion=_text(run, "conclusion"),
        html_url=_text(run, "html_url"),
        jobs=tuple(
            CiJob(
                job_id=_int(job, "id"),
                name=_text(job, "name"),
                status=_text(job, "status"),
                conclusion=_text(job, "conclusion"),
            )
            for job in jobs
        ),
    )


def parse_issue(issue: dict[str, object]) -> ReleaseIssue:
    return ReleaseIssue(
        number=_int(issue, "number"), state=_text(issue, "state"), body=_text(issue, "body")
    )


def ci_title(sha: str) -> str:
    """The run name ci.yml gives a full-tier dispatch of ``sha``."""
    return f"CI full {sha}"


def check_ci_run(run: CiRun, sha: str) -> None:
    """The run is a green, explicit full CI of exactly ``sha``, every cell included."""
    if run.event != "workflow_dispatch" or run.path != CI_WORKFLOW:
        raise GateError(f"CI run {run.run_id} is not an explicit dispatch of {CI_WORKFLOW}")
    if run.head_branch != "main":
        raise GateError(f"CI run {run.run_id} ran ci.yml from {run.head_branch!r}, not main")
    # A dispatch checks out inputs.commit_sha, not the branch tip, so the run's
    # head_sha is main's tip; the run name is what records the candidate.
    if run.display_title != ci_title(sha):
        raise GateError(
            f"CI run {run.run_id} is {run.display_title!r}, not a full run of {sha} "
            f"(dispatch: gh workflow run ci.yml -f tier=full -f commit_sha={sha})"
        )
    if run.status != "completed" or run.conclusion != "success":
        raise GateError(f"CI run {run.run_id} is {run.status}/{run.conclusion or 'pending'}")
    latest: dict[str, CiJob] = {}
    for job in run.jobs:
        if job.name not in latest or job.job_id > latest[job.name].job_id:
            latest[job.name] = job
    for name in (*REQUIRED, COVERAGE_JOB):
        job = latest.get(name)
        if job is None or job.status != "completed" or job.conclusion != "success":
            state = "missing" if job is None else job.conclusion or job.status
            raise GateError(f"CI run {run.run_id}: required job {name!r} is {state}")


def check_issue(issue: ReleaseIssue, sha: str, tag: str) -> None:
    """The release-request issue is open and names exactly this candidate and tag."""
    if issue.state != "open":
        raise GateError(f"release request #{issue.number} is {issue.state}, not open")
    directive = {match.group(1): match.group(2) for match in DIRECTIVE.finditer(issue.body)}
    if directive.get("candidate_sha") != sha or directive.get("tag") != tag:
        raise GateError(
            f"release request #{issue.number} names candidate_sha="
            f"{directive.get('candidate_sha')} tag={directive.get('tag')}, "
            f"not candidate_sha={sha} tag={tag}"
        )


@dataclass(frozen=True)
class GitResult:
    returncode: int
    stdout: str


def _git(*args: str) -> GitResult:
    # Output goes to a file, never a pipe the parent waits on (zackees/ci.yml PY-003).
    with tempfile.TemporaryFile("w+", encoding="utf-8") as out:
        code = subprocess.run(["git", *args], stdout=out, check=False).returncode
        out.seek(0)
        return GitResult(returncode=code, stdout=out.read())


def check_candidate(sha: str) -> None:
    """``sha`` is well-formed, checked out, and reachable from origin/main."""
    if not SHA.fullmatch(sha):
        raise GateError(f"candidate_sha {sha!r} is not a full 40-character lowercase SHA")
    head = _git("rev-parse", "HEAD").stdout.strip()
    if head != sha:
        raise GateError(f"checkout is {head}, not the candidate {sha}")
    if _git("merge-base", "--is-ancestor", sha, "origin/main").returncode != 0:
        raise GateError(f"candidate {sha} is not reachable from origin/main")


def check_tag(tag: str, sha: str) -> None:
    """The tag is new, or already names the candidate (resuming an attempt)."""
    tagged = _git("rev-parse", "-q", "--verify", f"refs/tags/{tag}^{{commit}}")
    if tagged.returncode == 0 and tagged.stdout.strip() != sha:
        raise GateError(f"tag {tag} already names {tagged.stdout.strip()}, not {sha}")


def _get(url: str, token: str) -> object:
    request = urllib.request.Request(
        url,
        headers={"Authorization": f"Bearer {token}", "Accept": "application/vnd.github+json"},
    )
    with urllib.request.urlopen(request, timeout=30) as response:
        return json.load(response)


def _object(value: object) -> dict[str, object]:
    if not isinstance(value, dict):
        raise GateError("GitHub API returned a non-object document")
    return {str(key): item for key, item in value.items()}


def fetch_run(repo: str, run_id: int, token: str) -> CiRun:
    base = f"https://api.github.com/repos/{repo}/actions/runs/{run_id}"
    run = _object(_get(base, token))
    jobs: list[dict[str, object]] = []
    page = 1
    while True:
        batch = _object(_get(f"{base}/jobs?filter=all&per_page=100&page={page}", token)).get("jobs")
        items = [_object(job) for job in batch] if isinstance(batch, list) else []
        jobs.extend(items)
        if len(items) < 100:
            return parse_run(run, jobs)
        page += 1


def fetch_issue(repo: str, number: int, token: str) -> ReleaseIssue:
    return parse_issue(_object(_get(f"https://api.github.com/repos/{repo}/issues/{number}", token)))


def resolve(args: argparse.Namespace) -> Decision:
    sha = args.candidate_sha.strip()
    check_candidate(sha)
    tag = f"v{release_version(Path('.'))}"
    check_tag(tag, sha)
    token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN") or ""
    if not args.ci_run_id.strip().isdigit():
        raise GateError(f"full_ci_run_id {args.ci_run_id!r} is not a run ID")
    check_ci_run(fetch_run(args.repo, int(args.ci_run_id), token), sha)
    if args.issue.strip():
        if not args.issue.strip().isdigit():
            raise GateError(f"issue {args.issue!r} is not an issue number")
        check_issue(fetch_issue(args.repo, int(args.issue), token), sha, tag)
    return Decision(tag=tag, sha=sha, dry_run=args.dry_run != "false")


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    cmd = sub.add_parser("resolve")
    cmd.add_argument("--candidate-sha", required=True)
    cmd.add_argument("--ci-run-id", required=True)
    cmd.add_argument("--issue", default="")
    cmd.add_argument("--dry-run", default="true", choices=("true", "false"))
    cmd.add_argument("--repo", default=os.environ.get("GITHUB_REPOSITORY", "zackees/bosn"))
    cmd.add_argument("--output", type=Path)
    cmd.add_argument("--summary", type=Path)
    args = parser.parse_args(argv)
    try:
        decision = resolve(args)
    except GateError as error:
        print(f"::error::release gate: {error}", file=sys.stderr)
        return 1
    line = (
        f"Releasing {decision.tag} from {decision.sha}: full CI run {args.ci_run_id} is green "
        f"on that exact SHA (dry run: {str(decision.dry_run).lower()})"
    )
    print(line)
    if args.output:
        with args.output.open("a", encoding="utf-8") as output:
            output.write(
                f"release=true\ntag={decision.tag}\nsha={decision.sha}\n"
                f"dry_run={str(decision.dry_run).lower()}\n"
            )
    if args.summary:
        with args.summary.open("a", encoding="utf-8") as summary:
            summary.write(line + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
