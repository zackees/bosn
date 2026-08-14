"""The build-job concurrency policy: coalesce with queue depth 1, joining by digest.

The bug being pinned is the agent edit loop. An agent edits a Dockerfile and re-runs every
30 seconds; each edit is a new digest against the same `(workspace, stack)` key. Identical
requests join and distinct keys parallelize, but *same key, different digest* is neither --
under plain serialization it queues, and the queue has no bound. Every entry but the last
is obsolete on arrival, each pins volumes against GC, and running jobs block the daemon's
idle retirement, so the runaway loop keeps the daemon resident and its resources
uncollectable at once.

`test_unbounded_serialization_is_the_failure_mode` runs that loop against a reference model
of the rejected design and shows the queue growing without bound. Everything after it runs
the same loop against the real JobManager and shows the bound holding.

No Docker here -- the builder is a fake whose completion the test controls.
"""

from __future__ import annotations

import threading
import time
from collections import deque

import pytest

from bosn.jobs import (
    CANCELLED,
    PENDING,
    QUEUED,
    RUNNING,
    SUCCEEDED,
    SUPERSEDED,
    BuildOutcome,
    Job,
    JobError,
    JobManager,
)

WORKSPACE = "/w/one"
STACK = "dev"


def wait_until(predicate, timeout: float = 10.0, interval: float = 0.01) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if predicate():
            return True
        time.sleep(interval)
    return predicate()


class SlowBuilder:
    """A build that does not finish until the test says so."""

    def __init__(self) -> None:
        self.release = threading.Event()
        self.started: list[str] = []
        self.finished: list[str] = []
        self._lock = threading.Lock()

    def __call__(self, job: Job) -> BuildOutcome:
        with self._lock:
            self.started.append(job.digest)
        job.log(f"building {job.digest}")
        while not self.release.is_set():
            if job.cancelled.is_set():
                return BuildOutcome(returncode=130)
            time.sleep(0.005)
        with self._lock:
            self.finished.append(job.digest)
        return BuildOutcome(returncode=0, result={"digest": job.digest})

    def running_count(self) -> int:
        with self._lock:
            return len(self.started) - len(self.finished)


@pytest.fixture
def builder() -> SlowBuilder:
    return SlowBuilder()


@pytest.fixture
def manager(builder: SlowBuilder):
    mgr = JobManager(builder, max_builds=4, ttl_seconds=3600)
    try:
        yield mgr
    finally:
        mgr.shutdown(timeout=5)


def submit(manager: JobManager, digest: str, *, workspace: str = WORKSPACE, stack: str = STACK):
    return manager.submit(workspace=workspace, stack=stack, digest=digest)


# -- RED: the design we rejected -------------------------------------------


class UnboundedSerializer:
    """Option D, the failure mode: same key serializes, and the queue has no bound.

    This is what §13 literally said before this work, modeled just closely enough to show
    what the edit loop does to it. It exists so the bound below is a measured improvement
    over a real alternative rather than an assertion about itself.
    """

    def __init__(self) -> None:
        self.queues: dict[tuple[str, str], deque[str]] = {}

    def submit(self, key: tuple[str, str], digest: str) -> None:
        queue = self.queues.setdefault(key, deque())
        if digest in queue:
            return  # identical in-flight requests join
        queue.append(digest)

    def depth(self, key: tuple[str, str]) -> int:
        return len(self.queues.get(key, ()))


def test_unbounded_serialization_is_the_failure_mode() -> None:
    """The rejected design: 50 edits become 50 queued builds, 49 of them already obsolete."""
    serializer = UnboundedSerializer()
    key = (WORKSPACE, STACK)
    for edit in range(50):
        serializer.submit(key, f"sha256:{edit:064x}")

    assert serializer.depth(key) == 50, "this is the pile-up the policy has to prevent"


# -- GREEN: the bound the policy guarantees --------------------------------


def test_the_edit_loop_never_exceeds_one_running_and_one_pending(
    manager: JobManager, builder: SlowBuilder
) -> None:
    """The same 50 edits, under the real policy. The bound is structural, not tuned."""
    for edit in range(50):
        submit(manager, f"sha256:{edit:064x}")
        assert manager.depth(WORKSPACE, STACK) <= 2, f"queue grew past the bound at edit {edit}"

    assert manager.depth(WORKSPACE, STACK) == 2
    assert builder.running_count() == 1, "exactly one build is burning CPU, not 50"

    builder.release.set()
    assert wait_until(lambda: manager.depth(WORKSPACE, STACK) == 0)
    # One obsolete build plus the newest one -- never the 48 in between.
    assert builder.finished == ["sha256:" + f"{0:064x}", "sha256:" + f"{49:064x}"]


def test_only_the_newest_digest_survives_the_loop(
    manager: JobManager, builder: SlowBuilder
) -> None:
    jobs = [submit(manager, f"sha256:{edit:064x}").job for edit in range(10)]
    builder.release.set()
    assert wait_until(lambda: all(j.finished for j in jobs))

    survivors = [j for j in jobs if j.state == SUCCEEDED]
    digests = {j.digest for j in survivors}
    assert f"sha256:{9:064x}" in digests, "the newest request must be the one that converges"


def test_every_dropped_request_is_told_it_was_superseded(
    manager: JobManager, builder: SlowBuilder
) -> None:
    """No silent no-ops: a dropped request ends in a terminal state naming what happened."""
    first = submit(manager, "sha256:aaa").job  # starts building
    dropped = submit(manager, "sha256:bbb").job  # takes the pending slot
    newest = submit(manager, "sha256:ccc").job  # replaces it

    assert dropped.state == SUPERSEDED
    assert dropped.error is not None
    assert "superseded" in dropped.error
    assert "aaa"[:12] in dropped.error or "bbb" in dropped.error
    assert "ccc" in dropped.error, "the message must name the digest that replaced it"
    assert dropped.done.is_set(), "a superseded job must not leave its client hanging"
    assert first.state == RUNNING
    assert newest.state == PENDING


def test_a_superseded_job_notifies_the_client_watching_it(
    manager: JobManager, builder: SlowBuilder
) -> None:
    submit(manager, "sha256:aaa")
    dropped = submit(manager, "sha256:bbb").job
    watcher = dropped.watch()  # a client is attached, waiting for its build to start

    submit(manager, "sha256:ccc")  # ...and a newer edit takes the slot out from under it

    event = watcher.get(timeout=5)
    assert event["final"] is True
    assert event["state"] == SUPERSEDED
    assert "superseded" in event["error"]


def test_identical_digests_join_instead_of_building_twice(
    manager: JobManager, builder: SlowBuilder
) -> None:
    first = submit(manager, "sha256:same")
    second = submit(manager, "sha256:same")
    third = submit(manager, "sha256:same")

    assert second.joined and third.joined
    assert second.job is first.job is third.job
    assert manager.depth(WORKSPACE, STACK) == 1
    assert builder.started == ["sha256:same"], "one build, three watchers"


def test_a_second_identical_request_joins_the_pending_slot(
    manager: JobManager, builder: SlowBuilder
) -> None:
    """Joining applies to the queued request too, not just the running one."""
    submit(manager, "sha256:running")
    pending = submit(manager, "sha256:next").job
    again = submit(manager, "sha256:next")

    assert again.joined and again.job is pending
    assert manager.depth(WORKSPACE, STACK) == 2


def test_the_pending_job_starts_when_the_active_one_finishes(
    manager: JobManager, builder: SlowBuilder
) -> None:
    active = submit(manager, "sha256:first").job
    pending = submit(manager, "sha256:second").job
    assert pending.state == PENDING

    builder.release.set()
    assert wait_until(lambda: active.finished and pending.finished)
    assert pending.state == SUCCEEDED
    assert builder.started == ["sha256:first", "sha256:second"]


# -- parallelism across keys -----------------------------------------------


def test_distinct_keys_build_in_parallel(manager: JobManager, builder: SlowBuilder) -> None:
    """A slow build in worktree A must never block worktree B."""
    a = submit(manager, "sha256:a", workspace="/w/a").job
    b = submit(manager, "sha256:b", workspace="/w/b").job

    assert wait_until(lambda: a.state == RUNNING and b.state == RUNNING)
    assert builder.running_count() == 2, "neither worktree waits on the other"


def test_the_same_workspace_with_different_stacks_also_parallelizes(
    manager: JobManager, builder: SlowBuilder
) -> None:
    a = submit(manager, "sha256:a", stack="dev").job
    b = submit(manager, "sha256:b", stack="ci").job
    assert wait_until(lambda: a.state == RUNNING and b.state == RUNNING)


def test_the_machine_wide_cap_bounds_total_concurrency(builder: SlowBuilder) -> None:
    """Per-key serialization does not bound host load; N worktrees still can. This does."""
    manager = JobManager(builder, max_builds=2, ttl_seconds=3600)
    try:
        jobs = [
            manager.submit(workspace=f"/w/{i}", stack=STACK, digest=f"sha256:{i}").job
            for i in range(6)
        ]
        assert wait_until(lambda: builder.running_count() == 2)
        time.sleep(0.1)
        assert builder.running_count() == 2, "the cap must hold, not just start out right"
        assert sum(1 for j in jobs if j.state == QUEUED) == 4

        builder.release.set()
        assert wait_until(lambda: all(j.finished for j in jobs), timeout=15)
        assert all(j.state == SUCCEEDED for j in jobs), "capped jobs run later, not never"
    finally:
        manager.shutdown(timeout=5)


# -- cancellation ----------------------------------------------------------


def test_cancelling_a_running_build_stops_it(manager: JobManager, builder: SlowBuilder) -> None:
    job = submit(manager, "sha256:slow").job
    assert wait_until(lambda: job.state == RUNNING)

    manager.cancel(job.id)
    assert wait_until(lambda: job.finished)
    assert job.state == CANCELLED
    assert job.error is not None


def test_cancelling_a_pending_job_never_starts_it(
    manager: JobManager, builder: SlowBuilder
) -> None:
    submit(manager, "sha256:running")
    pending = submit(manager, "sha256:pending").job

    manager.cancel(pending.id)
    assert pending.state == CANCELLED
    assert manager.depth(WORKSPACE, STACK) == 1
    assert "sha256:pending" not in builder.started


def test_cancelling_the_active_job_promotes_the_pending_one(
    manager: JobManager, builder: SlowBuilder
) -> None:
    active = submit(manager, "sha256:active").job
    pending = submit(manager, "sha256:pending").job
    assert wait_until(lambda: active.state == RUNNING)

    manager.cancel(active.id)
    assert wait_until(lambda: pending.state == RUNNING)


def test_cancelling_an_unknown_job_is_an_error_not_a_no_op(manager: JobManager) -> None:
    with pytest.raises(JobError, match="no such job"):
        manager.cancel("j999-nope")


def test_cancelling_a_finished_job_says_so(manager: JobManager, builder: SlowBuilder) -> None:
    job = submit(manager, "sha256:x").job
    builder.release.set()
    assert wait_until(lambda: job.finished)
    with pytest.raises(JobError, match="already finished"):
        manager.cancel(job.id)


# -- liveness --------------------------------------------------------------


def test_a_hung_build_is_reaped_by_its_ttl(builder: SlowBuilder) -> None:
    """A job that never reports would otherwise pin the daemon forever."""
    manager = JobManager(builder, max_builds=2, ttl_seconds=0.2)
    try:
        job = manager.submit(workspace=WORKSPACE, stack=STACK, digest="sha256:hung").job
        assert wait_until(lambda: job.state == RUNNING)
        time.sleep(0.3)

        assert manager.reap_expired() == [job]
        assert wait_until(lambda: job.finished)
        assert job.state == CANCELLED
        assert job.error is not None and "TTL" in job.error
        assert manager.active_count() == 0, "the daemon can retire again"
    finally:
        manager.shutdown(timeout=5)


def test_a_healthy_build_is_not_reaped(manager: JobManager, builder: SlowBuilder) -> None:
    job = submit(manager, "sha256:fine").job
    assert wait_until(lambda: job.state == RUNNING)
    assert manager.reap_expired() == []
    assert job.state == RUNNING


def test_active_count_tracks_work_the_daemon_must_not_abandon(
    manager: JobManager, builder: SlowBuilder
) -> None:
    assert manager.active_count() == 0
    submit(manager, "sha256:one")
    submit(manager, "sha256:two")
    assert manager.active_count() == 2

    builder.release.set()
    assert wait_until(lambda: manager.active_count() == 0)


def test_shutdown_cancels_in_flight_work_visibly(builder: SlowBuilder) -> None:
    manager = JobManager(builder, max_builds=2, ttl_seconds=3600)
    job = manager.submit(workspace=WORKSPACE, stack=STACK, digest="sha256:x").job
    assert wait_until(lambda: job.state == RUNNING)

    manager.shutdown(timeout=5)
    assert job.state == CANCELLED
    assert job.error is not None and "shutting down" in job.error
    with pytest.raises(JobError, match="shutting down"):
        manager.submit(workspace=WORKSPACE, stack=STACK, digest="sha256:y")


# -- watching --------------------------------------------------------------


def test_a_late_watcher_still_sees_the_output_it_missed(
    manager: JobManager, builder: SlowBuilder
) -> None:
    """This is what makes reattaching after a killed CLI useful rather than a blank screen."""
    job = submit(manager, "sha256:noisy").job
    assert wait_until(lambda: any(True for _ in [job]) and job.state == RUNNING)
    assert wait_until(lambda: len(job._log) > 0)

    watcher = job.watch()
    first = watcher.get(timeout=5)
    assert first["event"] == "log"
    assert "building" in first["line"]


def test_watching_a_finished_job_yields_its_terminal_event(
    manager: JobManager, builder: SlowBuilder
) -> None:
    job = submit(manager, "sha256:done").job
    builder.release.set()
    assert wait_until(lambda: job.finished)

    events = []
    watcher = job.watch()
    while not watcher.empty():
        events.append(watcher.get_nowait())
    assert events[-1]["final"] is True
    assert events[-1]["state"] == SUCCEEDED


def test_follow_ends_at_the_terminal_event(manager: JobManager, builder: SlowBuilder) -> None:
    job = submit(manager, "sha256:follow").job
    builder.release.set()
    seen = [event for event in manager.follow(job, heartbeat=5.0)]
    assert seen[-1]["final"] is True
    assert seen[-1]["state"] == SUCCEEDED
    assert seen[-1]["result"] == {"digest": "sha256:follow"}


def test_follow_heartbeats_so_a_quiet_build_is_not_mistaken_for_a_dead_daemon(
    manager: JobManager, builder: SlowBuilder
) -> None:
    job = submit(manager, "sha256:quiet").job
    events = manager.follow(job, heartbeat=0.05)
    seen = []
    for event in events:
        seen.append(event)
        if len(seen) >= 3:
            break
    assert any(e["event"] == "heartbeat" for e in seen)


# -- regressions -----------------------------------------------------------


def test_the_terminal_transition_is_atomic_against_watch(manager: JobManager) -> None:
    """A job must never be observable as "terminal but not yet filled in".

    `watch()` branches on the job's state: terminal means "hand back the summary and do not
    register". So if the transition set the state first and the fields after, a `watch()`
    landing between them would synthesize a summary with no returncode, no error and an
    empty result -- and, having taken the terminal branch, would never be registered for
    the real event. The client is told its build succeeded and handed an empty
    ConvergeResult, which downstream becomes `docker run` against an empty image tag.

    Racing that window directly is hopeless -- it is a handful of statements wide, and an
    earlier version of this test ran 200 attempts without once landing in it. So this tests
    the property instead: hold the lock `watch()` uses, and no part of the transition may
    become visible while it is held.
    """
    job = submit(manager, "sha256:atomic").job
    assert wait_until(lambda: job.state == RUNNING)

    finalized = threading.Thread(
        target=job.finalize,
        args=(SUCCEEDED,),
        kwargs={"error": None, "returncode": 0, "result": {"digest": "sha256:atomic"}},
        daemon=True,
    )
    with job._lock:
        finalized.start()
        time.sleep(0.1)  # ample time for an unsynchronized write to land
        assert job.state == RUNNING, "state changed while the watch() lock was held"
        assert job.result == {}, "fields were written outside the lock"

    finalized.join(timeout=5)
    assert job.state == SUCCEEDED
    assert job.result == {"digest": "sha256:atomic"}
    assert job.returncode == 0


def test_a_watcher_registered_before_the_job_settles_gets_the_complete_event(
    manager: JobManager, builder: SlowBuilder
) -> None:
    """And the other ordering: a watcher already attached receives the full terminal event."""
    job = submit(manager, "sha256:watched").job
    assert wait_until(lambda: job.state == RUNNING)
    watcher = job.watch()

    builder.release.set()
    assert wait_until(lambda: job.finished)

    events = []
    while True:
        event = watcher.get(timeout=5)
        events.append(event)
        if event.get("final"):
            break
    assert events[-1]["state"] == SUCCEEDED
    assert events[-1]["result"] == {"digest": "sha256:watched"}
    assert events[-1]["returncode"] == 0


def test_a_watcher_arriving_after_the_job_settles_gets_the_complete_event(
    manager: JobManager, builder: SlowBuilder
) -> None:
    """...as does one that shows up late, which is what `bosn attach` does."""
    job = submit(manager, "sha256:late").job
    builder.release.set()
    assert wait_until(lambda: job.finished)

    watcher = job.watch()
    events = []
    while not watcher.empty():
        events.append(watcher.get_nowait())
    assert events[-1]["final"] is True
    assert events[-1]["result"] == {"digest": "sha256:late"}
    assert events[-1]["returncode"] == 0


def test_a_new_request_does_not_join_a_job_that_is_already_being_cancelled(
    manager: JobManager, builder: SlowBuilder
) -> None:
    """A cancelled build stays RUNNING until its builder tears down.

    Joining it would hand the newcomer the corpse's CANCELLED event -- so `bosn cancel`
    followed by `bosn run` would build nothing and exit 5, and after a TTL reap of a build
    that ignores the kill, every subsequent run for that digest would do the same.
    """
    doomed = submit(manager, "sha256:doomed").job
    assert wait_until(lambda: doomed.state == RUNNING)

    doomed.cancelled.set()  # cancelled, but the builder has not noticed yet
    assert doomed.state == RUNNING

    again = submit(manager, "sha256:doomed")
    assert not again.joined, "a dying job must not be joined"
    assert again.job is not doomed
    assert again.job.state == PENDING

    builder.release.set()
    assert wait_until(lambda: again.job.finished)
    assert again.job.state == SUCCEEDED, "the newcomer got its own real build"


def test_the_ttl_reaper_does_not_re_reap_a_build_that_is_still_dying(
    builder: SlowBuilder,
) -> None:
    """reap_expired runs twice a second; a cancelled build can take tens of seconds to go."""
    manager = JobManager(builder, max_builds=2, ttl_seconds=0.1)
    job = manager.submit(workspace=WORKSPACE, stack=STACK, digest="sha256:wedged").job
    try:
        assert wait_until(lambda: job.state == RUNNING)
        time.sleep(0.2)

        assert manager.reap_expired() == [job], "the first pass reaps it"
        job.state = RUNNING  # simulate a builder that has not torn down yet
        assert manager.reap_expired() == [], "later passes must not reap it again"
    finally:
        job.state = CANCELLED
        manager.shutdown(timeout=5)


# -- reporting -------------------------------------------------------------


def test_list_jobs_reports_state_for_every_job(manager: JobManager, builder: SlowBuilder) -> None:
    running = submit(manager, "sha256:r").job
    pending = submit(manager, "sha256:p").job
    listed = {row["id"]: row for row in manager.list_jobs()}

    assert listed[running.id]["state"] == RUNNING
    assert listed[pending.id]["state"] == PENDING
    assert listed[running.id]["stack"] == STACK
    assert listed[running.id]["workspace"] == WORKSPACE


def test_finished_jobs_do_not_accumulate_forever(builder: SlowBuilder) -> None:
    """An agent loop keeps the daemon resident, so unbounded job history is a real leak."""
    manager = JobManager(builder, max_builds=4, ttl_seconds=3600, max_history=5)
    builder.release.set()
    try:
        jobs = []
        for edit in range(20):
            job = manager.submit(workspace=WORKSPACE, stack=STACK, digest=f"sha256:{edit:064x}").job
            jobs.append(job)
            assert wait_until(lambda j=job: j.finished or j.state == PENDING)
        assert wait_until(lambda: manager.active_count() == 0)

        assert len(manager.list_jobs()) <= 5, "old jobs must be forgotten"
        newest = manager.list_jobs()[-1]
        assert newest["id"] == jobs[-1].id, "the most recent job is the one kept"
    finally:
        manager.shutdown(timeout=5)


def test_pruning_never_forgets_a_running_job(builder: SlowBuilder) -> None:
    """Forgetting live work would lose the daemon's own build."""
    manager = JobManager(builder, max_builds=4, ttl_seconds=3600, max_history=1)
    try:
        live = manager.submit(workspace="/w/live", stack=STACK, digest="sha256:live").job
        assert wait_until(lambda: live.state == RUNNING)

        for edit in range(5):
            other = manager.submit(workspace=f"/w/{edit}", stack=STACK, digest=f"sha256:{edit}").job
            manager.cancel(other.id)

        assert manager.get(live.id) is live, "the running job survived every prune"
        assert live.state == RUNNING
    finally:
        manager.shutdown(timeout=5)


def test_a_builder_that_raises_fails_its_job_and_not_the_daemon() -> None:
    def explode(job: Job) -> BuildOutcome:
        raise RuntimeError("builder blew up")

    manager = JobManager(explode, max_builds=2, ttl_seconds=3600)
    try:
        job = manager.submit(workspace=WORKSPACE, stack=STACK, digest="sha256:boom").job
        assert wait_until(lambda: job.finished)
        assert job.state == "failed"
        assert job.error is not None and "builder blew up" in job.error

        later = manager.submit(workspace=WORKSPACE, stack=STACK, digest="sha256:after").job
        assert wait_until(lambda: later.finished), "the manager keeps serving after a crash"
    finally:
        manager.shutdown(timeout=5)
