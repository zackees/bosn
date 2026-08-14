"""Git workspace classifier: the safety property under test is the safe_to_mark_done table.

Every state except ABSENT and CLEAN must be False, including every failure mode. Real git
repositories are built in tmp dirs for each case rather than mocked, so these tests prove
the actual command parsing (porcelain output, rev-list exit codes, symbolic-ref failure on
detached HEAD) rather than an assumption about it.
"""

from __future__ import annotations

import subprocess
from pathlib import Path

import pytest

from bosn.gitstate import WorkspaceState, classify_workspace

GIT_ENV = {
    "GIT_AUTHOR_NAME": "Test",
    "GIT_AUTHOR_EMAIL": "test@example.com",
    "GIT_COMMITTER_NAME": "Test",
    "GIT_COMMITTER_EMAIL": "test@example.com",
}


def _git(cwd: Path, *args: str) -> None:
    subprocess.run(
        ["git", *args],
        cwd=cwd,
        check=True,
        capture_output=True,
        text=True,
        env={**_env(), **GIT_ENV},
    )


def _env() -> dict[str, str]:
    import os

    return dict(os.environ)


def _init_repo(path: Path) -> None:
    path.mkdir(parents=True, exist_ok=True)
    _git(path, "init", "-q", "-b", "main")
    _git(path, "config", "user.name", "Test")
    _git(path, "config", "user.email", "test@example.com")


def _commit_all(path: Path, message: str) -> None:
    _git(path, "add", "-A")
    _git(path, "commit", "-q", "-m", message)


def _make_bare_remote(tmp_path: Path) -> Path:
    remote = tmp_path / "remote.git"
    remote.mkdir()
    subprocess.run(["git", "init", "-q", "--bare", str(remote)], check=True, capture_output=True)
    return remote


def test_absent_workspace(tmp_path: Path) -> None:
    missing = tmp_path / "does-not-exist"
    verdict = classify_workspace(missing)
    assert verdict.state is WorkspaceState.ABSENT
    assert verdict.safe_to_mark_done is True
    assert verdict.evidence


def test_not_a_repo(tmp_path: Path) -> None:
    plain = tmp_path / "plain-dir"
    plain.mkdir()
    (plain / "file.txt").write_text("hello", encoding="utf-8")
    verdict = classify_workspace(plain)
    assert verdict.state is WorkspaceState.NOT_A_REPO
    assert verdict.safe_to_mark_done is False


def test_clean_and_pushed(tmp_path: Path) -> None:
    remote = _make_bare_remote(tmp_path)
    repo = tmp_path / "repo"
    _init_repo(repo)
    (repo / "a.txt").write_text("one", encoding="utf-8")
    _commit_all(repo, "initial")
    _git(repo, "remote", "add", "origin", str(remote))
    _git(repo, "push", "-q", "-u", "origin", "main")

    verdict = classify_workspace(repo)
    assert verdict.state is WorkspaceState.CLEAN
    assert verdict.safe_to_mark_done is True
    assert verdict.evidence


def test_dirty_tracked_modification(tmp_path: Path) -> None:
    remote = _make_bare_remote(tmp_path)
    repo = tmp_path / "repo"
    _init_repo(repo)
    (repo / "a.txt").write_text("one", encoding="utf-8")
    _commit_all(repo, "initial")
    _git(repo, "remote", "add", "origin", str(remote))
    _git(repo, "push", "-q", "-u", "origin", "main")

    (repo / "a.txt").write_text("modified", encoding="utf-8")

    verdict = classify_workspace(repo)
    assert verdict.state is WorkspaceState.DIRTY
    assert verdict.safe_to_mark_done is False
    assert "1" in verdict.evidence


def test_untracked_only_is_not_dirty(tmp_path: Path) -> None:
    """Untracked files (build output, scratch files) do not block a done-mark by themselves."""
    remote = _make_bare_remote(tmp_path)
    repo = tmp_path / "repo"
    _init_repo(repo)
    (repo / "a.txt").write_text("one", encoding="utf-8")
    _commit_all(repo, "initial")
    _git(repo, "remote", "add", "origin", str(remote))
    _git(repo, "push", "-q", "-u", "origin", "main")

    (repo / "build-output.tmp").write_text("scratch", encoding="utf-8")

    verdict = classify_workspace(repo)
    assert verdict.state is WorkspaceState.CLEAN
    assert verdict.safe_to_mark_done is True


def test_unpushed_commits(tmp_path: Path) -> None:
    remote = _make_bare_remote(tmp_path)
    repo = tmp_path / "repo"
    _init_repo(repo)
    (repo / "a.txt").write_text("one", encoding="utf-8")
    _commit_all(repo, "initial")
    _git(repo, "remote", "add", "origin", str(remote))
    _git(repo, "push", "-q", "-u", "origin", "main")

    (repo / "b.txt").write_text("two", encoding="utf-8")
    _commit_all(repo, "second")

    verdict = classify_workspace(repo)
    assert verdict.state is WorkspaceState.UNPUSHED
    assert verdict.safe_to_mark_done is False
    assert "1" in verdict.evidence


def test_detached_head(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    (repo / "a.txt").write_text("one", encoding="utf-8")
    _commit_all(repo, "initial")
    sha = subprocess.run(
        ["git", "rev-parse", "HEAD"], cwd=repo, check=True, capture_output=True, text=True
    ).stdout.strip()
    _git(repo, "checkout", "-q", sha)

    verdict = classify_workspace(repo)
    assert verdict.state is WorkspaceState.DETACHED
    assert verdict.safe_to_mark_done is False


def test_no_upstream(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    (repo / "a.txt").write_text("one", encoding="utf-8")
    _commit_all(repo, "initial")

    verdict = classify_workspace(repo)
    assert verdict.state is WorkspaceState.NO_UPSTREAM
    assert verdict.safe_to_mark_done is False
    assert "main" in verdict.evidence


def test_git_unavailable(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)

    monkeypatch.setattr("bosn.gitstate.shutil.which", lambda _binary: None)

    verdict = classify_workspace(repo)
    assert verdict.state is WorkspaceState.UNAVAILABLE
    assert verdict.safe_to_mark_done is False
    assert verdict.evidence


def test_git_command_failure_yields_unavailable(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A raised RuntimeError (timeout, spawn failure) from the subprocess wrapper must not
    propagate -- it has to collapse to a normal, protected UNAVAILABLE verdict."""
    repo = tmp_path / "repo"
    _init_repo(repo)

    def boom(*_args: object, **_kwargs: object) -> None:
        raise RuntimeError("simulated timeout")

    monkeypatch.setattr("bosn.gitstate.rp.subprocess_run", boom)

    verdict = classify_workspace(repo)
    assert verdict.state is WorkspaceState.UNAVAILABLE
    assert verdict.safe_to_mark_done is False


@pytest.mark.parametrize(
    "state",
    list(WorkspaceState),
)
def test_safety_table_is_exhaustive_and_explicit(state: WorkspaceState) -> None:
    """A new WorkspaceState member must get an explicit safety decision, not a default."""
    from bosn.gitstate import _SAFE_TO_MARK_DONE

    assert state in _SAFE_TO_MARK_DONE
    expected_safe = state in (WorkspaceState.ABSENT, WorkspaceState.CLEAN)
    assert _SAFE_TO_MARK_DONE[state] is expected_safe
