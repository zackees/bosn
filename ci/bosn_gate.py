"""Validate Bosn act2 source and executed-step receipts before attesting."""

from __future__ import annotations

import argparse
import json
import os
import platform
import re
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import TypeAlias

JsonValue: TypeAlias = str | int | float | bool | list["JsonValue"] | dict[str, "JsonValue"] | None
ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = ".github/workflows/ci.yml"


@dataclass(frozen=True)
class RequiredJob:
    job_id: str
    steps: tuple[str, ...]


@dataclass(frozen=True)
class Selection:
    job_id: str
    tier: str
    required_jobs: tuple[RequiredJob, ...]


RUST = Selection(
    "rust",
    "minimal",
    (
        RequiredJob("verify", ("Verify the local-gate attestation and decide attested skips",)),
        RequiredJob(
            "rust",
            (
                "Rust format and Clippy",
                "Verify kernal-api boundary and locked resolution",
                "Test Rust workspace",
            ),
        ),
    ),
)
LINUX = Selection(
    "linux",
    "test",
    (
        RequiredJob("verify", ("Verify the local-gate attestation and decide attested skips",)),
        RequiredJob("select-tier", ()),
        RequiredJob(
            "linux",
            (
                "Verify selected CI tier",
                "Docker engine version",
                "Install",
                "Lint",
                "Test (unit + docker)",
            ),
        ),
    ),
)


@dataclass(frozen=True)
class Section:
    name: str
    stage: str
    status: str
    conclusion: str


@dataclass(frozen=True)
class Job:
    job_id: str
    status: str
    conclusion: str
    sections: tuple[Section, ...]


@dataclass(frozen=True)
class Proof:
    workspace: Path
    sha: str
    act_version: str
    jobs: tuple[Job, ...]


@dataclass(frozen=True)
class Captured:
    returncode: int
    output: str


def document(value: JsonValue) -> dict[str, JsonValue]:
    """Validate a JSON object only at the receipt boundary."""
    if not isinstance(value, dict):
        raise ValueError("expected a JSON object")
    return value


def values(value: JsonValue) -> list[JsonValue]:
    if not isinstance(value, list):
        raise ValueError("expected a JSON array")
    return value


def text(value: JsonValue) -> str:
    if not isinstance(value, str):
        raise ValueError("expected a JSON string")
    return value


def integer(value: JsonValue) -> int:
    if type(value) is not int:
        raise ValueError("expected a JSON integer")
    return value


def _section(value: JsonValue) -> Section:
    raw = document(value)
    return Section(*(text(raw[key]) for key in ("name", "stage", "status", "conclusion")))


def _job(value: JsonValue) -> Job:
    raw = document(value)
    # Non-required setup/upload steps may be skipped; they are not proof.
    sections = tuple(
        _section(section)
        for section in values(raw["sections"])
        if document(section).get("stage") == "Main"
    )
    return Job(text(raw["job_id"]), text(raw["status"]), text(raw["conclusion"]), sections)


def _parse_proof(output: str, selection: Selection) -> Proof:
    documents: list[dict[str, JsonValue]] = [
        document(json.loads(line)) for line in output.splitlines() if line.startswith("{")
    ]
    if len(documents) != 1:
        raise ValueError("expected one unambiguous terminal Bosn receipt")
    raw = documents[0]
    if raw["dirty"] is not None:
        raise ValueError("Bosn executed a dirty snapshot")
    if integer(raw["schema_version"]) != 1 or integer(raw["exit_code"]) != 0:
        raise ValueError("unknown receipt schema or nonzero exit code")
    expected = {
        "state": "done",
        "conclusion": "success",
        "engine": "act",
        "event": "workflow_dispatch",
        "workflow": WORKFLOW,
        "job": selection.job_id,
        "mode": "minimal",
    }
    if any(raw[key] != value for key, value in expected.items()):
        raise ValueError("Bosn did not successfully execute the declared quick selection")
    inputs = document(document(raw["params"])["inputs"])
    if inputs.get("tier") != selection.tier:
        raise ValueError("Bosn executed another CI tier")
    tree = document(raw["tree"])
    if integer(tree["malformed_lines"]) != 0:
        raise ValueError("Bosn could not parse every workflow log record")
    jobs = tuple(
        _job(job) for group in values(tree["groups"]) for job in values(document(group)["jobs"])
    )
    return Proof(Path(text(raw["workspace"])), text(raw["sha"]), text(raw["act_version"]), jobs)


def _jobs_error(jobs: tuple[Job, ...], selection: Selection) -> str | None:
    expected_ids = {job.job_id for job in selection.required_jobs}
    if len(jobs) != len(expected_ids) or {job.job_id for job in jobs} != expected_ids:
        return "missing, duplicate or unexpected Bosn jobs"
    for required in selection.required_jobs:
        job = next(job for job in jobs if job.job_id == required.job_id)
        if job.status != "completed" or job.conclusion != "success":
            return f"Bosn job {job.job_id} did not pass"
        completed = {
            section.name
            for section in job.sections
            if section.status == "completed" and section.conclusion == "success"
        }
        if not set(required.steps).issubset(completed):
            return f"Bosn job {job.job_id} did not execute every required step"
    return None


def proof_error(output: str, *, selection: Selection, workspace: Path, head_sha: str) -> str | None:
    try:
        proof = _parse_proof(output, selection)
        if not proof.workspace.is_absolute() or proof.workspace.resolve() != workspace.resolve():
            return "Bosn executed another workspace"
        if proof.sha != head_sha:
            return "Bosn executed another commit"
        match = re.fullmatch(r"\d+\.\d+\.\d+-act2\.(\d+)", proof.act_version)
        if not match or int(match[1]) < 3:
            return "Bosn did not use the released act2 runner"
        return _jobs_error(proof.jobs, selection)
    except (KeyError, TypeError, ValueError, json.JSONDecodeError) as error:
        return f"invalid Bosn proof: {error}"


@dataclass(frozen=True, order=True)
class Version:
    major: int
    minor: int
    patch: int


def version_error(output: str, returncode: int) -> str | None:
    match = re.fullmatch(r"bosn (\d+)\.(\d+)\.(\d+)", output.strip())
    if returncode or not match or Version(*(int(n) for n in match.groups())) < Version(0, 1, 12):
        return "local gate requires Bosn >= 0.1.12 (act2.3); check the active environment PATH"
    return None


def fidelity_error(host_arch: str, daemon: str, returncode: int) -> str | None:
    if host_arch.lower() not in {"x86_64", "amd64"}:
        return "Linux tests require a native x64 host CPU"
    if returncode != 0 or daemon.strip().lower() not in {"linux x86_64", "linux amd64"}:
        return "Linux tests require a Linux x64 Docker daemon"
    return None


def run_captured(argv: list[str]) -> Captured:
    """Use files so a daemon inheriting output cannot hold a pipe open."""
    with tempfile.TemporaryFile() as output:
        child = subprocess.run(argv, cwd=ROOT, stdout=output, stderr=subprocess.STDOUT, check=False)
        output.seek(0)
        return Captured(child.returncode, output.read().decode("utf-8", errors="replace"))


def bosn_argv() -> list[str]:
    """Select a published runner while a broken current release is being fixed."""
    version = os.environ.get("BOSN_GATE_VERSION")
    if not version:
        return ["bosn"]
    if not re.fullmatch(r"\d+\.\d+\.\d+", version):
        raise ValueError("BOSN_GATE_VERSION must be a published x.y.z version")
    return ["uvx", "--from", f"bosn=={version}", "bosn"]


def head_sha() -> str:
    status = run_captured(["git", "status", "--porcelain"])
    head = run_captured(["git", "rev-parse", "HEAD"])
    if status.returncode or status.output.strip() or head.returncode:
        raise ValueError("local gate requires a clean committed worktree")
    return head.output.strip()


def command(selection: Selection) -> list[str]:
    argv = bosn_argv() + [
        "ci",
        "run",
        "--workspace",
        str(ROOT),
        "--workflow",
        WORKFLOW,
        "--job",
        selection.job_id,
        "--event",
        "workflow_dispatch",
        "--input",
        f"tier={selection.tier}",
        "--mode",
        "minimal",
        "--engine",
        "act",
        "--timeout-secs",
        "3600",
        "--wait",
        "--deadline-ms",
        "3600000",
        "--json",
    ]
    state_dir = os.environ.get("BOSN_GATE_STATE_DIR")
    if state_dir:
        argv.extend(["--state-dir", str(Path(state_dir).resolve())])
    return argv


def run_selection(selection: Selection) -> int:
    try:
        head = head_sha()
        version = run_captured(bosn_argv() + ["--version"])
        error = version_error(version.output, version.returncode)
        if error:
            raise ValueError(error)
        daemon = run_captured(["docker", "info", "--format", "{{.OSType}} {{.Architecture}}"])
        error = fidelity_error(platform.machine(), daemon.output, daemon.returncode)
        if error:
            raise ValueError(error)
        print(f"local gate: replaying {selection.job_id} through Bosn act2", flush=True)
        result = run_captured(command(selection))
        logs = ROOT / "target" / "local-gate-logs"
        logs.mkdir(parents=True, exist_ok=True)
        (logs / f"{selection.job_id}.log").write_text(result.output, encoding="utf-8")
        if head_sha() != head:
            raise ValueError("source changed while Bosn executed")
        error = proof_error(result.output, selection=selection, workspace=ROOT, head_sha=head)
        if result.returncode or error:
            print(result.output[-12000:])
            raise ValueError(error or f"Bosn exited {result.returncode}")
        print(f"local gate: {selection.job_id} passed with clean source and executed-step proof")
        return 0
    except (OSError, ValueError) as error:
        print(f"local gate: {error}")
        return 1


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--lane", choices=("py-static", "guards", "rust", "tests"))
    args = parser.parse_args(argv)
    for lane in (args.lane,) if args.lane else ("py-static", "guards", "rust", "tests"):
        if lane in {"py-static", "guards"}:
            code = subprocess.run(
                [sys.executable, "ci/local_gate.py", "--lane", lane], cwd=ROOT
            ).returncode
        else:
            code = run_selection(RUST if lane == "rust" else LINUX)
        if code:
            return code
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
