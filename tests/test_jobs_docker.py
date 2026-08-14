"""Daemon-owned builds against a real engine: the policy holds with real `docker build`.

The unit tests pin the policy with a fake builder. This one proves the same thing survives
contact with BuildKit -- real build output streams to an attached client, a real build
outlives the client that asked for it, and the coalescing bound holds when the "slow build"
is genuinely slow rather than an Event the test controls.

Docker-marked: Linux CI only.
"""

from __future__ import annotations

import threading
import time
import uuid
from collections.abc import Iterator
from pathlib import Path

import pytest

from bosn import daemon as daemon_mod
from bosn import ipc, labels
from bosn.daemon import Daemon
from bosn.engine import Engine

pytestmark = pytest.mark.docker

# `sleep` in the build makes the cold build slow enough to submit against while it runs.
MANIFEST = """
[stack.test]
dockerfile = "Dockerfile"
family = "jobs"
default = true
"""


def wait_until(predicate, timeout: float = 120.0, interval: float = 0.05) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if predicate():
            return True
        time.sleep(interval)
    return predicate()


@pytest.fixture
def engine() -> Engine:
    return Engine()


@pytest.fixture
def project(tmp_path: Path) -> Path:
    root = tmp_path / "proj"
    root.mkdir()
    (root / "bosn.toml").write_text(MANIFEST, encoding="utf-8")
    write_dockerfile(root, seconds=4)
    return root


def write_dockerfile(root: Path, *, seconds: int, marker: str | None = None) -> None:
    """A fresh marker on every write, so each edit is a genuinely new generation digest."""
    marker = marker or uuid.uuid4().hex[:8]
    (root / "Dockerfile").write_text(
        f"FROM alpine:3.20\nRUN echo {marker} > /marker && sleep {seconds}\n",
        encoding="utf-8",
    )


@pytest.fixture
def served(tmp_path: Path) -> Iterator[Daemon]:
    state_dir = tmp_path / "state"
    daemon = Daemon(state_dir=state_dir, idle_retire_seconds=3600)
    thread = threading.Thread(target=daemon.serve_forever, daemon=True)
    thread.start()
    assert wait_until(lambda: daemon_mod.is_serving(state_dir), timeout=30)
    try:
        yield daemon
    finally:
        daemon.request_stop()
        thread.join(timeout=30)
        daemon.shutdown()
        _remove_built_images(Engine())


def _remove_built_images(engine: Engine) -> None:
    listed = engine.run(["image", "ls", "--filter", f"label={labels.REGISTRY}", "--quiet"])
    for image_id in {line.strip() for line in listed.stdout.splitlines() if line.strip()}:
        engine.run(["image", "rm", "--force", image_id])


def converge(daemon: Daemon, project: Path, timeout: float = 300.0):
    return ipc.stream_request(
        daemon.port,
        {"verb": "converge", "manifest": str(project / "bosn.toml"), "auth": daemon.secret},
        timeout=timeout,
    )


def test_a_real_build_streams_and_registers(served: Daemon, project: Path) -> None:
    events = list(converge(served, project))

    assert events[0]["event"] == "submitted"
    assert any(e.get("event") == "log" for e in events), "real build output must stream"
    final = events[-1]
    assert final["final"] is True and final["state"] == "succeeded", final.get("error")
    assert final["result"]["image_tag"].startswith("bosn/test:")

    kinds = sorted(r.kind for r in served.registry.list_resources())
    assert "image" in kinds


def test_a_real_build_outlives_the_client_that_asked_for_it(served: Daemon, project: Path) -> None:
    """The failure this whole design exists to fix: a killed CLI must not kill the build."""
    stream = converge(served, project)
    job_id = next(stream)["job"]
    assert wait_until(lambda: served.jobs.get(job_id).state == "running", timeout=30)

    stream.close()  # the CLI dies mid-build
    time.sleep(0.5)
    assert served.jobs.get(job_id).state == "running", "the daemon kept building"

    # ...and a re-run reattaches to it rather than starting a second build
    reattached = ipc.stream_request(
        served.port, {"verb": "attach", "job": job_id, "auth": served.secret}, timeout=300
    )
    assert next(reattached)["event"] == "attached"
    final = [e for e in reattached if e.get("final")][-1]
    assert final["state"] == "succeeded", final.get("error")


def test_the_edit_loop_bound_holds_against_real_builds(served: Daemon, project: Path) -> None:
    """Ten edits in a tight loop against one key: at most one running, one pending."""
    streams = []
    for _ in range(10):
        write_dockerfile(project, seconds=4)
        streams.append(converge(served, project))
        next(streams[-1])  # the submitted event
        active = [j for j in served.jobs.list_jobs() if j["state"] in {"running", "queued"}]
        pending = [j for j in served.jobs.list_jobs() if j["state"] == "pending"]
        assert len(active) <= 1 and len(pending) <= 1, "the queue grew past the bound"

    finals = [[e for e in stream if e.get("final")][-1] for stream in streams]
    states = [event["state"] for event in finals]

    assert states[-1] == "succeeded", "the newest edit must be the one that converged"
    assert set(states) <= {"succeeded", "superseded"}, states
    # Exact counts depend on how fast BuildKit gets through a layer, so the assertion is the
    # bound rather than a schedule: the loop cannot produce more builds than the policy
    # allows, however the timing falls.
    assert states.count("succeeded") <= 2, "at most the in-flight build plus the newest one"
    assert states.count("superseded") >= 7, "the obsolete requests were dropped, and told so"
    for event in finals:
        if event["state"] == "superseded":
            assert "superseded" in (event["error"] or ""), "a dropped request must say why"

    images = [r for r in served.registry.list_resources() if r.kind == "image"]
    assert len(images) == states.count("succeeded"), "no image without a completed build"


def test_cancelling_a_real_build_leaves_no_generation_row(served: Daemon, project: Path) -> None:
    from bosn.manifest import generation_digest, load

    manifest = load(project)
    digest = generation_digest(manifest, manifest.stack(None))

    stream = converge(served, project)
    job_id = next(stream)["job"]
    assert wait_until(lambda: served.jobs.get(job_id).state == "running", timeout=30)

    assert ipc.send_request(served.port, {"verb": "cancel", "job": job_id, "auth": served.secret})[
        "ok"
    ]
    final = [e for e in stream if e.get("final")][-1]
    assert final["state"] == "cancelled"

    rows = served.registry.conn.execute(
        "SELECT 1 FROM generations WHERE digest = ?", (digest,)
    ).fetchall()
    assert rows == [], "a cancelled build must not imply a usable image"
    assert [r for r in served.registry.list_resources() if r.kind == "image"] == []


def test_two_workspaces_build_in_parallel(served: Daemon, tmp_path: Path) -> None:
    """A slow build in worktree A never blocks worktree B."""
    roots = []
    for name in ("wt-a", "wt-b"):
        root = tmp_path / name
        root.mkdir()
        (root / "bosn.toml").write_text(MANIFEST, encoding="utf-8")
        write_dockerfile(root, seconds=6)
        roots.append(root)

    streams = [converge(served, root) for root in roots]
    ids = [next(stream)["job"] for stream in streams]

    assert wait_until(
        lambda: all(served.jobs.get(i).state == "running" for i in ids), timeout=60
    ), "both worktrees must be building at once"

    for stream in streams:
        final = [e for e in stream if e.get("final")][-1]
        assert final["state"] == "succeeded", final.get("error")


# -- the whole path, through the CLI ---------------------------------------


@pytest.mark.slow
def test_bosn_run_converges_through_the_daemon_then_runs_locally(
    tmp_path: Path, project: Path, capsys
) -> None:
    """`bosn run` end to end: a real spawned daemon builds, this process runs the command.

    The split is the design -- the daemon owns the build so it survives a killed CLI, and
    the CLI runs the container itself so the command keeps this terminal and exit status.
    Everything else here mocks one side or the other; this is the only test that exercises
    the seam the way a user does.
    """
    from bosn import cli

    write_dockerfile(project, seconds=0)
    state_dir = tmp_path / "cli-state"
    args = ["--state-dir", str(state_dir), "run", "--manifest", str(project / "bosn.toml")]
    try:
        code = cli.main([*args, "--", "echo", "hello-from-the-stack"])
        assert code == 0, capsys.readouterr().err
        assert "hello-from-the-stack" in capsys.readouterr().out

        # ...and the second run reuses the image rather than rebuilding it
        assert cli.main([*args, "--", "echo", "again"]) == 0
        assert "again" in capsys.readouterr().out

        listed = daemon_mod.request("jobs", state_dir)["jobs"]
        assert listed, "the converge really did go through the daemon"
        assert all(row["state"] == "succeeded" for row in listed), listed
    finally:
        daemon_mod.stop(state_dir, timeout=60)
        _remove_built_images(Engine())
