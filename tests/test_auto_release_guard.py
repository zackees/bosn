"""The pretag release gate: auto-release.yml tags and publishes only an exact SHA on
main whose full CI run is green, and only when someone asks for that SHA.

ci/release_gate.py is the guard's whole decision, so it is tested directly: the git
checks against a scratch repository, the CI-run and issue checks against API-shaped
documents, and the workflow wiring against the YAML itself.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Any

import pytest
import yaml

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "ci"))
import release_gate as gate  # noqa: E402
from verify_full_coverage import REQUIRED  # noqa: E402

WORKFLOW = ROOT / ".github/workflows/auto-release.yml"
CI = ROOT / ".github/workflows/ci.yml"
# Hermetic git: a developer's global config (e.g. `tag.gpgSign`) must not change the result.
HERMETIC_GIT = {"GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1"}
SHA = "a" * 40

needs_git = pytest.mark.skipif(shutil.which("git") is None, reason="needs git")


def git(repo: Path, *args: str) -> str:
    with tempfile.TemporaryFile("w+", encoding="utf-8") as out:
        subprocess.run(
            ["git", *args], cwd=repo, env={**os.environ, **HERMETIC_GIT}, check=True, stdout=out
        )
        out.seek(0)
        return out.read().strip()


def commit(repo: Path, version: str, message: str) -> str:
    (repo / "Cargo.toml").write_text(
        f'# {message}\n[workspace]\nmembers = []\n\n[workspace.package]\nversion = "{version}"\n',
        encoding="utf-8",
    )
    git(repo, "add", "-A")
    git(repo, "commit", "-q", "-m", message)
    return git(repo, "rev-parse", "HEAD")


@pytest.fixture
def repo(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    repo = tmp_path / "repo"
    repo.mkdir()
    git(repo, "init", "-q", "-b", "main")
    git(repo, "config", "user.email", "gate@test.invalid")
    git(repo, "config", "user.name", "gate test")
    for key, value in HERMETIC_GIT.items():
        monkeypatch.setenv(key, value)
    monkeypatch.chdir(repo)
    return repo


def publish_main(repo: Path) -> None:
    """origin/main is the repository's own main, as the guard's checkout sees it."""
    git(repo, "update-ref", "refs/remotes/origin/main", "refs/heads/main")


def green_run(sha: str = SHA, overrides: dict[str, object] | None = None) -> dict[str, object]:
    run: dict[str, object] = {
        "id": 7,
        "event": "workflow_dispatch",
        "path": ".github/workflows/ci.yml",
        "head_branch": "main",
        "display_title": f"CI full {sha}",
        "status": "completed",
        "conclusion": "success",
        "html_url": "https://github.com/zackees/bosn/actions/runs/7",
    }
    run.update(overrides or {})
    return run


def green_jobs() -> list[dict[str, object]]:
    names = (*REQUIRED, gate.COVERAGE_JOB, "Verify local gate", "Select CI tier")
    return [
        {"id": index, "name": name, "status": "completed", "conclusion": "success"}
        for index, name in enumerate(names, 1)
    ]


# --- the candidate: exact, checked out, on main ---------------------------------


@needs_git
def test_a_commit_on_main_is_a_candidate(repo: Path) -> None:
    sha = commit(repo, "0.1.9", "release 0.1.9")
    publish_main(repo)
    gate.check_candidate(sha)
    gate.check_tag("v0.1.9", sha)


@needs_git
def test_an_abbreviated_or_uppercase_sha_is_refused(repo: Path) -> None:
    sha = commit(repo, "0.1.9", "release 0.1.9")
    publish_main(repo)
    for bad in (sha[:12], sha.upper(), "main"):
        with pytest.raises(gate.GateError, match="40-character"):
            gate.check_candidate(bad)


@needs_git
def test_a_commit_off_main_is_refused(repo: Path) -> None:
    commit(repo, "0.1.8", "release 0.1.8")
    publish_main(repo)
    git(repo, "checkout", "-q", "-b", "side")
    side = commit(repo, "0.1.9", "side work")
    with pytest.raises(gate.GateError, match="not reachable from origin/main"):
        gate.check_candidate(side)


@needs_git
def test_the_candidate_must_be_what_is_checked_out(repo: Path) -> None:
    first = commit(repo, "0.1.8", "release 0.1.8")
    commit(repo, "0.1.9", "later")
    publish_main(repo)
    with pytest.raises(gate.GateError, match="checkout is"):
        gate.check_candidate(first)


@needs_git
def test_an_existing_tag_may_only_name_the_candidate(repo: Path) -> None:
    old = commit(repo, "0.1.9", "first try")
    git(repo, "tag", "v0.1.9")
    new = commit(repo, "0.1.9", "second try")
    gate.check_tag("v0.1.9", old)  # resuming the same attempt
    with pytest.raises(gate.GateError, match="already names"):
        gate.check_tag("v0.1.9", new)


# --- the proof: a green full CI run of exactly that SHA ---------------------------


def test_a_green_full_dispatch_of_the_sha_is_proof() -> None:
    gate.check_ci_run(gate.parse_run(green_run(), green_jobs()), SHA)


@pytest.mark.parametrize(
    ("override", "message"),
    [
        ({"event": "push"}, "explicit dispatch"),
        ({"event": "pull_request"}, "explicit dispatch"),
        ({"path": ".github/workflows/auto-release.yml"}, "explicit dispatch"),
        ({"head_branch": "feature"}, "not main"),
        ({"display_title": f"CI full {'b' * 40}"}, "not a full run of"),
        ({"display_title": "CI"}, "not a full run of"),
        ({"status": "in_progress", "conclusion": None}, "in_progress"),
        ({"conclusion": "failure"}, "failure"),
        ({"conclusion": "cancelled"}, "cancelled"),
    ],
)
def test_anything_but_a_green_full_dispatch_of_the_sha_is_refused(
    override: dict[str, object], message: str
) -> None:
    with pytest.raises(gate.GateError, match=message):
        gate.check_ci_run(gate.parse_run(green_run(SHA, override), green_jobs()), SHA)


@pytest.mark.parametrize("name", [*REQUIRED, gate.COVERAGE_JOB])
def test_every_full_tier_cell_must_have_run_and_passed(name: str) -> None:
    jobs = green_jobs()
    run = gate.parse_run(green_run(), [job for job in jobs if job["name"] != name])
    with pytest.raises(gate.GateError, match="missing"):
        gate.check_ci_run(run, SHA)
    skipped = [{**job, "conclusion": "skipped"} if job["name"] == name else job for job in jobs]
    with pytest.raises(gate.GateError, match="skipped"):
        gate.check_ci_run(gate.parse_run(green_run(), skipped), SHA)


def test_a_rerun_job_counts_by_its_latest_attempt() -> None:
    jobs = green_jobs()
    jobs.append({"id": 0, "name": REQUIRED[0], "status": "completed", "conclusion": "failure"})
    gate.check_ci_run(gate.parse_run(green_run(), jobs), SHA)


def test_ci_names_a_full_dispatch_for_its_candidate() -> None:
    document = yaml.safe_load(CI.read_text(encoding="utf-8"))
    assert "format('CI full {0}', inputs.commit_sha)" in document["run-name"]
    assert "inputs.tier == 'full'" in document["run-name"]
    assert gate.ci_title(SHA) == f"CI full {SHA}"


# --- the optional release-request issue -------------------------------------------


def test_an_open_issue_naming_the_candidate_and_tag_is_a_request() -> None:
    body = f"Please release.\n\ncandidate_sha: `{SHA}`\ntag: v0.1.9\n"
    gate.check_issue(gate.parse_issue({"number": 5, "state": "open", "body": body}), SHA, "v0.1.9")


@pytest.mark.parametrize(
    ("state", "body", "message"),
    [
        ("closed", f"candidate_sha: {SHA}\ntag: v0.1.9", "closed"),
        ("open", f"candidate_sha: {'b' * 40}\ntag: v0.1.9", "names"),
        ("open", f"candidate_sha: {SHA}\ntag: v0.1.8", "names"),
        ("open", "release the latest main please", "names"),
    ],
)
def test_an_issue_that_does_not_name_this_release_is_refused(
    state: str, body: str, message: str
) -> None:
    issue = gate.parse_issue({"number": 5, "state": state, "body": body})
    with pytest.raises(gate.GateError, match=message):
        gate.check_issue(issue, SHA, "v0.1.9")


# --- end to end: what the guard step writes ---------------------------------------


@needs_git
def test_resolve_writes_the_release_only_after_every_check(
    repo: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    sha = commit(repo, "0.1.9", "release 0.1.9")
    publish_main(repo)
    monkeypatch.setattr(
        gate, "fetch_run", lambda _repo, _id, _token: gate.parse_run(green_run(sha), green_jobs())
    )
    body = f"candidate_sha: {sha}\ntag: v0.1.9"
    monkeypatch.setattr(
        gate,
        "fetch_issue",
        lambda _repo, _n, _token: gate.parse_issue({"number": 5, "state": "open", "body": body}),
    )
    output = tmp_path / "output"
    argv = ["resolve", "--candidate-sha", sha, "--ci-run-id", "7", "--issue", "5"]
    assert gate.main([*argv, "--dry-run", "false", "--output", str(output)]) == 0
    assert output.read_text(encoding="utf-8").splitlines() == [
        "release=true",
        "tag=v0.1.9",
        f"sha={sha}",
        "dry_run=false",
    ]
    output.unlink()
    monkeypatch.setattr(
        gate,
        "fetch_run",
        lambda _repo, _id, _token: gate.parse_run(green_run(sha, {"conclusion": "failure"}), []),
    )
    assert gate.main([*argv, "--output", str(output)]) == 1
    assert not output.exists(), "a refused release writes no outputs"


# --- the workflow: dispatch only, and nothing before the guard ---------------------


def release_workflow() -> Any:
    return yaml.safe_load(WORKFLOW.read_text(encoding="utf-8"))


def test_only_an_explicit_dispatch_can_start_a_release() -> None:
    triggers = release_workflow()[True]  # PyYAML reads the `on:` key as True
    assert set(triggers) == {"workflow_dispatch"}, "no version-bump or tag-push trigger"
    inputs = triggers["workflow_dispatch"]["inputs"]
    assert inputs["candidate_sha"]["required"] and inputs["full_ci_run_id"]["required"]
    assert inputs["dry_run"]["default"] is True


def test_every_job_waits_for_the_guard_and_the_guard_runs_the_gate() -> None:
    jobs = release_workflow()["jobs"]
    steps = jobs["guard"]["steps"]
    assert steps[0]["with"]["ref"] == "${{ inputs.candidate_sha }}"
    (source,) = [step for step in steps if step.get("id") == "source"]
    assert "ci/release_gate.py resolve" in source["run"]
    for name, job in jobs.items():
        if name == "guard":
            continue
        needs = job["needs"] if isinstance(job["needs"], list) else [job["needs"]]
        assert "guard" in needs, name
        assert "needs.guard.outputs.release == 'true'" in job["if"], name


def test_the_tag_is_created_only_by_the_release_job_at_the_guarded_sha() -> None:
    text = WORKFLOW.read_text(encoding="utf-8")
    assert "git tag" not in text and "git push" not in text
    job = release_workflow()["jobs"]["github-release"]
    (create,) = [s for s in job["steps"] if "gh release create" in s.get("run", "")]
    assert create["env"]["RELEASE_SHA"] == "${{ needs.guard.outputs.sha }}"
    assert '--target "$RELEASE_SHA"' in create["run"]
    assert "needs.guard.outputs.dry_run == 'false'" in job["if"]
