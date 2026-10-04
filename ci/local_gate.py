#!/usr/bin/env python3
"""bosn's local gate (zackees/ci.yml#166, GATE-001..010).

One command that runs the checks the remote CI runs, so a passing local gate
can attest the head and let the remote jobs it already ran be skipped
(GATE-008/010).

The lane that mirrors a remote job does so by *replaying that job* with
`bosn ci run --job <id>` rather than by re-listing its steps. That is what
keeps the lane from drifting: when a workflow step changes, the replay runs the
new step, and `receipt_error` fails the lane if a required step did not really
execute, or the job ran against another commit, workspace, engine, event, or
workflow.

GATE-005: bosn's test suite starts daemons and touches state roots, so it must
not run against a developer host; the guard refuses any run that is not CI or
the isolated gate image.
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import shutil
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path
from typing import TypeAlias

ROOT = Path(__file__).resolve().parent.parent

JsonValue: TypeAlias = (
    str | int | float | bool | list["JsonValue"] | dict[str, "JsonValue"] | None
)

LANES = ("rust",)
CI_WORKFLOW = ".github/workflows/ci.yml"


@dataclass(frozen=True)
class Captured:
    returncode: int
    stdout: str
    stderr: str


def run_captured(argv: list[str], env: dict[str, str] | None = None) -> Captured:
    """Run `argv` from the repository root with output captured through one
    temporary file, never a pipe (zackees/ci.yml PY-003): a full pipe blocks
    the child, and a daemon that inherits a pipe keeps the caller waiting for an
    EOF that never comes."""

    with tempfile.TemporaryFile() as out:
        proc = subprocess.run(
            argv,
            cwd=ROOT,
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=out,
            stderr=subprocess.STDOUT,
            check=False,
        )
        out.seek(0)
        return Captured(proc.returncode, out.read().decode("utf-8", errors="replace"), "")


# ── the isolated-run receipt ────────────────────────────────────────────────


@dataclass(frozen=True)
class Section:
    name: str
    stage: str
    status: str
    conclusion: str | None


@dataclass(frozen=True)
class Job:
    job_id: str
    status: str
    conclusion: str | None
    sections: tuple[Section, ...]


@dataclass(frozen=True)
class Receipt:
    workspace: str
    sha: str
    state: str
    conclusion: str
    engine: str
    event: str
    workflow: str
    selected_job: str | None
    exit_code: int
    jobs: tuple[Job, ...]
    reason: str | None


# act2 cannot give a called reusable workflow a qualified execution identity, so
# a run whose workflow calls one ends with conclusion="incomplete" and a nonzero
# exit code even though act itself exited 0 and every job completed. Treating
# that as a failure makes the gate permanently un-green, which blocks the
# attestation and therefore every remote skip.
#
# The exemption below is safe because the per-job evidence still has to stand on
# its own: the selected job must have completed successfully AND run every
# required step. A receipt is only exempted when it names exactly this reason --
# any other "incomplete", or a failed job, still fails closed.
REUSABLE_WORKFLOW_LIMITATION = "reusable workflows require qualified execution identity"


def _document(value: JsonValue) -> dict[str, JsonValue]:
    if not isinstance(value, dict):
        raise TypeError("expected a JSON object")
    return value


def _values(value: JsonValue) -> list[JsonValue]:
    if not isinstance(value, list):
        raise TypeError("expected a JSON array")
    return value


def _text(value: JsonValue) -> str:
    if not isinstance(value, str):
        raise TypeError("expected a JSON string")
    return value


def _optional_text(value: JsonValue) -> str | None:
    return None if value is None else _text(value)


def _job(value: JsonValue) -> Job:
    raw = _document(value)
    sections = tuple(
        Section(
            _text(section["name"]),
            _text(section["stage"]),
            _text(section["status"]),
            _optional_text(section["conclusion"]),
        )
        for section in (_document(item) for item in _values(raw["sections"]))
    )
    return Job(
        _text(raw["job_id"]),
        _text(raw["status"]),
        _optional_text(raw["conclusion"]),
        sections,
    )


def parse_receipt(output: str) -> Receipt:
    documents = [_document(json.loads(line)) for line in output.splitlines() if line.startswith("{")]
    if len(documents) != 1:
        raise ValueError("expected exactly one Bosn run receipt")
    document = documents[0]
    if document["dirty"] is not None:
        raise ValueError("Bosn executed a dirty snapshot")
    exit_code = document["exit_code"]
    if type(exit_code) is not int:
        raise ValueError("missing terminal exit code")
    jobs: list[Job] = []
    for group_value in _values(_document(document["tree"])["groups"]):
        jobs.extend(_job(item) for item in _values(_document(group_value)["jobs"]))
    return Receipt(
        _text(document["workspace"]),
        _text(document["sha"]),
        _text(document["state"]),
        _text(document["conclusion"]),
        _text(document["engine"]),
        _text(document["event"]),
        _text(document["workflow"]),
        _optional_text(document["job"]),
        exit_code,
        tuple(jobs),
        _optional_text(document.get("reason")),
    )


def _run_finished(receipt: Receipt) -> bool:
    """Whether the receipt may be trusted to carry job evidence."""

    if receipt.state != "done":
        return False
    if receipt.conclusion == "success" and receipt.exit_code == 0:
        return True
    return (
        receipt.conclusion == "incomplete"
        and receipt.reason == REUSABLE_WORKFLOW_LIMITATION
        and receipt.exit_code == 3
    )


def _job_proves_steps(job: Job, required_steps: tuple[str, ...]) -> bool:
    if job.status != "completed" or job.conclusion != "success":
        return False
    completed = {
        section.name
        for section in job.sections
        if section.stage == "Main" and section.status == "completed" and section.conclusion == "success"
    }
    return set(required_steps).issubset(completed)


def receipt_error(
    output: str,
    *,
    workspace: Path,
    head_sha: str,
    workflow: str,
    expected_job: str,
    selected_job: str | None,
    required_steps: tuple[str, ...],
) -> str | None:
    """Fail closed on wrong source, unknown evidence, or unexecuted checks."""

    if not required_steps:
        return "no required Bosn job steps were declared"
    try:
        receipt = parse_receipt(output)
    except (KeyError, ValueError, TypeError) as error:
        return f"invalid Bosn source proof: {error}"
    if not Path(receipt.workspace).is_absolute() or Path(receipt.workspace).resolve() != workspace.resolve():
        return "Bosn executed another workspace"
    if receipt.sha != head_sha:
        return "Bosn executed another commit"
    if not _run_finished(receipt):
        return "Bosn run did not finish successfully"
    if receipt.engine != "act" or receipt.event != "pull_request":
        return "Bosn used another engine or event"
    if receipt.workflow != workflow or receipt.selected_job != selected_job:
        return "Bosn executed another workflow selection"
    jobs = [job for job in receipt.jobs if job.job_id == expected_job]
    if not jobs or not all(_job_proves_steps(job, required_steps) for job in jobs):
        return "Bosn did not execute every required job step successfully"
    return None


def isolation_error() -> str | None:
    """GATE-005/009: only a native Linux x64 Docker daemon can prove these lanes."""

    daemon = run_captured(["docker", "info", "--format", "{{.OSType}} {{.Architecture}}"])
    if daemon.returncode != 0:
        return "cannot determine Docker daemon architecture"
    if daemon.stdout.strip().lower() not in {"linux x86_64", "linux amd64"}:
        return "bosn's lanes require a Linux x64 Docker daemon"
    if platform.machine() not in {"x86_64", "amd64"}:
        return f"bosn's lanes require a Linux x64 host, not {platform.machine()}"
    return None


# ── checks ──────────────────────────────────────────────────────────────────


@dataclass(frozen=True)
class Check:
    name: str
    argv: tuple[str, ...]
    lane: str
    isolated: bool = False
    bosn_workflow: str = ""
    bosn_job: str = ""
    selected_job: str | None = None
    required_steps: tuple[str, ...] = ()


def checks() -> list[Check]:
    """The Rust workspace job, replayed rather than mirrored, so the gate
    cannot drift from ci.yml."""

    return [
        Check(
            "Rust workspace (isolated Bosn Actions)",
            (
                "bosn", "ci", "run", "--workspace", ".", "--workflow", CI_WORKFLOW,
                "--job", "rust", "--trigger", "pr", "--wait", "--json",
            ),
            "rust",
            isolated=True,
            bosn_workflow=CI_WORKFLOW,
            bosn_job="rust",
            selected_job="rust",
            required_steps=(
                "Verify kernal-api boundary and locked resolution",
                "Test Rust workspace",
            ),
        ),
    ]


@dataclass(frozen=True)
class Result:
    check: Check
    code: int
    seconds: float
    output: str


def run_check(check: Check) -> Result:
    head: Captured | None = None
    if check.isolated:
        error = isolation_error()
        if error:
            return Result(check, 1, 0.0, f"local gate: {error}\n")
        head = run_captured(["git", "rev-parse", "HEAD"])
        if head.returncode != 0:
            return Result(check, 1, 0.0, "cannot determine this checkout's HEAD")

    if shutil.which(check.argv[0]) is None:
        return Result(check, 127, 0.0, f"{check.argv[0]}: not found on PATH")

    start = time.monotonic()
    proc = run_captured(list(check.argv))
    result = Result(check, proc.returncode, time.monotonic() - start, proc.stdout)

    if not check.isolated:
        return result
    if head is None or head.returncode != 0:
        return Result(check, 1, result.seconds, result.output + "\nlocal gate: cannot determine HEAD\n")
    error = receipt_error(
        result.output,
        workspace=ROOT,
        head_sha=head.stdout.strip(),
        workflow=check.bosn_workflow,
        expected_job=check.bosn_job,
        selected_job=check.selected_job,
        required_steps=check.required_steps,
    )
    return Result(
        check,
        1 if error else 0,
        result.seconds,
        result.output + (f"\nlocal gate: {error}\n" if error else ""),
    )


def save_log(result: Result) -> Path:
    logs = ROOT / "target" / "local-gate-logs"
    logs.mkdir(parents=True, exist_ok=True)
    safe = "".join(char if char.isalnum() or char in "._-" else "-" for char in result.check.name).strip("-")
    path = logs / f"{safe}.log"
    path.write_text(result.output, encoding="utf-8")
    return path


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--lane", choices=("all", *LANES), default="all", help="one GATE-007 lane")
    parser.add_argument("--list", action="store_true", help="print the checks and exit")
    parser.add_argument("--jobs", type=int, default=min(4, os.cpu_count() or 2))
    args = parser.parse_args(argv)

    selected = [check for check in checks() if args.lane in ("all", check.lane)]
    if args.list:
        for check in selected:
            print(f"{check.lane:8} {check.name}")
        return 0

    start = time.monotonic()
    failed = False
    for check in selected:
        result = run_check(check)
        status = "ok  " if result.code == 0 else "FAIL"
        print(f"{status} {result.seconds:6.1f}s  {result.check.name}", flush=True)
        if result.code != 0:
            failed = True
            print(f"full output: {save_log(result)}", flush=True)
    if failed:
        return 1
    print(f"local gate: {len(selected)}/{len(selected)} passed in {time.monotonic() - start:.0f}s")
    return 0


if __name__ == "__main__":
    sys.exit(main())