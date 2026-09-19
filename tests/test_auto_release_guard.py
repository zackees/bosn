"""Run auto-release.yml's release guard for real, against a scratch git repository.

The guard decides whether a push to main publishes a release, so it is tested as the
exact bash in the workflow file rather than a copy of its logic.
"""

from __future__ import annotations

import os
import shutil
import stat
import subprocess
from pathlib import Path

import pytest
import yaml

WORKFLOW = Path(".github/workflows/auto-release.yml")
ZEROS = "0" * 40
# Hermetic git: a developer's global config (e.g. `tag.gpgSign`) must not change the result.
HERMETIC_GIT = {"GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1"}

pytestmark = pytest.mark.skipif(
    shutil.which("git") is None or shutil.which("bash") is None, reason="needs git and bash"
)


def guard_script() -> str:
    steps = yaml.safe_load(WORKFLOW.read_text(encoding="utf-8"))["jobs"]["guard"]["steps"]
    (step,) = [step for step in steps if step.get("id") == "source"]
    return step["run"]


def git(repo: Path, *args: str) -> str:
    return subprocess.run(
        ["git", *args],
        cwd=repo,
        env={**os.environ, **HERMETIC_GIT},
        check=True,
        capture_output=True,
        text=True,
    ).stdout.strip()


def commit(repo: Path, version: str | None, message: str) -> str:
    # The message rides along as a comment, so every commit changes Cargo.toml the way
    # a real edit to it would, whether or not the version moves.
    manifest = f"# {message}\n[workspace]\nmembers = []\n"
    if version is not None:
        manifest += f'\n[workspace.package]\nversion = "{version}"\n'
    (repo / "Cargo.toml").write_text(manifest, encoding="utf-8")
    git(repo, "add", "-A")
    git(repo, "commit", "-q", "-m", message)
    return git(repo, "rev-parse", "HEAD")


@pytest.fixture
def repo(tmp_path: Path) -> Path:
    repo = tmp_path / "repo"
    repo.mkdir()
    git(repo, "init", "-q", "-b", "main")
    git(repo, "config", "user.email", "guard@test.invalid")
    git(repo, "config", "user.name", "guard test")
    return repo


def run_guard(
    repo: Path,
    tmp_path: Path,
    *,
    event: str,
    released: set[str] = frozenset(),  # type: ignore[assignment]
    ref_type: str = "branch",
    ref_name: str = "main",
    before: str = ZEROS,
    input_tag: str = "",
    input_dry_run: str = "",
) -> tuple[int, dict[str, str]]:
    # origin/main is the repository's own main, as the checkout sees it.
    git(repo, "update-ref", "refs/remotes/origin/main", "refs/heads/main")
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir(exist_ok=True)
    fake_gh = bin_dir / "gh"
    # `gh release view TAG ...` succeeds only for a released tag.
    fake_gh.write_text(
        "#!/usr/bin/env bash\n"
        f'released=" {" ".join(sorted(released))} "\n'
        '[[ "$1 $2" == "release view" && "$released" == *" $3 "* ]]\n',
        encoding="utf-8",
    )
    fake_gh.chmod(fake_gh.stat().st_mode | stat.S_IEXEC)
    output = tmp_path / "output"
    output.write_text("", encoding="utf-8")
    env = {
        **os.environ,
        **HERMETIC_GIT,
        "PATH": f"{bin_dir}{os.pathsep}{os.environ['PATH']}",
        "GITHUB_OUTPUT": str(output),
        "GITHUB_STEP_SUMMARY": str(tmp_path / "summary"),
        "GITHUB_REPOSITORY": "zackees/bosn",
        "GH_TOKEN": "unused",
        "EVENT": event,
        "REF_TYPE": ref_type,
        "REF_NAME": ref_name,
        "BEFORE": before,
        "INPUT_TAG": input_tag,
        "INPUT_DRY_RUN": input_dry_run,
    }
    completed = subprocess.run(
        ["bash", "-c", guard_script()], cwd=repo, env=env, capture_output=True, text=True
    )
    outputs = dict(
        line.split("=", 1) for line in output.read_text(encoding="utf-8").splitlines() if line
    )
    return completed.returncode, outputs


# --- a push to main releases only a version change ---------------------------


def test_a_version_bump_on_main_releases_that_version(repo: Path, tmp_path: Path) -> None:
    before = commit(repo, "0.1.4", "release 0.1.4")
    head = commit(repo, "0.1.5", "bump")
    code, out = run_guard(repo, tmp_path, event="push", before=before, released={"v0.1.4"})
    assert code == 0
    assert out == {"release": "true", "tag": "v0.1.5", "sha": head, "dry_run": "false"}


def test_a_push_that_does_not_change_the_version_does_nothing(repo: Path, tmp_path: Path) -> None:
    before = commit(repo, "0.1.4", "release 0.1.4")
    commit(repo, "0.1.4", "an unrelated Cargo.toml edit")
    code, out = run_guard(repo, tmp_path, event="push", before=before)
    assert (code, out["release"]) == (0, "false")


def test_introducing_the_workspace_version_does_not_release(repo: Path, tmp_path: Path) -> None:
    # The merge that adds [workspace.package] must not ship: 0.1.4 already shipped.
    before = commit(repo, None, "before single-source versioning")
    commit(repo, "0.1.4", "single-source version")
    code, out = run_guard(repo, tmp_path, event="push", before=before)
    assert (code, out["release"]) == (0, "false")


def test_a_bump_that_is_already_released_does_nothing(repo: Path, tmp_path: Path) -> None:
    before = commit(repo, "0.1.4", "release 0.1.4")
    commit(repo, "0.1.5", "bump")
    code, out = run_guard(repo, tmp_path, event="push", before=before, released={"v0.1.5"})
    assert (code, out["release"]) == (0, "false")


def test_a_bump_whose_tag_was_pushed_by_hand_leaves_it_to_that_run(
    repo: Path, tmp_path: Path
) -> None:
    before = commit(repo, "0.1.4", "release 0.1.4")
    commit(repo, "0.1.5", "bump")
    git(repo, "tag", "v0.1.5")
    code, out = run_guard(repo, tmp_path, event="push", before=before)
    assert (code, out["release"]) == (0, "false")


def test_the_first_push_of_a_branch_does_not_release(repo: Path, tmp_path: Path) -> None:
    commit(repo, "0.1.5", "first")
    code, out = run_guard(repo, tmp_path, event="push", before=ZEROS)
    assert (code, out["release"]) == (0, "false")


# --- the manual routes are unchanged -------------------------------------------


def test_a_pushed_tag_releases_its_own_commit(repo: Path, tmp_path: Path) -> None:
    tagged = commit(repo, "0.1.4", "release 0.1.4")
    git(repo, "tag", "v0.1.4")
    commit(repo, "0.1.4", "later work")
    code, out = run_guard(repo, tmp_path, event="push", ref_type="tag", ref_name="v0.1.4")
    assert code == 0
    assert out == {"release": "true", "tag": "v0.1.4", "sha": tagged, "dry_run": "false"}


def test_a_dry_run_of_an_unpushed_tag_rehearses_main(repo: Path, tmp_path: Path) -> None:
    head = commit(repo, "0.1.5", "bump")
    code, out = run_guard(
        repo, tmp_path, event="workflow_dispatch", input_tag="v0.1.5", input_dry_run="true"
    )
    assert code == 0
    assert out == {"release": "true", "tag": "v0.1.5", "sha": head, "dry_run": "true"}


def test_publishing_a_tag_that_does_not_exist_is_refused(repo: Path, tmp_path: Path) -> None:
    commit(repo, "0.1.5", "bump")
    code, _ = run_guard(
        repo, tmp_path, event="workflow_dispatch", input_tag="v0.1.5", input_dry_run="false"
    )
    assert code == 1


def test_a_tag_off_main_is_refused(repo: Path, tmp_path: Path) -> None:
    commit(repo, "0.1.4", "release 0.1.4")
    git(repo, "checkout", "-q", "-b", "side")
    commit(repo, "0.1.5", "side work")
    git(repo, "tag", "v0.1.5")
    git(repo, "checkout", "-q", "main")
    code, _ = run_guard(repo, tmp_path, event="push", ref_type="tag", ref_name="v0.1.5")
    assert code == 1
