"""Daemon-owned build jobs end to end: streaming, attach, cancel, and retirement.

These exercise the real daemon over the real loopback protocol with a fake builder, so the
streaming transport and the job policy are tested together without needing Docker. The
`docker` marker covers the same ground against a real build in `test_scenario_docker.py`.
"""

from __future__ import annotations

import sqlite3
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


# 60s, not 15s, and this is a *budget* increase rather than the kind of timeout-padding
# that hides a race -- the two are worth telling apart, since the rest of issue #95 was
# fixed by removing a race, not by waiting longer.
#
# The predicate these helpers most often carry is `daemon_mod.is_serving(state_dir)`, which
# is an IPC ping with its own `timeout=2.0` (see daemon.py). Under sustained CPU
# oversubscription a loopback round trip can miss that 2s window even though the daemon is
# serving perfectly well, and each miss burns the full 2s -- so a 15s ceiling buys only
# about seven attempts, and a run where the machine is busy for a couple of seconds fails
# on attempt count rather than on anything being wrong. Measured, not guessed: with 12 busy
# loops saturating this 16-core box, daemon startup blew the 15s ceiling on 1 of 5 runs.
#
# Raising the ceiling costs a healthy run exactly nothing: this polls and returns the
# instant the predicate is true, so the larger number is only ever reached by a run that
# was going to fail anyway -- it changes how long a genuine hang takes to report, not how
# long a passing test takes. A real hang still fails, just with more evidence that it is
# real.
_WAIT_TIMEOUT = 60.0


def wait_until(predicate, timeout: float = _WAIT_TIMEOUT, interval: float = 0.02) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if predicate():
            return True
        time.sleep(interval)
    return predicate()


def _event_kinds(daemon: Daemon, limit: int = 500) -> list[str]:
    """Read the daemon's own event log without going anywhere near IPC.

    `daemon.registry.events()` is a plain SELECT against the SQLite connection the daemon
    already holds open in-process (the connection is opened `check_same_thread=False`
    specifically so callers other than the daemon's own threads can read it). It never
    touches `note_activity`/`last_activity` -- unlike `daemon_mod.is_serving(state_dir)`,
    which is a real IPC ping and would reset the very idle clock this module's tests are
    waiting on. That is why this, and not `is_serving`, is the safe way to watch a
    self-retiring daemon from the outside.

    `shutdown()` closes the registry before the serve thread actually exits, so there is a
    real window where the daemon is still alive-by-`thread.is_alive()` but the connection
    underneath has just been closed by the same shutdown in progress. That is expected,
    not a bug to chase: treat it as "no new event observed yet" rather than letting the
    close race fail the read.
    """
    try:
        return [row["kind"] for row in daemon.registry.events(limit=limit)]
    except KeyboardInterrupt:
        raise
    except sqlite3.Error:
        return []


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
    # Keep transport/job-policy tests engine-free. Image-resolution behavior has focused
    # fake-engine and Docker-backed coverage in the converge suites.
    (root / "Dockerfile").write_text("FROM scratch\n", encoding="utf-8")
    (root / "bosn.toml").write_text(SAMPLE, encoding="utf-8")
    return root


@pytest.fixture
def builder() -> ControlledBuilder:
    return ControlledBuilder()


@pytest.fixture
def served(tmp_path: Path, builder: ControlledBuilder) -> Iterator[Daemon]:
    state_dir = tmp_path / "state"
    daemon = Daemon(state_dir=state_dir, idle_retire_seconds=3600)
    # A fresh registry has no stored maintenance deadline, so `_next_maintenance_at`
    # defaults to `started_at` -- due on the watchdog's very first tick. If teardown's
    # `request_stop()` lands while that tick is mid-`_run_maintenance()`, `shutdown()`'s
    # unconditional `watchdog.join()` (no timeout, see daemon.py) blocks until the
    # maintenance pass finishes -- including its engine-reachability probe, which is
    # 60s per call and is called twice on an engine-less/unreachable runner (see
    # test_daemon.py::test_idle_retirement_stops_an_unused_daemon). That race is what
    # made teardown occasionally blow the `thread.join(timeout=15)` below under load:
    # nothing here needs a real maintenance pass, so it is pushed out of the way.
    daemon._set_next_maintenance(daemon.clock.now() + 3600)
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


def test_custom_manifest_source_survives_daemon_queue_to_worker_payload(
    served: Daemon, project: Path, builder: ControlledBuilder
) -> None:
    """A custom source filename is not rewritten to a decoy `bosn.toml` (#126)."""
    custom = project / "development.toml"
    custom.write_text(
        "[stack.chosen]\ndockerfile = 'Dockerfile'\ndefault = true\n", encoding="utf-8"
    )
    # Before #126, `_verb_converge` loaded `development.toml` correctly, then stored
    # `<root>/bosn.toml` in the queued job. The worker would consequently build this
    # different declaration later.
    (project / "bosn.toml").write_text(
        "[stack.decoy]\ndockerfile = 'Dockerfile'\ndefault = true\n", encoding="utf-8"
    )
    stream = ipc.stream_request(
        served.port,
        {"verb": "converge", "manifest": str(custom), "auth": served.secret},
        timeout=30,
    )
    submitted = next(stream)
    job = served.jobs.get(submitted["job"])

    assert job.stack == "chosen"
    assert job.payload["manifest"] == str(custom.resolve())
    # `_build` consumes this durable job payload, so pinning it is the client -> daemon
    # queue -> worker source-path contract without needing a Docker build.
    builder.release.set()
    assert [event for event in stream if event.get("final")][-1]["state"] == "succeeded"


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


def test_image_resolved_digest_is_the_job_coalescing_key(
    served: Daemon,
    project: Path,
    builder: ControlledBuilder,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    from bosn import converge as converge_mod

    current = ["sha256:resolved-one"]

    def fake_coalescing_key(*_args, **_kwargs):
        return current[0]

    monkeypatch.setattr(converge_mod, "generation_coalescing_key", fake_coalescing_key)
    first_stream = converge_events(served, project)
    first = next(first_stream)
    assert builder.started.wait(10)

    current[0] = "sha256:resolved-two"
    second_stream = converge_events(served, project)
    second = next(second_stream)

    assert first["coalescing_key"] == "sha256:resolved-one"
    assert second["coalescing_key"] == "sha256:resolved-two"
    assert second["joined"] is False

    builder.release.set()
    drain(first_stream)
    drain(second_stream)


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

    assert (
        served.registry.generation_superseded_at(
            digest,
            stack=manifest.stack(None).name,
            workspace=str(manifest.root),
        )
        is None
    )
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
    # idle_retire_seconds starts long, not the 0.2s this test is actually about. Arming the
    # short window at construction time races the daemon's own watchdog (0.5s tick) against
    # nothing more than "the test thread gets scheduled and lands one IPC round trip" --
    # under load, that can lose: the daemon retires before anyone ever contacts it, and
    # every later `is_serving` probe then fails forever because the thing being pinged is
    # already gone (issue #95, confirmed by 4/40 trials under load logging
    # thread_alive=False and a since-closed registry before the first successful ping).
    # The short window is armed below, once the test has established the exact state it
    # means to measure -- so the daemon is definitely up and definitely mid-build before
    # idle retirement is even a possibility.
    daemon = Daemon(state_dir=state_dir, idle_retire_seconds=3600)
    # See test_daemon.py::test_idle_retirement_stops_an_unused_daemon: a due maintenance
    # pass probes the engine twice at 60s each inside the same watchdog tick that checks
    # retirement, which on an engine-less runner outlasts the join deadline below.
    daemon._set_next_maintenance(daemon.clock.now() + 3600)
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

        # Now that the build is confirmed running, arm the short idle window this test is
        # actually about. `idle_retire_seconds` is a plain attribute (daemon.py); the
        # watchdog reads it fresh on every tick, so this takes effect on the very next one.
        daemon.idle_retire_seconds = 0.2

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
    # idle_retire_seconds starts long: arming 0.2s at construction races the watchdog's
    # first tick against nothing more than "the test thread gets scheduled and lands one
    # IPC round trip", and under load that race can be lost before the test ever contacts
    # the daemon -- see test_a_running_job_blocks_idle_retirement above for the same fix
    # and the 4/40-under-load confirmation that this is real, not hypothetical. The short
    # window is armed below only once the build is confirmed hung, which is the state this
    # test is actually about. `build_ttl_seconds` stays fixed at construction: the TTL
    # clock doesn't start until a job exists, so it isn't exposed to the same startup race.
    daemon = Daemon(state_dir=state_dir, idle_retire_seconds=3600, build_ttl_seconds=0.5)
    # See test_a_running_job_blocks_idle_retirement above: a fresh registry has no stored
    # maintenance deadline, so a maintenance pass is due on the very first watchdog tick --
    # the same tick that has to notice the TTL expiry below. On a runner where the engine
    # binary is present but its daemon is unreachable, that pass blocks on a 60s-per-call
    # reachability probe (called twice), which starves the reap/retire check this test is
    # timing and was the actual cause of this test's flakiness, not scheduler jitter on the
    # 0.2s/0.5s windows themselves. Pushed out of the way so only the TTL reap is measured.
    daemon._set_next_maintenance(daemon.clock.now() + 3600)
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

        # Now that the build is confirmed hung, arm the short idle window this test is
        # actually about. `idle_retire_seconds` is a plain attribute (daemon.py); the
        # watchdog reads it fresh on every tick, so this takes effect on the very next one.
        daemon.idle_retire_seconds = 0.2

        # The TTL reaps the hung build, and only then can idle retirement proceed. Rather
        # than betting everything on one opaque `thread.join(timeout=30)` and only finding
        # out afterward whether the daemon ever noticed, wait on the state the daemon
        # itself publishes when it retires: `daemon.idle_retired`, logged right after
        # `request_stop()` succeeds in `_idle_watchdog` (see daemon.py). That event is
        # read straight off the daemon's own Registry connection (see `_event_kinds`
        # above), never through IPC, so unlike polling `is_serving` it cannot reset
        # `last_activity` -- the very idle clock this test is waiting on.
        #
        # Once that event exists the daemon has already committed to stopping; the
        # remaining `thread.join` only has to cover the mechanical tail of
        # `_server.serve_forever()` returning and `shutdown()` running, so it gets a much
        # smaller budget than the wait for retirement itself.
        retired = wait_until(
            lambda: "daemon.idle_retired" in _event_kinds(daemon) or not thread.is_alive(),
            timeout=30,
        )
        assert retired, (
            "the daemon never logged daemon.idle_retired; recent events: "
            f"{_event_kinds(daemon)[:20]}"
        )
        thread.join(timeout=15)
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
    # Prophylactic, not observed failing: same `request_stop()` + untimed-`watchdog.join()`
    # race as the `served` fixture above -- see its comment. Pushed out so shutdown here
    # cannot stall on a same-tick maintenance pass.
    daemon._set_next_maintenance(daemon.clock.now() + 3600)
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
