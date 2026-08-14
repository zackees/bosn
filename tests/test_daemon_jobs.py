"""Daemon-owned build jobs end to end: streaming, attach, cancel, and retirement.

These exercise the real daemon over the real loopback protocol with a fake builder, so the
streaming transport and the job policy are tested together without needing Docker. The
`docker` marker covers the same ground against a real build in `test_scenario_docker.py`.
"""

from __future__ import annotations

import threading
import time
from collections.abc import Iterator
from pathlib import Path

import pytest

from bosn import daemon as daemon_mod
from bosn import ipc
from bosn.daemon import Daemon
from bosn.jobs import BuildOutcome, Job

SAMPLE = """
[stack.dev]
dockerfile = "Dockerfile"
family = "rust"
default = true

[stack.dev.volumes]
target = { scope = "spec" }
"""


def wait_until(predicate, timeout: float = 15.0, interval: float = 0.02) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if predicate():
            return True
        time.sleep(interval)
    return predicate()


class ControlledBuilder:
    """A daemon builder the test can hold open and release."""

    def __init__(self) -> None:
        self.release = threading.Event()
        self.started = threading.Event()
        self.digests: list[str] = []

    def __call__(self, job: Job) -> BuildOutcome:
        self.digests.append(job.digest)
        self.started.set()
        job.log("step 1/2 : FROM alpine")
        while not self.release.is_set():
            if job.cancelled.is_set():
                return BuildOutcome(returncode=130)
            time.sleep(0.005)
        job.log("step 2/2 : done")
        return BuildOutcome(returncode=0, result={"stack": job.stack, "digest": job.digest})


@pytest.fixture
def project(tmp_path: Path) -> Path:
    root = tmp_path / "proj"
    root.mkdir()
    (root / "Dockerfile").write_text("FROM alpine\n", encoding="utf-8")
    (root / "bosn.toml").write_text(SAMPLE, encoding="utf-8")
    return root


@pytest.fixture
def builder() -> ControlledBuilder:
    return ControlledBuilder()


@pytest.fixture
def served(tmp_path: Path, builder: ControlledBuilder) -> Iterator[Daemon]:
    state_dir = tmp_path / "state"
    daemon = Daemon(state_dir=state_dir, idle_retire_seconds=3600)
    daemon.jobs.builder = builder
    thread = threading.Thread(target=daemon.serve_forever, daemon=True)
    thread.start()
    assert wait_until(lambda: daemon_mod.is_serving(state_dir)), "daemon never came up"
    try:
        yield daemon
    finally:
        builder.release.set()
        daemon.request_stop()
        thread.join(timeout=15)
        daemon.shutdown()


def converge_events(daemon: Daemon, project: Path, **extra):
    return ipc.stream_request(
        daemon.port,
        {
            "verb": "converge",
            "manifest": str(project / "bosn.toml"),
            "auth": daemon.secret,
            **extra,
        },
        timeout=30,
    )


def drain(events) -> list[dict]:
    return list(events)


# -- streaming converge ----------------------------------------------------


def test_converge_streams_build_output_and_a_terminal_event(
    served: Daemon, project: Path, builder: ControlledBuilder
) -> None:
    builder.release.set()
    events = drain(converge_events(served, project))

    assert events[0]["event"] == "submitted"
    assert any(e.get("event") == "log" and "FROM alpine" in e["line"] for e in events)
    assert events[-1]["final"] is True
    assert events[-1]["state"] == "succeeded"


def test_a_second_identical_converge_joins_rather_than_rebuilding(
    served: Daemon, project: Path, builder: ControlledBuilder
) -> None:
    stream = converge_events(served, project)
    assert next(stream)["event"] == "submitted"
    assert builder.started.wait(10)

    joined = next(converge_events(served, project))
    assert joined["joined"] is True, "identical in-flight work must be joined, not duplicated"

    builder.release.set()
    drain(stream)
    assert len(builder.digests) == 1, "one build served both requests"


def test_an_unreadable_manifest_is_reported_not_swallowed(served: Daemon, tmp_path: Path) -> None:
    events = drain(
        ipc.stream_request(
            served.port,
            {
                "verb": "converge",
                "manifest": str(tmp_path / "nope" / "bosn.toml"),
                "auth": served.secret,
            },
            timeout=30,
        )
    )
    assert events[-1]["ok"] is False
    assert events[-1]["final"] is True


# -- the job survives its client -------------------------------------------


def test_hanging_up_mid_build_leaves_the_job_running(
    served: Daemon, project: Path, builder: ControlledBuilder
) -> None:
    """The whole point: a killed CLI must not destroy a 20-minute build."""
    stream = converge_events(served, project)
    submitted = next(stream)
    job_id = submitted["job"]
    assert builder.started.wait(10)

    stream.close()  # the client goes away, exactly as a killed CLI would
    time.sleep(0.2)

    assert served.jobs.get(job_id).state == "running", "the daemon kept the work"

    listed = ipc.send_request(served.port, {"verb": "jobs", "auth": served.secret})["jobs"]
    assert [row for row in listed if row["id"] == job_id and row["state"] == "running"]


def test_attach_reconnects_to_a_surviving_job(
    served: Daemon, project: Path, builder: ControlledBuilder
) -> None:
    stream = converge_events(served, project)
    job_id = next(stream)["job"]
    assert builder.started.wait(10)
    stream.close()

    reattached = ipc.stream_request(
        served.port, {"verb": "attach", "job": job_id, "auth": served.secret}, timeout=30
    )
    first = next(reattached)
    assert first["event"] == "attached"
    assert first["job"] == job_id
    # the output produced while nobody was watching is replayed, not lost
    assert any("FROM alpine" in e.get("line", "") for e in [next(reattached)])

    builder.release.set()
    final = [e for e in reattached if e.get("final")]
    assert final and final[-1]["state"] == "succeeded"


def test_rerunning_the_same_verb_reattaches_instead_of_rebuilding(
    served: Daemon, project: Path, builder: ControlledBuilder
) -> None:
    stream = converge_events(served, project)
    first_id = next(stream)["job"]
    assert builder.started.wait(10)
    stream.close()

    again = next(converge_events(served, project))
    assert again["job"] == first_id, "the re-run rejoined the surviving job"
    assert again["joined"] is True


def test_attaching_to_an_unknown_job_says_so(served: Daemon) -> None:
    events = drain(
        ipc.stream_request(
            served.port, {"verb": "attach", "job": "nope", "auth": served.secret}, timeout=10
        )
    )
    assert events[-1]["ok"] is False
    assert "no such job" in events[-1]["error"]


# -- cancellation ----------------------------------------------------------


def test_cancel_stops_a_running_build_and_tells_the_watcher(
    served: Daemon, project: Path, builder: ControlledBuilder
) -> None:
    stream = converge_events(served, project)
    job_id = next(stream)["job"]
    assert builder.started.wait(10)

    reply = ipc.send_request(served.port, {"verb": "cancel", "job": job_id, "auth": served.secret})
    assert reply["ok"]

    final = [e for e in stream if e.get("final")]
    assert final and final[-1]["state"] == "cancelled"
    assert final[-1]["error"]


def test_a_cancelled_build_leaves_no_generation_row(
    served: Daemon, project: Path, builder: ControlledBuilder
) -> None:
    """A cancelled build must not imply a usable image exists."""
    from bosn.manifest import generation_digest, load

    manifest = load(project)
    digest = generation_digest(manifest, manifest.stack(None))

    stream = converge_events(served, project)
    job_id = next(stream)["job"]
    assert builder.started.wait(10)
    ipc.send_request(served.port, {"verb": "cancel", "job": job_id, "auth": served.secret})
    drain(stream)

    assert served.registry.generation_superseded_at(digest) is None
    rows = served.registry.conn.execute(
        "SELECT 1 FROM generations WHERE digest = ?", (digest,)
    ).fetchall()
    assert rows == [], "no generation row for a build that never completed"
    assert served.registry.list_resources() == [], "and no image resource either"


def test_cancelling_an_unknown_job_is_an_error(served: Daemon) -> None:
    reply = ipc.send_request(
        served.port, {"verb": "cancel", "job": "j0-nope", "auth": served.secret}
    )
    assert reply["ok"] is False
    assert "no such job" in reply["error"]


# -- retirement ------------------------------------------------------------


def test_a_running_job_blocks_idle_retirement(
    tmp_path: Path, project: Path, builder: ControlledBuilder
) -> None:
    """Retiring mid-build would destroy the build; the old idle-only rule would have."""
    state_dir = tmp_path / "state2"
    daemon = Daemon(state_dir=state_dir, idle_retire_seconds=0.2)
    daemon.jobs.builder = builder
    thread = threading.Thread(target=daemon.serve_forever, daemon=True)
    thread.start()
    try:
        assert wait_until(lambda: daemon_mod.is_serving(state_dir))
        stream = ipc.stream_request(
            daemon.port,
            {"verb": "converge", "manifest": str(project / "bosn.toml"), "auth": daemon.secret},
            timeout=30,
        )
        next(stream)
        assert builder.started.wait(10)
        stream.close()

        # Note: nothing may poll `is_serving` while waiting on retirement -- a ping is a
        # request, and a request resets the idle clock this test is measuring.
        time.sleep(1.0)  # comfortably past the 0.2s idle window
        assert daemon.should_retire() is False
        assert thread.is_alive(), "the daemon stayed up for its own build"

        builder.release.set()
        thread.join(timeout=20)
        assert not thread.is_alive(), "with the build done, idle retirement proceeds"
    finally:
        builder.release.set()
        daemon.request_stop()
        thread.join(timeout=15)
        daemon.shutdown()


def test_a_job_that_never_reports_cannot_pin_the_daemon_forever(
    tmp_path: Path, project: Path
) -> None:
    """The TTL is what keeps 'running jobs block retirement' from being a deadlock."""
    hung = threading.Event()

    def never_finishes(job: Job) -> BuildOutcome:
        hung.set()
        while not job.cancelled.is_set():
            time.sleep(0.01)
        return BuildOutcome(returncode=130)

    state_dir = tmp_path / "state3"
    daemon = Daemon(state_dir=state_dir, idle_retire_seconds=0.2, build_ttl_seconds=0.5)
    daemon.jobs.builder = never_finishes
    thread = threading.Thread(target=daemon.serve_forever, daemon=True)
    thread.start()
    try:
        assert wait_until(lambda: daemon_mod.is_serving(state_dir))
        stream = ipc.stream_request(
            daemon.port,
            {"verb": "converge", "manifest": str(project / "bosn.toml"), "auth": daemon.secret},
            timeout=30,
        )
        next(stream)
        assert hung.wait(10)
        stream.close()

        # The TTL reaps the hung build, and only then can idle retirement proceed. Waiting
        # on the thread rather than polling `is_serving`, whose ping would reset the clock.
        thread.join(timeout=30)
        assert not thread.is_alive(), "a hung build must not pin the daemon forever"
    finally:
        daemon.request_stop()
        thread.join(timeout=15)
        daemon.shutdown()


def test_shutdown_tells_attached_clients_why_their_build_stopped(
    tmp_path: Path, project: Path, builder: ControlledBuilder
) -> None:
    state_dir = tmp_path / "state4"
    daemon = Daemon(state_dir=state_dir, idle_retire_seconds=3600)
    daemon.jobs.builder = builder
    thread = threading.Thread(target=daemon.serve_forever, daemon=True)
    thread.start()
    assert wait_until(lambda: daemon_mod.is_serving(state_dir))

    stream = ipc.stream_request(
        daemon.port,
        {"verb": "converge", "manifest": str(project / "bosn.toml"), "auth": daemon.secret},
        timeout=30,
    )
    job_id = next(stream)["job"]
    assert builder.started.wait(10)

    daemon.request_stop()
    thread.join(timeout=15)
    daemon.shutdown()

    job = daemon.jobs.get(job_id)
    assert job.state == "cancelled"
    assert job.error is not None and "shutting down" in job.error


# -- reporting -------------------------------------------------------------


def test_jobs_verb_reports_real_jobs_not_a_placeholder(
    served: Daemon, project: Path, builder: ControlledBuilder
) -> None:
    reply = ipc.send_request(served.port, {"verb": "jobs", "auth": served.secret})
    assert reply["jobs"] == []
    assert reply["max_builds"] >= 1

    stream = converge_events(served, project)
    job_id = next(stream)["job"]
    assert builder.started.wait(10)

    listed = ipc.send_request(served.port, {"verb": "jobs", "auth": served.secret})["jobs"]
    assert [row for row in listed if row["id"] == job_id]
    assert listed[0]["stack"] == "dev"
    stream.close()
