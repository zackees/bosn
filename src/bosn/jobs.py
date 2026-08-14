"""Daemon-owned build jobs and the concurrency policy that bounds them.

A build moved behind the daemon survives the CLI that asked for it -- which is the whole
point, since a killed CLI must not destroy a 20-minute build. But it also means the daemon
now owns work that outlives its requester, and *something* has to bound how much of it
accumulates. bosn's primary consumer is an agent in an edit-and-rerun loop, so the bound is
not a nicety.

**Policy: coalesce with queue depth 1, joining by digest.**

Per `(workspace-id, stack)` key the daemon holds at most one active job and one pending
job:

1. Nothing active for this key -> the request becomes active and builds.
2. Active job has the *same* digest -> the request joins it. One build, many watchers.
3. Active job has a *different* digest -> the request takes the pending slot. If that slot
   is already occupied, an identical digest joins it, and a different digest replaces its
   occupant, which terminates as SUPERSEDED and tells its clients so.

Why this and not the alternatives:

- *Cancel-and-replace* (kill the in-flight build, newest always wins) is more responsive
  but can livelock: under a fast edit loop every build is cancelled before finishing, so
  nothing ever completes and the agent never gets output. Avoiding that needs age floors
  or debounce -- machinery whose tuning is another thing to get wrong. Depth-1 coalescing
  is bounded and livelock-free *by construction*: the worst case is waiting out one
  obsolete build, and that build warms BuildKit's layer cache for the one that follows it.
- *Fail fast* ("a build is already running, stop it first") is simplest for a human at a
  terminal and hostile to the loop this project exists to serve. An error whose remedy is
  always the same mechanical command is just a forced retry loop.
- *Unbounded serialization* is the failure mode itself: a queue of obsolete builds, each
  pinning volumes against GC and blocking the daemon's idle retirement.

The consequence worth naming: superseding only ever drops a job that has **not started**,
so cancellation is never on the hot path. Cancelling a *running* build stays a real,
deliberate act -- `bosn cancel`, daemon shutdown, or the per-job TTL -- and never something
the policy does behind your back.

Nothing here silently does nothing. Every job that is superseded, cancelled, or dropped
terminates with a reason its clients can read.
"""

from __future__ import annotations

import itertools
import queue
import threading
import time
import uuid
from collections import deque
from collections.abc import Callable, Iterator
from dataclasses import dataclass, field
from typing import Any

# -- job states -------------------------------------------------------------

PENDING = "pending"  # holds the key's depth-1 pending slot, waiting on the active job
QUEUED = "queued"  # owns the key, waiting on the machine-wide build cap
RUNNING = "running"  # the builder is executing
SUCCEEDED = "succeeded"
FAILED = "failed"
CANCELLED = "cancelled"
SUPERSEDED = "superseded"

TERMINAL_STATES = frozenset({SUCCEEDED, FAILED, CANCELLED, SUPERSEDED})
ACTIVE_STATES = frozenset({PENDING, QUEUED, RUNNING})

DEFAULT_TTL_SECONDS = 3600.0
# Keep the tail of a job's output for late attachers without letting a chatty build grow
# the daemon's heap without bound.
MAX_LOG_LINES = 5000
# How many finished jobs stay listable and attachable. Bounding this matters for the same
# reason the queue is bounded: an agent loop submits continuously and keeps the daemon
# resident while it does, so "remember every job forever" is a leak that only shows up
# under exactly the workload this design is for. Older jobs are forgotten, not their work.
MAX_FINISHED_JOBS = 50


def default_max_builds() -> int:
    """How many builds may run at once across every key on this machine.

    Per-key serialization alone does not bound host load: N worktrees each building their
    own distinct key are all individually legal and can still saturate the machine. #1 caps
    bytes; this is the CPU-side analogue of the same commitment.
    """
    from bosn.config import load

    return int(load().get("max_builds"))


def default_ttl_seconds() -> float:
    from bosn.config import load

    return load().get("build_ttl_seconds")


class JobError(RuntimeError):
    """A job could not be submitted, found, or cancelled."""


@dataclass(frozen=True)
class BuildOutcome:
    """What a builder returns: an exit code plus whatever the verb wants to report back."""

    returncode: int
    result: dict[str, Any] = field(default_factory=dict)


# A builder runs one job to completion. It must poll `job.cancelled` and stop when set.
Builder = Callable[["Job"], BuildOutcome]

_ids = itertools.count(1)


class Job:
    """One unit of daemon-owned work, plus the fan-out to everyone watching it."""

    def __init__(
        self,
        *,
        workspace: str,
        stack: str,
        digest: str,
        payload: dict[str, Any],
        state: str,
        now: float,
    ) -> None:
        self.id = f"j{next(_ids)}-{uuid.uuid4().hex[:8]}"
        self.workspace = workspace
        self.stack = stack
        self.digest = digest
        self.payload = payload
        self.state = state
        self.created_at = now
        self.started_at: float | None = None
        self.last_progress_at: float | None = None
        self.ended_at: float | None = None
        self.returncode: int | None = None
        self.error: str | None = None
        self.result: dict[str, Any] = {}

        self.cancelled = threading.Event()
        self.done = threading.Event()
        self._lock = threading.Lock()
        self._log: deque[dict[str, Any]] = deque(maxlen=MAX_LOG_LINES)
        self._dropped = 0
        self._watchers: list[queue.SimpleQueue[dict[str, Any]]] = []

    @property
    def key(self) -> tuple[str, str]:
        return (self.workspace, self.stack)

    @property
    def finished(self) -> bool:
        return self.state in TERMINAL_STATES

    # -- output fan-out ----------------------------------------------------

    def emit(self, event: dict[str, Any]) -> None:
        """Publish one event to every watcher, and remember it for late arrivals."""
        with self._lock:
            if event.get("event") == "log":
                if len(self._log) == self._log.maxlen:
                    self._dropped += 1
                self._log.append(event)
            watchers = list(self._watchers)
        for watcher in watchers:
            watcher.put(event)

    def watch(self) -> queue.SimpleQueue[dict[str, Any]]:
        """Subscribe, receiving the output so far before anything new.

        The backlog is what makes reattaching useful: a CLI that died mid-build and came
        back still sees what it missed rather than joining a silent stream.
        """
        watcher: queue.SimpleQueue[dict[str, Any]] = queue.SimpleQueue()
        with self._lock:
            if self._dropped:
                watcher.put(
                    {
                        "event": "log",
                        "stream": "meta",
                        "line": f"[{self._dropped} earlier line(s) dropped from the buffer]",
                    }
                )
            for event in self._log:
                watcher.put(event)
            if self.state in TERMINAL_STATES:
                watcher.put(self.summary(final=True))
            else:
                if self.cancelled.is_set():
                    watcher.put(
                        {"event": "cancelling", "job": self.id, "reason": self.error or "cancelled"}
                    )
                self._watchers.append(watcher)
        return watcher

    def unwatch(self, watcher: queue.SimpleQueue[dict[str, Any]]) -> None:
        with self._lock:
            if watcher in self._watchers:
                self._watchers.remove(watcher)

    def finalize(
        self,
        state: str,
        *,
        error: str | None,
        returncode: int | None,
        result: dict[str, Any] | None,
    ) -> bool:
        """Move to a terminal state and notify, as one atomic step. True if this call did it.

        The atomicity is the point, and it is subtle. `watch()` decides whether to register
        a watcher or hand back the terminal event directly, based on `self.state` -- so if
        the terminal transition were spread over several statements, a `watch()` landing in
        the middle would see the terminal state, synthesize an event from fields not yet
        written (no returncode, no error, empty result), and skip registering, so the real
        event never reached it. A client hitting that window would be told its build
        succeeded and handed an empty ConvergeResult, i.e. an empty image tag.

        Everything therefore happens under the same lock `watch()` takes, which leaves only
        two orderings: watch-then-finalize (registered, gets the real event) and
        finalize-then-watch (sees a fully written job and gets a complete summary).
        """
        with self._lock:
            if self.state in TERMINAL_STATES:
                return False
            self.state = state
            self.ended_at = time.time()
            self.error = error
            self.returncode = returncode
            if result:
                self.result = result
            self.done.set()
            event = self.summary(final=True)
            watchers = list(self._watchers)
            self._watchers.clear()
        # Outside the lock only because it need not be inside; SimpleQueue.put is unbounded
        # and never blocks, so holding it would also have been safe.
        for watcher in watchers:
            watcher.put(event)
        return True

    def log(self, line: str, stream: str = "stdout") -> None:
        # Builder output is the liveness signal. Watcher heartbeats deliberately never
        # call this: a disconnected or chatty client must not keep a wedged build alive.
        with self._lock:
            self.last_progress_at = time.time()
        self.emit({"event": "log", "stream": stream, "line": line})

    def summary(self, *, final: bool = False) -> dict[str, Any]:
        return {
            "event": "end" if final else "status",
            "final": final,
            "job": self.id,
            "state": self.state,
            "workspace": self.workspace,
            "stack": self.stack,
            "coalescing_key": self.digest,
            "created_at": self.created_at,
            "started_at": self.started_at,
            "ended_at": self.ended_at,
            "returncode": self.returncode,
            "error": self.error,
            "result": self.result,
        }


@dataclass
class _Slot:
    """The whole bound, in two fields: one active job, one pending job."""

    active: Job | None = None
    pending: Job | None = None

    def empty(self) -> bool:
        return self.active is None and self.pending is None


@dataclass(frozen=True)
class Submission:
    """What happened to a submit() -- the caller needs to be able to say which."""

    job: Job
    joined: bool
    superseded: Job | None = None

    @property
    def disposition(self) -> str:
        if self.joined:
            return "joined"
        return self.job.state


class JobManager:
    """The job table: the policy above, plus the threads that carry it out."""

    def __init__(
        self,
        builder: Builder,
        *,
        max_builds: int | None = None,
        ttl_seconds: float | None = None,
        max_history: int = MAX_FINISHED_JOBS,
        on_settled: Callable[[], None] | None = None,
    ) -> None:
        self.builder = builder
        self.max_builds = max_builds if max_builds is not None else default_max_builds()
        self.ttl_seconds = ttl_seconds if ttl_seconds is not None else default_ttl_seconds()
        self.max_history = max_history
        self.on_settled = on_settled

        self._lock = threading.RLock()
        self._slots: dict[tuple[str, str], _Slot] = {}
        self._jobs: dict[str, Job] = {}
        self._cap_queue: deque[Job] = deque()
        self._running = 0
        self._threads: list[threading.Thread] = []
        self._stopped = False

    # -- submission --------------------------------------------------------

    def submit(
        self,
        *,
        workspace: str,
        stack: str,
        digest: str,
        payload: dict[str, Any] | None = None,
    ) -> Submission:
        """Apply the coalescing policy to one request. Never blocks on a build."""
        with self._lock:
            if self._stopped:
                raise JobError("the daemon is shutting down; no new jobs are accepted")
            slot = self._slots.setdefault((workspace, stack), _Slot())

            # 2. identical work already in flight -- join it rather than build twice.
            #
            # Unless it is already dying. A cancelled job stays `active` until its builder
            # notices and tears down, which can take a while (killing a build, or a TTL
            # reap of a build that is ignoring the kill). Joining it would hand the new
            # request the corpse's CANCELLED event: `bosn cancel` immediately followed by
            # `bosn run` would build nothing and exit 5, and after a TTL reap *every*
            # subsequent run for that digest would do the same until the old process
            # finally died -- an agent loop wedged by a job it never asked about.
            if (
                slot.active is not None
                and slot.active.digest == digest
                and not slot.active.cancelled.is_set()
            ):
                return Submission(slot.active, joined=True)

            # 1. nothing in flight for this key
            if slot.active is None:
                job = self._new_job(workspace, stack, digest, payload, QUEUED)
                slot.active = job
                self._cap_queue.append(job)
                self._pump()
                return Submission(job, joined=False)

            # 3. a different digest is in flight: the depth-1 pending slot decides
            if slot.pending is not None and slot.pending.digest == digest:
                return Submission(slot.pending, joined=True)

            superseded = slot.pending
            if superseded is not None:
                self._settle(
                    superseded,
                    SUPERSEDED,
                    error=(
                        f"superseded: digest {_short(superseded.digest)} replaced by "
                        f"{_short(digest)} before its build started"
                    ),
                )
            job = self._new_job(workspace, stack, digest, payload, PENDING)
            slot.pending = job
            return Submission(job, joined=False, superseded=superseded)

    def _new_job(
        self,
        workspace: str,
        stack: str,
        digest: str,
        payload: dict[str, Any] | None,
        state: str,
    ) -> Job:
        job = Job(
            workspace=workspace,
            stack=stack,
            digest=digest,
            payload=payload or {},
            state=state,
            now=time.time(),
        )
        self._jobs[job.id] = job
        return job

    # -- scheduling --------------------------------------------------------

    def _pump(self) -> None:
        """Start as many queued jobs as the machine-wide cap allows. Caller holds the lock."""
        # Never start new work while shutting down. `shutdown` cancels in-flight jobs one at
        # a time, releasing the lock between each, so without this a job finishing in that
        # gap would promote its pending successor and launch a fresh `docker build` for a
        # daemon that is on its way out.
        if self._stopped:
            return
        while self._running < self.max_builds and self._cap_queue:
            job = self._cap_queue.popleft()
            if job.finished:
                continue
            job.state = RUNNING
            job.started_at = time.time()
            job.last_progress_at = job.started_at
            self._running += 1
            job.emit(job.summary())
            thread = threading.Thread(target=self._run, args=(job,), daemon=True)
            # Dead threads are dropped here rather than accumulating for the daemon's
            # lifetime; an agent loop starts one per edit.
            self._threads = [t for t in self._threads if t.is_alive()]
            self._threads.append(thread)
            thread.start()

    def _run(self, job: Job) -> None:
        state = FAILED
        error: str | None = None
        outcome: BuildOutcome | None = None
        try:
            outcome = self.builder(job)
        except KeyboardInterrupt:
            raise
        except Exception as exc:  # noqa: BLE001 - a builder crash must terminate its job, not the daemon
            error = f"{type(exc).__name__}: {exc}"
        else:
            state = SUCCEEDED if outcome.returncode == 0 else FAILED
            if state == FAILED:
                error = f"build exited {outcome.returncode}"

        if job.cancelled.is_set():
            state, error = CANCELLED, job.error or "cancelled"

        with self._lock:
            self._settle(
                job,
                state,
                error=error,
                returncode=outcome.returncode if outcome else None,
                result=outcome.result if outcome else None,
            )
            self._running -= 1
            self._advance(job)
            self._pump()
        if self.on_settled is not None:
            self.on_settled()

    def _advance(self, job: Job) -> None:
        """Promote the pending job into the slot its predecessor just vacated."""
        slot = self._slots.get(job.key)
        if slot is None or slot.active is not job:
            return
        slot.active = slot.pending
        slot.pending = None
        if slot.active is not None:
            slot.active.state = QUEUED
            slot.active.emit(slot.active.summary())
            self._cap_queue.append(slot.active)
        elif slot.empty():
            self._slots.pop(job.key, None)

    def _settle(
        self,
        job: Job,
        state: str,
        *,
        error: str | None = None,
        returncode: int | None = None,
        result: dict[str, Any] | None = None,
    ) -> None:
        """Move a job to a terminal state exactly once and tell everyone watching."""
        if not job.finalize(state, error=error, returncode=returncode, result=result):
            return
        self._prune_history()

    def _prune_history(self) -> None:
        """Forget the oldest finished jobs. Caller holds the lock.

        Only finished jobs are ever dropped, so this can never lose track of work that is
        still running. A forgotten job id stops resolving, which `attach` and `cancel`
        report as "no such job" -- the honest answer.
        """
        finished = [job for job in self._jobs.values() if job.finished]
        excess = len(finished) - self.max_history
        if excess <= 0:
            return
        finished.sort(key=lambda job: job.ended_at or job.created_at)
        for job in finished[:excess]:
            self._jobs.pop(job.id, None)

    # -- cancellation ------------------------------------------------------

    def cancel(self, job_id: str, *, reason: str = "cancelled by request") -> Job:
        """Cancel a job. Running builds are asked to stop; queued ones never start."""
        with self._lock:
            job = self._jobs.get(job_id)
            if job is None:
                raise JobError(f"no such job {job_id!r}")
            if job.finished:
                raise JobError(f"job {job_id} already finished as {job.state}")
            job.error = reason
            job.cancelled.set()
            if job.state == RUNNING:
                # The builder sees the event, kills its process, and _run settles the job.
                return job
            slot = self._slots.get(job.key)
            self._settle(job, CANCELLED, error=reason)
            if slot is not None:
                if slot.pending is job:
                    slot.pending = None
                    if slot.empty():
                        self._slots.pop(job.key, None)
                elif slot.active is job:
                    self._advance(job)
            self._pump()
        if self.on_settled is not None:
            self.on_settled()
        return job

    def reap_expired(self, now: float | None = None) -> list[Job]:
        """Cancel builds whose builder has stopped making progress for the TTL.

        Without this a builder that hangs forever pins the daemon forever: running jobs
        block idle retirement, so a job that never reports is also a daemon that never
        retires and resources that never get collected.
        """
        now = now if now is not None else time.time()
        reaped: list[Job] = []
        notifications: list[tuple[Job, dict[str, Any]]] = []
        with self._lock:
            for job in list(self._jobs.values()):
                # `log()` holds this same job lock while it refreshes progress, so the
                # decision cannot race a new builder line between stale inspection and
                # cancellation.
                with job._lock:
                    if job.state != RUNNING or job.started_at is None or job.cancelled.is_set():
                        continue
                    progress_at = job.last_progress_at or job.started_at
                    if now - progress_at <= self.ttl_seconds:
                        continue
                    reason = f"no builder progress for {self.ttl_seconds:.0f}s (build TTL)"
                    job.error = reason
                    job.cancelled.set()
                    notifications.append(
                        (job, {"event": "cancelling", "job": job.id, "reason": reason})
                    )
                    reaped.append(job)
        for job, event in notifications:
            job.emit(event)
        return reaped

    # -- observation -------------------------------------------------------

    def get(self, job_id: str) -> Job:
        with self._lock:
            job = self._jobs.get(job_id)
        if job is None:
            raise JobError(f"no such job {job_id!r}")
        return job

    def active_count(self) -> int:
        with self._lock:
            return sum(1 for job in self._jobs.values() if job.state in ACTIVE_STATES)

    def list_jobs(self, *, include_finished: bool = True) -> list[dict[str, Any]]:
        with self._lock:
            jobs = sorted(self._jobs.values(), key=lambda j: j.created_at)
        return [
            {
                "id": job.id,
                "state": job.state,
                "workspace": job.workspace,
                "stack": job.stack,
                "coalescing_key": job.digest,
                "created_at": job.created_at,
                "started_at": job.started_at,
                "ended_at": job.ended_at,
                "returncode": job.returncode,
                "error": job.error,
            }
            for job in jobs
            if include_finished or not job.finished
        ]

    def depth(self, workspace: str, stack: str) -> int:
        """How many jobs this key is holding. The policy's invariant: never more than 2."""
        with self._lock:
            slot = self._slots.get((workspace, stack))
            if slot is None:
                return 0
            return int(slot.active is not None) + int(slot.pending is not None)

    def follow(self, job: Job, *, heartbeat: float = 30.0) -> Iterator[dict[str, Any]]:
        """Yield a job's events until it ends, with heartbeats so silence is not death.

        A cold build can go minutes without printing. Without a heartbeat the client cannot
        distinguish a quiet build from a daemon that died, and would have to choose between
        a timeout that kills long builds and one that hangs forever.

        This deliberately ends only at the job's terminal event, and not on daemon
        shutdown: shutdown cancels every in-flight job first, so waiting for the terminal
        event is what lets an attached client be *told* it was cancelled. Bailing out on
        the stop signal would race that message and leave the client with a bare dropped
        connection instead of a reason.
        """
        watcher = job.watch()
        try:
            while True:
                try:
                    event = watcher.get(timeout=heartbeat)
                except queue.Empty:
                    yield {"event": "heartbeat", "job": job.id, "state": job.state}
                    continue
                yield event
                if event.get("final"):
                    return
        finally:
            job.unwatch(watcher)

    # -- lifecycle ---------------------------------------------------------

    def shutdown(self, *, timeout: float = 10.0) -> None:
        """Stop accepting work and cancel what is in flight, visibly."""
        with self._lock:
            self._stopped = True
            in_flight = [j for j in self._jobs.values() if j.state in ACTIVE_STATES]
        for job in in_flight:
            try:
                self.cancel(job.id, reason="the bosn daemon is shutting down")
            except JobError:
                continue
        deadline = time.time() + timeout
        for thread in list(self._threads):
            remaining = deadline - time.time()
            if remaining <= 0:
                break
            thread.join(timeout=remaining)


def _short(digest: str) -> str:
    return digest.removeprefix("sha256:")[:12]
