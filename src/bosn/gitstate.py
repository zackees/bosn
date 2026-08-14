"""Read-only Git workspace classifier for derived done-signals.

bosn frees a workspace's caches when it decides the workspace is "finished." The only
first-party signal is `bosn done`. This module adds a *derived* signal: infer "finished"
from repository state so a worktree deleted outside clud's teardown hook, or simply left
clean and pushed, gets its caches reclaimed before the TTL expires instead of after.

The asymmetry that governs every decision here: a false "finished" makes someone's warm
cache collectable while they are still using it -- expensive, sometimes irrecoverable. A
false "still working" only costs some disk until the TTL expires -- cheap. So every
ambiguous or unavailable state MUST classify as protected, never finished. This module
never mutates a repository (no fetch, no gc, no writes) -- it runs unattended in a daemon
against a developer's real worktrees, so a bug here must never be able to touch their data.

Untracked-files decision: untracked files do NOT by themselves make a workspace DIRTY.
`git status --porcelain` reports untracked files as `??` lines, and this module treats
those as non-blocking while any other porcelain line (staged, modified, deleted, renamed,
conflicted) still marks DIRTY. The tradeoff: build output, IDE scratch files, and other
disposable junk sit untracked in most real workspaces, and treating any stray file as
"still working" would mean almost no workspace is ever safe to mark done -- the derived
signal would never fire and #49 would ship no observable benefit. The risk this accepts is
narrow: genuine new work that was never `git add`ed could theoretically be discarded. But
`bosn done` only clears *caches* (stacks/specs), never the workspace's files themselves --
an untracked file on disk survives a done-mark untouched, it just stops being kept warm.
Losing warm cache for untracked work is the same cheap failure mode as a TTL expiry, not
data loss, so the severe asymmetry above does not apply to this specific choice.
"""

from __future__ import annotations

import shutil
import subprocess
from dataclasses import dataclass
from enum import Enum
from pathlib import Path

import running_process as rp

GIT_TIMEOUT_SECONDS = 15


class WorkspaceState(Enum):
    ABSENT = "absent"
    """The workspace directory does not exist."""

    CLEAN = "clean"
    """A repo, no uncommitted changes, nothing unpushed."""

    DIRTY = "dirty"
    """Uncommitted changes (staged, modified, deleted, renamed, or conflicted)."""

    UNPUSHED = "unpushed"
    """Commits not present on the tracking branch."""

    DETACHED = "detached"
    """Detached HEAD -- there is no branch to compare against a remote."""

    NO_UPSTREAM = "no_upstream"
    """No tracking branch configured, so "pushed" cannot be proven."""

    NOT_A_REPO = "not_a_repo"
    """The path exists but is not a Git repository (or worktree)."""

    UNAVAILABLE = "unavailable"
    """Git is missing, or a command failed/timed out for any other reason."""


# Single obvious table for the one property that matters: which states are safe to
# infer "done" from. Only ABSENT and CLEAN qualify. Every other state -- including
# every failure mode -- is False, deliberately, so a reader can audit the safety
# property without tracing conditionals. Do not special-case around this table.
_SAFE_TO_MARK_DONE: dict[WorkspaceState, bool] = {
    WorkspaceState.ABSENT: True,
    WorkspaceState.CLEAN: True,
    WorkspaceState.DIRTY: False,
    WorkspaceState.UNPUSHED: False,
    WorkspaceState.DETACHED: False,
    WorkspaceState.NO_UPSTREAM: False,
    WorkspaceState.NOT_A_REPO: False,
    WorkspaceState.UNAVAILABLE: False,
}


@dataclass(frozen=True)
class WorkspaceVerdict:
    state: WorkspaceState
    safe_to_mark_done: bool
    evidence: str


def _verdict(state: WorkspaceState, evidence: str) -> WorkspaceVerdict:
    return WorkspaceVerdict(
        state=state, safe_to_mark_done=_SAFE_TO_MARK_DONE[state], evidence=evidence
    )


def _run_git(args: list[str], cwd: Path) -> rp.CompletedProcess[str] | None:
    """Run a git command, returning None (never raising) when it could not be trusted.

    A None result must always be interpreted as UNAVAILABLE by the caller -- this
    function does not distinguish "git said no" (a normal non-zero exit, e.g. no
    upstream) from a real failure. Callers that need to tell those apart inspect
    `.returncode` on a non-None result themselves.
    """
    try:
        return rp.subprocess_run(["git", *args], cwd=cwd, check=False, timeout=GIT_TIMEOUT_SECONDS)
    except KeyboardInterrupt:
        raise
    except (RuntimeError, OSError, subprocess.SubprocessError):
        # RuntimeError: running_process wraps timeouts and spawn failures (missing
        # binary) as RuntimeError. OSError/SubprocessError: defensive belt-and-braces
        # for any lower-level spawn failure that slips through unwrapped. Every path
        # here means the command could not be trusted, so the caller must fall back
        # to UNAVAILABLE rather than guess at a state.
        return None


def classify_workspace(path: Path | str) -> WorkspaceVerdict:
    """Classify a workspace's Git state for the derived done-signal.

    Read-only: never fetches, never runs gc, never writes anything. Shells out to
    `git` using plumbing/porcelain forms only (no human-readable log parsing), and
    treats any missing binary, non-zero exit that isn't a recognized "no upstream"
    signal, or timeout as UNAVAILABLE -- the safe default when the tool cannot prove
    a workspace is finished.
    """
    workspace = Path(path)

    if not workspace.exists():
        return _verdict(WorkspaceState.ABSENT, f"{workspace} does not exist")

    if shutil.which("git") is None:
        return _verdict(WorkspaceState.UNAVAILABLE, "git is not on PATH")

    # `git rev-parse --is-inside-work-tree` is the standard plumbing probe for "is this
    # a repository (or worktree) at all," and fails cleanly (non-zero, no exception)
    # outside one.
    inside = _run_git(["rev-parse", "--is-inside-work-tree"], cwd=workspace)
    if inside is None:
        return _verdict(
            WorkspaceState.UNAVAILABLE, "git rev-parse --is-inside-work-tree failed to run"
        )
    if inside.returncode != 0 or inside.stdout.strip() != "true":
        return _verdict(WorkspaceState.NOT_A_REPO, f"{workspace} is not a git repository")

    status_proc = _run_git(["status", "--porcelain"], cwd=workspace)
    if status_proc is None or status_proc.returncode != 0:
        return _verdict(WorkspaceState.UNAVAILABLE, "git status --porcelain failed to run")

    status_lines = [line for line in status_proc.stdout.splitlines() if line.strip()]
    # Untracked-only entries ("?? path") do not block a done-mark; see module docstring
    # for the tradeoff. Anything else (staged/modified/deleted/renamed/conflicted) does.
    tracked_changes = [line for line in status_lines if not line.startswith("??")]
    if tracked_changes:
        noun = "change" if len(tracked_changes) == 1 else "changes"
        return _verdict(
            WorkspaceState.DIRTY,
            f"{len(tracked_changes)} uncommitted {noun}",
        )

    head_proc = _run_git(["symbolic-ref", "-q", "--short", "HEAD"], cwd=workspace)
    if head_proc is None:
        return _verdict(WorkspaceState.UNAVAILABLE, "git symbolic-ref failed to run")
    if head_proc.returncode != 0:
        # symbolic-ref fails specifically (and only) when HEAD is not a symbolic ref,
        # i.e. detached. It is not a generic failure mode we need to distinguish
        # further -- there is no upstream question to ask without a branch.
        return _verdict(WorkspaceState.DETACHED, "HEAD is detached")
    branch = head_proc.stdout.strip()

    upstream_proc = _run_git(
        ["rev-list", "--count", "@{upstream}..HEAD"],
        cwd=workspace,
    )
    if upstream_proc is None:
        return _verdict(WorkspaceState.UNAVAILABLE, "git rev-list failed to run")
    if upstream_proc.returncode != 0:
        # rev-list fails with "no upstream configured for branch" when @{upstream}
        # cannot be resolved -- a normal, expected outcome for a local-only branch,
        # not a tool failure. Distinguish it from UNAVAILABLE explicitly rather than
        # folding it into the generic failure path.
        return _verdict(WorkspaceState.NO_UPSTREAM, f"{branch!r} has no tracking branch")

    ahead_text = upstream_proc.stdout.strip()
    try:
        ahead = int(ahead_text)
    except ValueError:
        return _verdict(
            WorkspaceState.UNAVAILABLE, f"git rev-list returned non-numeric output: {ahead_text!r}"
        )

    if ahead > 0:
        noun = "commit" if ahead == 1 else "commits"
        return _verdict(WorkspaceState.UNPUSHED, f"{ahead} {noun} ahead of upstream")

    return _verdict(WorkspaceState.CLEAN, f"{branch!r} is clean and matches upstream")
