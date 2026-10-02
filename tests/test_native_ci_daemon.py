"""``bosn.Client.ci`` against a real daemon, with no Docker (#344).

The runner is drained first, so a submitted run stays queued and nothing
executes: the Python surface must still list, page logs, wait with a
deadline, cancel, report and retry exactly as the ``bosn_ci_*`` MCP tools do.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import tempfile
from collections.abc import Iterator
from pathlib import Path

import pytest

import bosn
from setup_docker_support import production_daemon, wait_for_daemon

GIT_ENV = {
    "GIT_AUTHOR_NAME": "t",
    "GIT_AUTHOR_EMAIL": "t@t",
    "GIT_COMMITTER_NAME": "t",
    "GIT_COMMITTER_EMAIL": "t@t",
    "PATH": os.environ["PATH"],
}


@pytest.fixture
def short_root() -> Iterator[Path]:
    """A Unix socket path must stay short; pytest's temp paths are not."""
    base = Path(tempfile.gettempdir())
    if len(str(base)) > 40:
        base = Path.home() / ".cache"
    base.mkdir(parents=True, exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix="bpy", dir=base))
    try:
        yield root
    finally:
        shutil.rmtree(root, ignore_errors=True)


def checkout(root: Path) -> Path:
    repo = root / "repo"
    (repo / ".github" / "workflows").mkdir(parents=True)
    (repo / ".github" / "workflows" / "ci.yml").write_text("on: [push]\njobs: {}\n")
    for command in (
        ["git", "init", "-q", "-b", "main"],
        ["git", "add", "-A"],
        ["git", "commit", "-qm", "init"],
    ):
        subprocess.run(command, cwd=repo, check=True, env=GIT_ENV)
    return repo


def test_client_ci_drives_a_queued_run_like_the_mcp_tools(short_root: Path) -> None:
    repo = checkout(short_root)
    state = short_root / "s"
    with production_daemon(state) as (_, daemon):
        client = bosn.Client(state)
        wait_for_daemon(client, daemon)

        drained = client.ci("runners", action="drain")
        assert drained["runners"]["drained"] is True

        submitted = client.ci("run", workspace=str(repo))
        run = submitted["run"]
        assert submitted["coalesced"] is False
        again = client.ci("run", workspace=str(repo))
        assert (again["run"], again["coalesced"]) == (run, True), "identical runs share one"

        listed = client.ci("list", limit=5)
        assert [r["id"] for r in listed["runs"]] == [run]
        assert listed["runs"][0]["state"] == "queued"
        assert listed["runners"]["queued"] == 1

        page = client.ci("logs", run=run, since_seq=0)
        assert (page["records"], page["more"]) == ([], False)

        waited = client.ci("wait", run=run, deadline_ms=200)
        assert waited["finished"] is False

        cancelled = client.ci("cancel", run=run)
        assert cancelled["cancelled"] is True
        status = client.ci("status", run=run)
        assert (status["conclusion"], status["exit_code"]) == ("cancelled", 2)
        report = client.ci("report", run=run)
        assert report["conclusion"] == "cancelled"
        assert report["logs_command"] == f"bosn ci logs {run}"

        retried = client.ci("retry", run=run)
        assert retried["run"] != run
        assert retried["record"]["retry_of"] == run
        assert client.ci("cancel", run=retried["run"])["cancelled"] is True

        with pytest.raises(RuntimeError, match="not_found|no run"):
            client.ci("status", run="00000000-0000-4000-8000-000000000000")
        resumed = client.ci("runners", action="resume")
        assert resumed["runners"]["drained"] is False
