"""The `bosn __daemon` singleton.

The daemon is the registry's only writer and the only executor of reap and GC. It is lazily
spawned by the CLI, heartbeats while it serves, and idle-retires so an unused machine runs
no daemon.

**Singleton enforcement is the port bind itself.** The daemon listens on a deterministic
loopback port derived from its state directory, with address reuse disabled, so a second
daemon's `bind()` fails with EADDRINUSE and the operating system is the arbiter. No broker,
no lock file, and no pid comparison can disagree with it -- a stale state file cannot fake
a live listener, and a live listener cannot be missed. The state file is reporting metadata
only; discovery never depends on it.
"""

from __future__ import annotations

import errno
import hashlib
import json
import math
import os
import secrets
import socket
import socketserver
import subprocess
import sys
import threading
import time
import uuid
from collections.abc import Generator, Iterator
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any

from bosn import __version__, ipc
from bosn.clock import Clock, SystemClock
from bosn.config import Config
from bosn.jobs import BuildOutcome, Job, JobError, JobManager
from bosn.registry import Registry, default_state_dir

DAEMON_NAME = "bosn-daemon"
DEFAULT_PORT = 47764
# Deterministic per-state-dir ports live in the IANA dynamic range, above the default.
PORT_RANGE_START = 47765
PORT_RANGE_SIZE = 1024

DEFAULT_IDLE_RETIRE_SECONDS = 900.0
DEFAULT_MAINTENANCE_INTERVAL_SECONDS = 300.0
MAINTENANCE_BACKOFF_INITIAL_SECONDS = 30.0
MAINTENANCE_BACKOFF_MAX_SECONDS = 3600.0
SPAWN_TIMEOUT_SECONDS = 30.0

# Verbs that hold the connection open and write many messages instead of one.
STREAMING_VERBS = frozenset({"converge", "attach"})

# How long shutdown waits for cancelled builds to finish tearing down and release the
# registry. Must exceed a builder's worst case (Engine.stream waits 30s for the process,
# then converge still has its registry writes and volume creation to finish).
SHUTDOWN_DRAIN_SECONDS = 60.0

# The watchdog's periodic maintenance pass asks "is the engine reachable" before doing any
# real GC work through it. That is a liveness question, not a build or a GC operation, and
# it does not need Engine.DEFAULT_TIMEOUT's 60s -- a docker/podman CLI that is going to
# answer at all answers in well under this on every engine this project targets. Kept an
# int (not a float) because Engine.__init__'s `timeout` parameter is typed int.
MAINTENANCE_ENGINE_PROBE_TIMEOUT_SECONDS = 5

# Upper bound on how long shutdown() waits for the watchdog thread to notice `_stop` and
# exit, once the cooperative checks in `_run_maintenance` and the bounded probe above are
# both in place. `EngineInfo.info()` can make the probe call up to twice before it decides
# the engine is unreachable (client version, then server version), so the worst case the
# watchdog can still be doing engine I/O after `_stop` is set is roughly
# 2 * MAINTENANCE_ENGINE_PROBE_TIMEOUT_SECONDS. This is set well above that with slack for
# the (fast, non-networked) phases before it, rather than pinned exactly to 2x -- a bound
# that just barely covers the worst case would make an ordinary stop-during-probe look like
# the timeout path this constant exists to guard against. See `shutdown()` for what happens
# if this bound is ever actually exceeded.
WATCHDOG_JOIN_TIMEOUT_SECONDS = 15.0

# Upper bound on how long shutdown() waits for the startup-reconciliation thread (see
# `_reconcile_startup_resources`) to notice `_stop` and exit, before handing the registry
# close off to the same deferred-close path used when the watchdog overruns its own bound
# (see `_close_registry_after_background_threads`).
#
# This is deliberately much shorter than `WATCHDOG_JOIN_TIMEOUT_SECONDS`, and is 0.0 --
# not "small", zero -- on purpose. Do not "helpfully" raise this back into the seconds; a
# non-zero value here was tried and measured, and it does not buy what it looks like it
# should buy. Read on before changing it.
#
# The watchdog's maintenance pass is broken into phases with a cooperative `_stop` check
# between each (see `_run_maintenance`), so a bounded wait on it can be sized to the worst
# *remaining* phase and expect to usually succeed -- that is what
# `WATCHDOG_JOIN_TIMEOUT_SECONDS` is for, and it stays as-is. The startup scan has no such
# structure: `ResourceScanner(engine).scan(...)` is one call, not a sequence of abandonable
# phases, so there is no "remaining phase" to bound against.
#
# What that call actually does is bimodal, not "usually fast, occasionally slow": either
# the engine is unreachable and it fails in well under a millisecond (`Engine.available()`
# checks PATH before ever spawning anything), in which case the thread has *already*
# exited by the time `shutdown()` gets around to checking it and any positive bound is
# pure waste -- or the engine is reachable and it is doing a real scan, which issue #99
# tracks as slow on object-heavy hosts, in which case no bound measured in tens or hundreds
# of milliseconds has a realistic chance of covering it either. A first version of this
# constant used 2.0s on the theory that it would "usually" cover the reconcile thread; measuring it
# against this repo's own test suite showed that theory was wrong -- there is essentially no
# middle ground of shutdowns that are *almost* done and just need a couple hundred
# milliseconds more, so a positive bound here mostly just pays its own cost for no benefit:
# `test_daemon.py` + `test_daemon_jobs.py` went from ~37s to ~132s, and the full non-docker
# suite from ~93s to ~192s, i.e. the entire suite roughly doubled from a bound that was
# supposed to only matter in the (deliberately rare, per the comment on the watchdog's own
# bound above) "still running" case. In production this is worse than test-time waste: it
# is up to 2 extra seconds tacked onto every `bosn stop` whose reconcile thread has not yet
# finished, which for a slow scan on an object-heavy host (#99) is exactly the case #97
# already fixed shutdown latency for -- this constant was quietly re-introducing it.
#
# So: zero. If the thread has already exited, `thread.join(timeout=0.0)` observes that
# immediately and the ordinary close path is taken with no wait at all. If it has not, this
# defers to `_close_registry_after_background_threads` immediately rather than waiting to
# confirm what the bimodal reasoning above already predicts -- deferring costs nothing here
# (the actual close happens on its own daemon thread; `shutdown()` does not block on it),
# so there is nothing to gain by delaying the decision to defer.
RECONCILE_JOIN_TIMEOUT_SECONDS = 0.0


class DaemonError(RuntimeError):
    """The daemon could not be started, reached, or stopped."""


def state_file(state_dir: Path | None = None) -> Path:
    return (state_dir or default_state_dir()) / "daemon.json"


def heartbeat_file(state_dir: Path | None = None) -> Path:
    """A host-visible heartbeat consumed by persistent container PID 1 watchdogs."""
    return (state_dir or default_state_dir()) / "daemon.heartbeat"


def secret_file(state_dir: Path | None = None) -> Path:
    return (state_dir or default_state_dir()) / "daemon.secret"


def _secret(state_dir: Path | None = None) -> str:
    try:
        return secret_file(state_dir).read_text(encoding="utf-8").strip()
    except OSError:
        return ""


def port_for(state_dir: Path | None = None) -> int:
    """The deterministic loopback port that owns this state directory.

    Deterministic, never ephemeral: the client must be able to find the daemon without
    reading any file, and the OS must be able to refuse a second bind on the same port.
    An explicit BOSN_PORT wins so operators can move it off a conflicting port.
    """
    override = os.environ.get("BOSN_PORT")
    if override:
        return int(override)
    state_dir = state_dir or default_state_dir()
    if state_dir == default_state_dir():
        return DEFAULT_PORT
    digest = hashlib.sha256(str(state_dir.resolve()).encode("utf-8")).digest()
    return PORT_RANGE_START + (int.from_bytes(digest[:4], "big") % PORT_RANGE_SIZE)


@dataclass(frozen=True)
class DaemonState:
    pid: int
    port: int
    started_at: float
    version: str

    def write(self, path: Path) -> None:
        path.parent.mkdir(parents=True, exist_ok=True)
        tmp = path.with_suffix(".tmp")
        tmp.write_text(json.dumps(asdict(self)), encoding="utf-8")
        tmp.replace(path)

    @classmethod
    def read(cls, path: Path) -> DaemonState | None:
        try:
            raw = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            return None
        try:
            return cls(**raw)
        except TypeError:
            return None


def is_serving(
    state_dir: Path | None = None,
    *,
    timeout: float = 2.0,
    preserve_timeout: bool = False,
) -> bool:
    """True when something answers the ping on this state dir's port.

    Normal callers want a Boolean for spawn/stop decisions. A bounded diagnostic is different:
    a timeout proves that a listener was contacted but did not answer, which must not be
    flattened into absence before the caller can report a degraded control plane.
    """
    try:
        reply = ipc.send_request(
            port_for(state_dir), {"verb": "ping", "auth": _secret(state_dir)}, timeout=timeout
        )
    except ipc.TransportTimeout:
        if preserve_timeout:
            raise
        return False
    except ipc.TransportError:
        return False
    return bool(reply.get("ok"))


def running_state(state_dir: Path | None = None) -> DaemonState | None:
    """Reporting metadata for a *live* daemon, or None.

    Liveness is decided by the port, not by the file. A state file left behind by a killed
    daemon is removed rather than believed.
    """
    state_dir = state_dir or default_state_dir()
    path = state_file(state_dir)
    serving = is_serving(state_dir)
    state = DaemonState.read(path)
    if not serving:
        path.unlink(missing_ok=True)
        return None
    if state is None:
        # Serving but no readable metadata: report what the authenticated ping proves.
        try:
            reply = ipc.send_request(
                port_for(state_dir), {"verb": "ping", "auth": _secret(state_dir)}
            )
            return DaemonState(
                pid=int(reply.get("pid") or 0),
                port=port_for(state_dir),
                started_at=0.0,
                version=str(reply.get("version") or "unknown"),
            )
        except ipc.TransportError:
            return None
    return state


class _Handler(socketserver.StreamRequestHandler):
    def handle(self) -> None:
        request = ipc.read_request(self.connection)
        if request is None:
            return
        daemon_ref: Daemon = self.server.daemon_ref  # type: ignore[attr-defined]
        daemon_ref.note_activity()
        verb = str(request.get("verb", ""))
        if not secrets.compare_digest(str(request.get("auth") or ""), daemon_ref.secret):
            daemon_ref.registry.log_event("ipc.unauthenticated", verb)
            ipc.send_response(
                self.connection, {"ok": False, "error": "unauthenticated IPC request"}
            )
            return
        mutating_verbs = {
            "converge",
            "cancel",
            "gc",
            "done",
            "adopt",
            "compose-adopt",
            "execution-acquire",
            "execution-release",
            "compose-acquire",
            "compose-release",
        }
        if (
            verb != "shutdown"
            and verb in mutating_verbs
            and "version" in request
            and str(request.get("version") or "") != __version__
        ):
            ipc.send_response(
                self.connection,
                {
                    "ok": False,
                    "error": "daemon version differs; restart the daemon before destructive use",
                },
            )
            return
        try:
            if verb in STREAMING_VERBS:
                self._stream(daemon_ref, verb, request)
                return
            response = daemon_ref.dispatch(verb, request)
        except KeyboardInterrupt:
            # Ctrl-C on the daemon means shut down, not "this one request failed".
            daemon_ref.request_stop()
            raise
        except Exception as exc:  # noqa: BLE001 - every failure is observable, none fatal
            response = {"ok": False, "error": f"{type(exc).__name__}: {exc}"}
        ipc.send_response(self.connection, response)

    def _stream(self, daemon_ref: Daemon, verb: str, request: dict[str, Any]) -> None:
        """Hold the connection open and write events until the job reaches a terminal one.

        A client that hangs up here is abandoning its *view* of the job, never the job --
        which is the entire point of moving builds into the daemon. So a write failure ends
        this loop and nothing else.
        """

        def emit(event: dict[str, Any]) -> bool:
            try:
                ipc.send_response(self.connection, event)
            except OSError:
                return False  # the client went away; the job carries on without it
            return True

        # The whole iteration is guarded, not just the call that builds it: a streaming
        # verb is a generator, so nothing in its body runs until the first `next`. Catching
        # only around the call would let a bad manifest or an unknown job id hang the
        # connection up with no message at all -- a silent failure, which is the one thing
        # this protocol may never do.
        try:
            for event in daemon_ref.dispatch_stream(verb, request):
                if not emit(event):
                    return
        except KeyboardInterrupt:
            daemon_ref.request_stop()
            raise
        except Exception as exc:  # noqa: BLE001 - a failed stream still owes the client a reason
            emit({"ok": False, "final": True, "error": f"{type(exc).__name__}: {exc}"})


class _Server(socketserver.ThreadingTCPServer):
    # SO_REUSEADDR is platform-conditional, and the difference matters for correctness.
    #
    # On POSIX it only permits binding a port stuck in TIME_WAIT; a port with a *live*
    # listener still refuses the bind, so the singleton guarantee is intact and a restarted
    # daemon does not have to wait out TIME_WAIT before it can serve again.
    #
    # On Windows SO_REUSEADDR is different: it lets a second socket steal a port that is
    # actively being listened on, which would silently break the singleton. So it stays off
    # there and a restart eats the TIME_WAIT delay instead.
    #
    # SO_REUSEPORT is never set on any platform -- that one really does allow two live
    # listeners on one port, which is precisely the thing this design relies on being
    # impossible.
    allow_reuse_address = not sys.platform.startswith("win")
    daemon_threads = True

    def __init__(self, addr: tuple[str, int], daemon_ref: Daemon) -> None:
        self.daemon_ref = daemon_ref
        super().__init__(addr, _Handler)


class Daemon:
    """The serving side. Owns the registry connection and the idle clock."""

    def __init__(
        self,
        *,
        state_dir: Path | None = None,
        port: int | None = None,
        idle_retire_seconds: float = DEFAULT_IDLE_RETIRE_SECONDS,
        max_builds: int | None = None,
        build_ttl_seconds: float | None = None,
        engine_binary: str = "docker",
        maintenance_interval_seconds: float = DEFAULT_MAINTENANCE_INTERVAL_SECONDS,
        clock: Clock | None = None,
        config: Config | None = None,
    ) -> None:
        self.engine_binary = engine_binary
        self.config = config
        self.clock = clock or SystemClock()
        self.state_dir = state_dir or default_state_dir()
        self.bind_port = port_for(self.state_dir) if port is None else port
        self.idle_retire_seconds = idle_retire_seconds
        self.started_at = self.clock.now()
        self.last_activity = self.started_at
        self.heartbeat_at = self.started_at
        self.maintenance_interval_seconds = maintenance_interval_seconds
        self._maintenance_backoff_seconds = MAINTENANCE_BACKOFF_INITIAL_SECONDS
        self._server: _Server | None = None
        self._stop = threading.Event()
        self._watchdog_thread: threading.Thread | None = None
        self._reconcile_thread: threading.Thread | None = None
        self._execution_sessions: dict[str, tuple[str, ...]] = {}
        self._execution_containers: dict[str, str] = {}
        self._execution_owners: dict[str, tuple[int, float | None]] = {}
        self._execution_engines: dict[str, str] = {}
        self._execution_lock = threading.RLock()
        self._stopping = False
        self.secret = secrets.token_urlsafe(32)
        self.registry = Registry(self.state_dir / "registry.sqlite3", clock=self.clock)
        # Foreground command ownership must outlive the daemon process itself. A restarted
        # daemon restores this proof before serving, so it cannot overlap a still-running
        # remote exec or clean up a Podman session through Docker (or vice versa).
        for persisted in self.registry.execution_sessions():
            self._execution_sessions[persisted.id] = persisted.lease_ids
            self._execution_containers[persisted.id] = persisted.container_id
            self._execution_owners[persisted.id] = (
                persisted.client_pid,
                persisted.client_start,
            )
            self._execution_engines[persisted.id] = persisted.engine_binary
        stored_deadline = self.registry.meta("maintenance.next_deadline")
        try:
            deadline = float(stored_deadline) if stored_deadline else self.started_at
            if not math.isfinite(deadline):
                raise ValueError("deadline must be finite")
            self._next_maintenance_at = deadline
        except (TypeError, ValueError):
            self._next_maintenance_at = self.started_at
            self.registry.log_event("maintenance.deadline.recovered", stored_deadline or "")
        self.jobs = JobManager(
            self._build,
            max_builds=max_builds,
            ttl_seconds=build_ttl_seconds,
            # A finished job is activity: without this the daemon could retire the instant
            # a 20-minute build lands, before the client that was waiting for it comes back.
            on_settled=self.note_activity,
        )

    def _reconcile_startup_resources(self) -> None:
        """Repair create-before-registry crashes without making engine absence fatal."""
        from bosn.engine import Engine, EngineError
        from bosn.resources import ResourceScanner, recompute_manifest_generations, reconcile_owned

        try:
            engine = Engine(self.engine_binary)
            # Maintenance tests provide a minimal reachability probe.  Those test
            # doubles intentionally are not a resource-listing engine.
            if not callable(getattr(engine, "run", None)):
                return
            prior_resources = self.registry.list_resources()
            scan = ResourceScanner(engine).scan(self.registry.registry_id)
            if self._stop.is_set():
                return
            repaired = reconcile_owned(self.registry, scan, prior_resources=prior_resources)
            recompute_manifest_generations(self.registry, scan)
        except EngineError as exc:
            # #101: this call used to be the one that actually crashed. The scan failing
            # with `EngineError` (an unreachable engine is the most common way it fails) is
            # entirely expected -- that is what this whole `try` exists to catch -- but
            # `shutdown()` used to close the registry without ever waiting for this thread,
            # so `self.registry.log_event(...)` here could run after the connection was
            # already gone, raising `sqlite3.ProgrammingError: Cannot operate on a closed
            # database` from a background thread with nothing positioned to catch it.
            #
            # An `if not self._stop.is_set():` guard was considered here (to match the
            # `except Exception` branch below, which had one) and rejected: `_stop` is set
            # at the very start of `shutdown()`, before it even begins waiting for this
            # thread, so by the time this branch's `log_event` call could ever race a real
            # close, `_stop` is already set -- meaning the guard would suppress the log on
            # essentially every real occurrence of the race it was trying to protect
            # against, not just the unsafe ones. That trade only made sense when there was
            # no other way to reduce the crash's frequency. Now there is: `shutdown()`
            # tracks and joins this thread before it ever closes the registry (see
            # `RECONCILE_JOIN_TIMEOUT_SECONDS` and `_close_registry_after_background_
            # threads`), so by the time this line runs, the registry is guaranteed to still
            # be open regardless of `_stop` -- logging unconditionally is what "the branches
            # should agree" resolves to once the actual close-ordering bug is fixed, not
            # copying a guard whose only remaining effect would be silently dropping a
            # diagnostic that safely could have been recorded.
            self.registry.log_event("recovery.scan.unavailable", str(exc))
            return
        except KeyboardInterrupt:
            raise
        except Exception as exc:  # background recovery must never take down IPC
            # See the `except EngineError` branch above for why this no longer guards on
            # `_stop.is_set()` either -- the two branches must agree, and unconditional
            # logging (now safe, thanks to `shutdown()` joining this thread first) is the
            # agreement that does not cost a diagnostic every time this races shutdown.
            self.registry.log_event("recovery.scan.failed", str(exc))
            return
        if repaired:
            self.registry.log_event("recovery.startup", ",".join(repaired))

    # -- lifecycle ---------------------------------------------------------

    @property
    def port(self) -> int:
        if self._server is None:
            return self.bind_port
        return int(self._server.server_address[1])

    def note_activity(self) -> None:
        self.last_activity = self.clock.now()
        self.heartbeat_at = self.last_activity

    def idle_seconds(self) -> float:
        return self.clock.now() - self.last_activity

    def should_retire(self) -> bool:
        """Idle *and* holding no work. Retiring mid-build would destroy it.

        Request idleness alone is not enough now that the daemon owns builds: a client that
        submits a 20-minute build and hangs up leaves no request traffic at all, and the
        old rule would have retired the daemon out from under its own job. The job TTL is
        what keeps this from becoming a way to pin the daemon forever -- a build that never
        reports is eventually cancelled, and then this goes back to being about idleness.
        """
        with self._execution_lock:
            sessions_active = bool(self._execution_sessions)
        if self.jobs.active_count() > 0 or sessions_active:
            return False
        return self.idle_seconds() >= self.idle_retire_seconds

    def serve_forever(self) -> int:
        try:
            self._server = _Server((ipc.LOOPBACK, self.bind_port), self)
        except OSError as exc:
            self.registry.close()
            if exc.errno in (errno.EADDRINUSE, getattr(errno, "WSAEADDRINUSE", errno.EADDRINUSE)):
                raise DaemonError(
                    f"another bosn daemon already owns port {self.bind_port}; "
                    "refusing to start a second"
                ) from exc
            raise

        DaemonState(
            pid=os.getpid(),
            port=self.port,
            started_at=self.started_at,
            version=__version__,
        ).write(state_file(self.state_dir))
        self._write_heartbeat()
        secret_file(self.state_dir).write_text(self.secret, encoding="utf-8")
        try:
            os.chmod(secret_file(self.state_dir), 0o600)
        except OSError:
            pass
        self.registry.log_event("daemon.started", f"pid={os.getpid()} port={self.port}")

        # Docker can be unavailable or taking a long time to wake.  Recovery must not
        # postpone serving IPC, so this runs on a background thread rather than blocking
        # here. The comment that used to sit on this line claimed the thread "is cancelled
        # by normal shutdown" -- it never was: nothing sets an event this thread checks
        # before or during its one real blocking call, `ResourceScanner(...).scan(...)`,
        # and issue #101 is the background-thread crash that resulted from believing that
        # claim. `shutdown()` now tracks this thread the same way it tracks the watchdog
        # (see `RECONCILE_JOIN_TIMEOUT_SECONDS`) and folds it into the same deferred-close
        # decision, so the registry is never closed while this thread could still be using
        # it -- waited on, not cancelled.
        self._reconcile_thread = threading.Thread(
            target=self._reconcile_startup_resources, daemon=True
        )
        self._reconcile_thread.start()

        self._watchdog_thread = threading.Thread(target=self._idle_watchdog, daemon=True)
        self._watchdog_thread.start()
        try:
            self._server.serve_forever(poll_interval=0.2)
        finally:
            self.shutdown()
        return 0

    def _idle_watchdog(self) -> None:
        while not self._stop.wait(0.5):
            self._write_heartbeat()
            for job in self.jobs.reap_expired():
                self.registry.log_event("job.expired", f"{job.id} {job.stack}")
            if self.should_retire():
                if self.request_stop():
                    self.registry.log_event(
                        "daemon.idle_retired", f"idle={self.idle_seconds():.0f}s"
                    )
                    return
            self.run_maintenance_if_due()

    def _write_heartbeat(self) -> None:
        heartbeat_file(self.state_dir).touch()

    def run_maintenance_if_due(self) -> bool:
        """Run one unattended reap/GC pass when its deadline has arrived.

        Kept separate from the wall-clock watchdog so tests and embedders can advance an
        injected clock without sleeping.  Reap always precedes GC: an expired build lease
        must not shield a resource from the same pass that would otherwise collect it.
        """
        if self.clock.now() < self._next_maintenance_at:
            return False
        self._run_maintenance()
        return True

    def _run_maintenance(self) -> None:
        """Execute a maintenance pass, recording every outcome and scheduling its retry.

        Checks `self._stop` between every phase so a pass already in progress when
        shutdown begins abandons itself promptly instead of running to completion.
        `shutdown()` joins this thread with a bounded timeout on the assumption that it
        responds to `_stop` quickly; without these checks a pass could hold the watchdog
        thread for the sum of every phase (including the engine probe below) regardless
        of how long ago `_stop` was set. Each abandonment is logged -- a pass that gave up
        partway through must be visible as such, not indistinguishable from one that
        quietly ran to completion.
        """
        from bosn.engine import Engine
        from bosn.gc import Collector
        from bosn.resources import prune_dead_leases

        def abandoned_after(phase: str) -> bool:
            if not self._stop.is_set():
                return False
            self.registry.log_event("maintenance.aborted", f"stop requested after {phase}")
            return True

        self.registry.log_event("maintenance.reap.started")
        try:
            expired = self.jobs.reap_expired()
            for job in expired:
                self.registry.log_event("job.expired", f"{job.id} {job.stack}")
            self.registry.log_event("maintenance.reap.finished", f"expired={len(expired)}")
        except KeyboardInterrupt:
            raise
        except Exception as exc:  # noqa: BLE001 - a scheduler failure must be visible
            self.registry.log_event("maintenance.reap.error", f"{type(exc).__name__}: {exc}")
            self._set_next_maintenance(self.clock.now() + self.maintenance_interval_seconds)
            return
        if abandoned_after("reap"):
            return

        self.registry.log_event("maintenance.prune_leases.started")
        try:
            pruned = prune_dead_leases(self.registry)
            self.registry.log_event("maintenance.prune_leases.finished", f"pruned={len(pruned)}")
        except KeyboardInterrupt:
            raise
        except Exception as exc:  # noqa: BLE001 - a pruning failure must be visible, not silent
            self.registry.log_event(
                "maintenance.prune_leases.error", f"{type(exc).__name__}: {exc}"
            )
        if abandoned_after("prune_leases"):
            return

        # Derived done-signals run before GC (so a workspace reclaimed in this very pass is
        # eligible for the same pass's collection) and before the engine reachability check
        # below (so a Docker-down machine still gets the benefit -- this step only shells
        # out to git, never Docker).
        self.registry.log_event("maintenance.derived_done.started")
        try:
            candidates, reclaimed = self._derived_done_pass()
            self.registry.log_event(
                "maintenance.derived_done.finished",
                f"candidates={candidates} reclaimed={reclaimed}",
            )
        except KeyboardInterrupt:
            raise
        except Exception as exc:  # noqa: BLE001 - a scheduler failure must be visible, not silent
            self.registry.log_event(
                "maintenance.derived_done.error", f"{type(exc).__name__}: {exc}"
            )
        if abandoned_after("derived_done"):
            return

        # This probe only answers "is the engine there", so it gets the short named
        # timeout above rather than Engine's 60s default -- that default is right for the
        # real GC work below (`engine`, a *separate* instance kept at the normal timeout),
        # which legitimately needs however long a real docker/podman operation takes.
        # Reusing one Engine instance and mutating its `.timeout` around this call was
        # considered and rejected: it would leave a window where a concurrent reader of
        # `engine.timeout` (there are none today, but the next contributor to touch this
        # method would have no way to know that) sees the wrong value, for a saving of one
        # cheap, I/O-free constructor call.
        probe_engine = Engine(self.engine_binary, timeout=MAINTENANCE_ENGINE_PROBE_TIMEOUT_SECONDS)
        info = probe_engine.info()
        if abandoned_after("engine reachability probe"):
            return
        if not info.reachable:
            detail = info.detail or "engine daemon unreachable"
            self.registry.log_event(
                "maintenance.engine_down",
                f"retry_in={self._maintenance_backoff_seconds:g}s {detail}",
            )
            self._set_next_maintenance(self.clock.now() + self._maintenance_backoff_seconds)
            self._maintenance_backoff_seconds = min(
                self._maintenance_backoff_seconds * 2, MAINTENANCE_BACKOFF_MAX_SECONDS
            )
            return

        engine = Engine(self.engine_binary)
        self.registry.log_event("maintenance.execution_reap.started")
        reaped = self._reap_dead_execution_sessions()
        self.registry.log_event("maintenance.execution_reap.finished", f"reaped={reaped}")
        if abandoned_after("execution_reap"):
            return

        self.registry.log_event("maintenance.gc.started")
        try:
            result = Collector(self.registry, engine, config=self.config).collect(dry_run=False)
        except KeyboardInterrupt:
            raise
        except Exception as exc:  # noqa: BLE001 - never claim a failed GC succeeded
            self.registry.log_event("maintenance.gc.error", f"{type(exc).__name__}: {exc}")
        else:
            self.registry.log_event("maintenance.gc.finished", json.dumps(result.summary()))
        self._maintenance_backoff_seconds = MAINTENANCE_BACKOFF_INITIAL_SECONDS
        self._set_next_maintenance(self.clock.now() + self.maintenance_interval_seconds)

    def _derived_done_pass(self) -> tuple[int, int]:
        """Mark done every not-yet-done workspace whose Git state proves it is finished.

        Explicit `bosn done` remains the strongest, first-party signal -- this only ever
        *adds* done-ness through the same `gc.mark_done` write path, never removes it and
        never overrides one. A workspace is enumerated from `resources.workspace` (via
        `list_resources`), the same identity `mark_workspace_done` matches on, so it is
        passed through to `classify_workspace`/`mark_done` unmodified rather than
        renormalized. This misses a workspace that only survives in an older
        `resource_uses` row whose owning resource has since moved on to a different
        workspace -- an acceptable gap, since the failure direction is a missed
        reclamation, never a false "done".

        Every decision is logged, protected or reclaimed alike: a protected workspace with
        no recorded reason is what makes a derived-done pass untrustworthy. A classifier
        that raises protects the workspace it was evaluating and is logged, but must never
        aim to take down the rest of this pass -- so it is caught per workspace, not once
        for the whole loop.

        Returns (candidates considered, workspaces reclaimed).
        """
        from bosn.gc import mark_done
        from bosn.gitstate import classify_workspace

        known = {
            resource.workspace
            for resource in self.registry.list_resources()
            if resource.scope != "machine" and resource.workspace
        }
        candidates = sorted(known - self.registry.done_workspace_ids())
        reclaimed = 0
        for workspace in candidates:
            try:
                verdict = classify_workspace(workspace)
            except KeyboardInterrupt:
                raise
            except Exception as exc:  # noqa: BLE001 - ambiguity protects; a failed classifier
                # decides nothing, so treat it the same as any other non-safe verdict:
                # log why, leave the workspace exactly as it was, keep evaluating the rest.
                self.registry.log_event(
                    "maintenance.derived_done.error", f"{workspace}: {type(exc).__name__}: {exc}"
                )
                continue
            if verdict.safe_to_mark_done:
                mark_done(self.registry, workspace)
                reclaimed += 1
                self.registry.log_event(
                    "maintenance.derived_done.reclaimed",
                    f"{workspace} state={verdict.state.value} evidence={verdict.evidence}",
                )
            else:
                self.registry.log_event(
                    "maintenance.derived_done.protected",
                    f"{workspace} state={verdict.state.value} evidence={verdict.evidence}",
                )
        return len(candidates), reclaimed

    def _set_next_maintenance(self, deadline: float) -> None:
        self._next_maintenance_at = deadline
        self.registry.set_meta("maintenance.next_deadline", f"{deadline:.6f}")

    def request_stop(self) -> bool:
        # A SIGKILLed client cannot send execution-release. Reap only sessions whose
        # exact process identity is confirmed dead before deciding that active work must
        # block an explicit shutdown.
        with self._execution_lock:
            has_reapable_sessions = bool(self._execution_owners)
        if has_reapable_sessions:
            self._reap_dead_execution_sessions()
        with self._execution_lock:
            if self._execution_sessions:
                return False
            self._stopping = True
        self._stop.set()
        if self._server is not None:
            threading.Thread(target=self._server.shutdown, daemon=True).start()
        return True

    def shutdown(self) -> None:
        self._stop.set()
        # Both background threads get the same treatment: a bounded join here, and -- if
        # either one is still running once its bound expires -- the registry close is
        # deferred to a background thread that waits for *all* outstanding threads with no
        # bound of its own. Bounding the join alone (`thread.join(timeout=N)` with nothing
        # else) was considered and rejected for the watchdog originally (#97) and applies
        # equally here: falling through to closing the registry a few lines down while a
        # background thread is still reading/writing through that same connection turns a
        # bounded *hang* (annoying, but internally consistent) into an unbounded *crash*
        # (`sqlite3.ProgrammingError: Cannot operate on a closed database`, raised from a
        # background thread with no one positioned to handle it -- worse than the bug this
        # fix exists to close). See `WATCHDOG_JOIN_TIMEOUT_SECONDS` and
        # `RECONCILE_JOIN_TIMEOUT_SECONDS` for why each bound is sized the way it is; they
        # are deliberately not the same value, because the two threads' ability to finish
        # promptly once `_stop` is set is not the same.
        watchdog = self._watchdog_thread
        watchdog_finished = self._bounded_join(watchdog, WATCHDOG_JOIN_TIMEOUT_SECONDS)
        reconcile = self._reconcile_thread
        reconcile_finished = self._bounded_join(reconcile, RECONCILE_JOIN_TIMEOUT_SECONDS)
        # Cancel in-flight builds and wait for them *before* closing the registry they
        # write to. Each one tells its attached clients why it stopped rather than dying
        # silently.
        #
        # The timeout has to cover a builder's whole teardown, not just the kill: a
        # cancelled build waits up to 30s for the process to die and then still finishes
        # its converge. Closing the registry underneath it would turn a build that actually
        # succeeded into a failure, and leave the image and volumes Docker just created
        # with no registry rows -- the untracked resources bosn exists to prevent.
        self.jobs.shutdown(timeout=SHUTDOWN_DRAIN_SECONDS)
        if self._server is not None:
            self._server.server_close()
            self._server = None
        state_file(self.state_dir).unlink(missing_ok=True)
        heartbeat_file(self.state_dir).unlink(missing_ok=True)
        secret_file(self.state_dir).unlink(missing_ok=True)

        outstanding = [
            (name, thread)
            for name, thread, finished in (
                ("watchdog", watchdog, watchdog_finished),
                ("startup-reconcile", reconcile, reconcile_finished),
            )
            if thread is not None and not finished
        ]
        if outstanding:
            # At least one background thread is still running past its own patience.
            # Closing the registry here would race whatever it is doing right now --
            # possibly nothing worse than one more `log_event`, but there is no way from
            # here to know that, and guessing wrong means a background-thread crash. So
            # instead of closing, hand the close off to a thread that waits for *every*
            # outstanding thread however long that actually takes (unbounded is fine: this
            # thread is itself daemonic, so it cannot keep the process alive) and only then
            # performs the close this method would otherwise have done immediately below.
            #
            # Reaching this branch for the watchdog should be rare in ordinary operation
            # (see the comment on `WATCHDOG_JOIN_TIMEOUT_SECONDS`'s join above); reaching it
            # for startup-reconcile is the *expected* outcome whenever `shutdown()` runs
            # while the startup scan is still genuinely in flight (see
            # `RECONCILE_JOIN_TIMEOUT_SECONDS`). Either way it is worth a loud, findable
            # event before we return.
            names = ", ".join(name for name, _ in outstanding)
            # Guarded for the same reason this whole issue exists (#101): a diagnostic must
            # never be the thing that crashes.
            #
            # `shutdown()` runs twice in normal operation (once from `serve_forever`'s
            # `finally`, once explicitly), and a deferred close spawned by an earlier call
            # can complete at any moment -- including between this call's `_bounded_join`
            # deciding a thread is still outstanding and this line running. The connection
            # is then already gone, and there is no flag to test for it that would not
            # itself be racy. Every other registry write on the shutdown path is already
            # wrapped; this "loud event" was not, and CI caught it as exactly the
            # `sqlite3.ProgrammingError: Cannot operate on a closed database` this change
            # set out to eliminate, relocated one line away from the fix.
            #
            # Best-effort is the right contract here: the event explains a deferral that
            # has already been decided, so losing it costs a log line, while raising costs
            # the shutdown.
            try:
                self.registry.log_event(
                    "shutdown.background_join_timeout",
                    f"{names} still running; deferring registry close until they exit",
                )
            except KeyboardInterrupt:
                raise
            except Exception:  # noqa: BLE001 - shutdown must not raise
                pass
            threading.Thread(
                target=self._close_registry_after_background_threads,
                args=(tuple(thread for _, thread in outstanding),),
                daemon=True,
            ).start()
            return

        try:
            self.registry.log_event("daemon.stopped", "")
            self.registry.close()
        except KeyboardInterrupt:
            raise
        except Exception:  # noqa: BLE001 - shutdown must not raise
            pass

    @staticmethod
    def _bounded_join(thread: threading.Thread | None, timeout: float) -> bool:
        """True once `thread` has exited, given up to `timeout` seconds for it to do so.

        Also true when there is no thread to wait for (`None`) or when called from inside
        the thread itself -- `Thread.join` on the current thread deadlocks, and `shutdown()`
        can legitimately run on a thread it is also tracking (a verb handler calling
        `request_stop()` runs on a request-handling thread, never on the watchdog or
        reconcile thread, but this guard is cheap enough to keep unconditionally rather than
        rely on that staying true).
        """
        if thread is None or thread is threading.current_thread():
            return True
        thread.join(timeout=timeout)
        return not thread.is_alive()

    def _close_registry_after_background_threads(
        self, threads: tuple[threading.Thread, ...]
    ) -> None:
        """Finish a shutdown whose bounded joins gave up. See `shutdown()`.

        Reached only from the timeout branch above, carrying whichever of the watchdog and
        startup-reconcile threads (one or both) had not exited within their own bounded
        join. Waits for every one of them with no timeout of its own -- by this point there
        is nothing left to bound against, the goal is simply to never close the registry
        while any of them might still be touching it. Waiting for only the thread that
        triggered this call and ignoring any other still-outstanding one would reintroduce
        exactly the race this method exists to close, just for the thread that got left out.

        `shutdown()` may itself run again after spawning this (the
        `serve_forever`/`served`-fixture pattern of calling `shutdown()` both from
        `serve_forever`'s `finally` and again explicitly is already relied on elsewhere in
        this codebase); a second call sees each already-finished thread's bounded join
        return immediately, and -- once this method has joined whatever was still
        outstanding -- takes the ordinary close path itself. If both somehow race, sqlite's
        `close()` is a no-op on an already-closed connection and the surrounding
        `except Exception` here and in `shutdown()` swallow the `log_event` that could
        otherwise fire twice.
        """
        for thread in threads:
            thread.join()
        try:
            self.registry.log_event("shutdown.background_thread_finished_late", "")
            self.registry.log_event("daemon.stopped", "")
            self.registry.close()
        except KeyboardInterrupt:
            raise
        except Exception:  # noqa: BLE001 - shutdown must not raise
            pass

    # -- verbs -------------------------------------------------------------

    def dispatch(self, verb: str, request: dict[str, Any]) -> dict[str, Any]:
        handler = {
            "ping": self._verb_ping,
            "status": self._verb_status,
            "jobs": self._verb_jobs,
            "cancel": self._verb_cancel,
            "gc": self._verb_gc,
            "done": self._verb_done,
            "adopt": self._verb_adopt,
            "compose-adopt": self._verb_compose_adopt,
            "execution-acquire": self._verb_execution_acquire,
            "execution-release": self._verb_execution_release,
            "compose-acquire": self._verb_compose_acquire,
            "compose-release": self._verb_compose_release,
            "shutdown": self._verb_shutdown,
        }.get(verb)
        if handler is None:
            return {"ok": False, "error": f"unknown daemon verb {verb!r}"}
        return handler(request)

    def dispatch_stream(self, verb: str, request: dict[str, Any]) -> Iterator[dict[str, Any]]:
        if verb == "converge":
            return self._verb_converge(request)
        if verb == "attach":
            return self._verb_attach(request)
        raise DaemonError(f"unknown streaming verb {verb!r}")

    def _verb_ping(self, _request: dict[str, Any]) -> dict[str, Any]:
        return {"ok": True, "pong": True, "pid": os.getpid(), "version": __version__}

    def _verb_status(self, _request: dict[str, Any]) -> dict[str, Any]:
        from bosn.resources import process_alive

        with self._execution_lock:
            sessions = []
            for session, owner in self._execution_owners.items():
                alive = process_alive(*owner)
                last_reap_error = self.registry.latest_event(
                    "execution.orphan_reap.error", detail_prefix=f"session={session} "
                )
                sessions.append(
                    {
                        "id": session,
                        "container_id": self._execution_containers.get(session),
                        "engine": self._execution_engines.get(session),
                        "client_pid": owner[0],
                        "client_start": owner[1],
                        "client_alive": alive,
                        "lease_ids": list(self._execution_sessions.get(session, ())),
                        "blocking_reason": (
                            "client is live"
                            if alive
                            else "client is dead; awaiting safe exact-container reap"
                        ),
                        "last_orphan_reap_error": (
                            {
                                "at": last_reap_error["at"],
                                "detail": last_reap_error["detail"],
                            }
                            if last_reap_error is not None
                            else None
                        ),
                        "recovery": (
                            "do not interrupt the live client"
                            if alive
                            else "run `bosn daemon-stop`; it reaps only this exact "
                            "container after confirming the client is dead"
                        ),
                    }
                )
        return {
            "ok": True,
            "pid": os.getpid(),
            "port": self.port,
            "version": __version__,
            "registry_id": self.registry.registry_id,
            "uptime_seconds": time.time() - self.started_at,
            "idle_seconds": self.idle_seconds(),
            "resources": len(self.registry.list_resources()),
            "execution_sessions": sessions,
            "config": self.config.report() if self.config is not None else None,
        }

    def _verb_jobs(self, request: dict[str, Any]) -> dict[str, Any]:
        return {
            "ok": True,
            "jobs": self.jobs.list_jobs(include_finished=bool(request.get("all", True))),
            "max_builds": self.jobs.max_builds,
        }

    def _verb_cancel(self, request: dict[str, Any]) -> dict[str, Any]:
        job_id = str(request.get("job") or "")
        try:
            job = self.jobs.cancel(job_id)
        except JobError as exc:
            return {"ok": False, "error": str(exc)}
        self.registry.log_event("job.cancelled", job.id)
        return {"ok": True, "job": job.id, "state": job.state}

    def _verb_gc(self, request: dict[str, Any]) -> dict[str, Any]:
        from bosn.config import load as load_config
        from bosn.engine import Engine
        from bosn.gc import Collector

        flags = request.get("policy_flags")
        config = load_config(flags=flags if isinstance(flags, dict) else None)
        result = Collector(
            self.registry, Engine(str(request.get("engine") or self.engine_binary)), config=config
        ).collect(dry_run=bool(request.get("dry_run", True)))
        return {
            "ok": True,
            "result": result.summary(),
            "removed": result.removed,
            "stopped": result.stopped,
            "would_stop": result.would_stop,
            "image_dependency_deferred": result.image_dependency_deferred,
            "image_decisions": result.image_decisions,
            "errors": result.errors,
            "advisories": result.advisories,
        }

    def _verb_done(self, request: dict[str, Any]) -> dict[str, Any]:
        from bosn.gc import mark_done
        from bosn.paths import normalize_workspace_path

        workspace = str(request.get("workspace") or "")
        if not workspace:
            return {"ok": False, "error": "done requires a workspace"}
        # The bundled CLI already sends a normalized identity (workspace_of() resolves the
        # manifest root before it ever reaches IPC), but the wire contract itself accepts a
        # raw string. Any other spelling -- /cygdrive/, MSYS, a trailing slash, drive-letter
        # case -- must still match the identity mark_workspace_done matches against by exact
        # SQL equality, or this silently marks nothing while reporting ok=True.
        workspace = normalize_workspace_path(workspace)
        return {"ok": True, "marked": mark_done(self.registry, workspace)}

    def _verb_adopt(self, request: dict[str, Any]) -> dict[str, Any]:
        from bosn.engine import Engine
        from bosn.resources import (
            ResourceScanner,
            TransferError,
            adopt,
            recompute_manifest_generations,
            reconcile_owned,
            transfer_volume,
        )

        engine = Engine(str(request.get("engine") or self.engine_binary))
        selectors = [str(item) for item in request.get("transfer", [])]
        if selectors:
            transferred: list[str] = []
            for selector in selectors:
                kind, separator, name = selector.partition(":")
                if separator != ":" or not kind or not name:
                    return {"ok": False, "error": f"invalid transfer selector: {selector}"}
                if kind != "volume":
                    return {
                        "ok": False,
                        "error": (
                            f"{kind} labels are immutable and cannot be safely recreated; "
                            "only detached volumes support transfer"
                        ),
                    }
                candidates = ResourceScanner(engine).discover(kind)
                resource = next((item for item in candidates if item.name == name), None)
                if resource is None or not resource.complete:
                    return {"ok": False, "error": f"no complete labeled volume named {name}"}
                if resource.owned_by(self.registry.registry_id):
                    return {
                        "ok": False,
                        "error": f"volume already belongs to this registry: {name}",
                    }
                try:
                    transferred.append(transfer_volume(self.registry, engine, resource))
                except TransferError as exc:
                    return {"ok": False, "error": str(exc), "transferred": transferred}
            scan = ResourceScanner(engine).scan(self.registry.registry_id, kinds=["volume"])
            reconcile_owned(self.registry, scan)
            return {
                "ok": True,
                "transferred": transferred,
                "registry_id": self.registry.registry_id,
            }
        scan = ResourceScanner(engine).scan("", kinds=["container", "volume", "image"])
        registries = scan.foreign_registries
        if not registries:
            return {"ok": True, "adopted": [], "registry_id": None}
        selected = str(request.get("source_registry") or "")
        if selected:
            if selected not in registries:
                return {"ok": False, "error": f"adopt source registry not found: {selected}"}
            registry_id = selected
        elif len(registries) == 1:
            registry_id = next(iter(registries))
        else:
            commands = "; ".join(
                f"bosn adopt --from-registry {candidate}" for candidate in sorted(registries)
            )
            return {"ok": False, "error": f"choose a source registry: {commands}"}
        if self.registry.list_resources() and registry_id != self.registry.registry_id:
            return {
                "ok": False,
                "error": (
                    "adopt recovery requires an empty registry; current identity is preserved. "
                    "Use an explicit ownership-transfer workflow instead."
                ),
            }
        self.registry.set_meta("registry_id", registry_id)
        recovered = ResourceScanner(engine).scan(registry_id)
        names = adopt(self.registry, recovered)
        recompute_manifest_generations(self.registry, recovered)
        return {"ok": True, "adopted": names, "registry_id": registry_id}

    def _verb_compose_adopt(self, request: dict[str, Any]) -> dict[str, Any]:
        """Reconcile the registry against what Compose actually left on the engine.

        Additive by default (`adopt()`): a failed or partial engine listing must never be
        read as permission to forget rows, so plain reconcile-after-compose (every `up`,
        `down`, `logs`, `ps`) only ever registers what it finds.

        `prune_missing=True` is the opt-in exception, sent only when the caller just told
        Compose to *delete* something (`down -v`/`--volumes`): it additionally removes rows
        for previously-registered resources that the scan's kinds cover but did not find,
        via `reconcile_owned`'s `prior_resources` path -- the same crash-boundary repair
        `_reconcile_startup_resources` already trusts. Never used unconditionally here:
        that would turn an engine hiccup on an ordinary `up`/`logs`/`ps` into silent row
        loss for resources that never left the engine at all.
        """
        from bosn.resources import ResourceScanner, adopt, reconcile_owned

        if bool(request.get("prune_missing")):
            prior_resources = self.registry.list_resources()
            scan = ResourceScanner().scan(self.registry.registry_id)
            reconciled = reconcile_owned(self.registry, scan, prior_resources=prior_resources)
            return {"ok": True, "adopted": reconciled}
        scan = ResourceScanner().scan(self.registry.registry_id)
        return {"ok": True, "adopted": adopt(self.registry, scan)}

    def _verb_execution_acquire(self, request: dict[str, Any]) -> dict[str, Any]:
        from bosn.converge import Converger, ConvergeResult
        from bosn.engine import Engine
        from bosn.manifest import load
        from bosn.paths import normalize_workspace_path
        from bosn.registry import ExecutionSession
        from bosn.resources import process_alive

        manifest = load(Path(str(request.get("manifest") or "")))
        converged = ConvergeResult.from_dict(dict(request.get("result") or {}))
        workspace = str(request.get("workspace") or "")
        if not workspace:
            return {"ok": False, "error": "execution-acquire requires workspace"}
        pid = request.get("pid")
        if not isinstance(pid, int) or pid <= 0:
            return {"ok": False, "error": "execution-acquire requires a client pid"}
        proc_start_raw = request.get("proc_start")
        proc_start = float(proc_start_raw) if proc_start_raw is not None else None
        # Same boundary gap as `done`, on the write side: an un-normalized spelling here
        # would record the container's registry row under an identity that `done` (which
        # does normalize) can never match, permanently pinning the resource. Normalizing is
        # a no-op for the CLI's already-canonical value.
        workspace = normalize_workspace_path(workspace)
        engine_binary = str(request.get("engine") or self.engine_binary)
        engine = Engine(engine_binary)
        # Recover a previous client that died without reaching its finally/release path
        # before considering this container busy. The immutable id is retained alongside
        # the session, so cleanup can never broaden into a label/name-based deletion.
        self._reap_dead_execution_sessions()
        session = str(uuid.uuid4())
        with self._execution_lock:
            if self._stopping:
                return {"ok": False, "error": "daemon is stopping; retry after it restarts"}
            # Reserve before engine/registry work so shutdown sees this as active immediately.
            self._execution_sessions[session] = ()
            self._execution_owners[session] = (pid, proc_start)
            self._execution_engines[session] = engine_binary
        try:
            container_id, leases = Converger(
                manifest, self.registry, engine
            )._acquire_execution_container(
                converged,
                stack_name=request.get("stack") or None,
                workspace=workspace,
                lease_pid=pid,
                lease_proc_start=proc_start,
            )
        except KeyboardInterrupt:
            with self._execution_lock:
                self._execution_sessions.pop(session, None)
                self._execution_containers.pop(session, None)
                self._execution_owners.pop(session, None)
                self._execution_engines.pop(session, None)
            raise
        except Exception:
            with self._execution_lock:
                self._execution_sessions.pop(session, None)
                self._execution_containers.pop(session, None)
                self._execution_owners.pop(session, None)
                self._execution_engines.pop(session, None)
            raise
        with self._execution_lock:
            # Container IDs are effectively unique, and global serialization is the safe
            # answer for alias spellings such as `docker` versus its absolute executable
            # path. The original engine spelling remains session metadata only for cleanup.
            conflict = container_id in self._execution_containers.values()
            if conflict:
                self._execution_sessions.pop(session, None)
                self._execution_owners.pop(session, None)
                self._execution_engines.pop(session, None)
            else:
                lease_ids = tuple(lease.id for lease in leases)
                self._execution_sessions[session] = lease_ids
                self._execution_containers[session] = container_id
                try:
                    self.registry.save_execution_session(
                        ExecutionSession(
                            id=session,
                            container_id=container_id,
                            engine_binary=engine_binary,
                            client_pid=pid,
                            client_start=proc_start,
                            lease_ids=lease_ids,
                        )
                    )
                except KeyboardInterrupt:
                    # Retain the provisional in-memory proof. The client never received
                    # this session, so it cannot start a remote exec; maintenance can still
                    # retry exact-container and lease cleanup if this daemon keeps running.
                    raise
                except Exception as exc:
                    self.registry.log_event(
                        "execution.persistence.error",
                        f"session={session} container={container_id} {type(exc).__name__}: {exc}",
                    )
                    raise
        if conflict:
            for lease in leases:
                self.registry.release_lease(lease.id)
            return {
                "ok": False,
                "error": "another command is already running in this stack; retry after it exits",
            }
        # The caller can die while converge/acquire is in flight. Do not return a session
        # that was already orphaned; route it through the same fail-closed cleanup path.
        if not process_alive(pid, proc_start):
            self._reap_dead_execution_sessions()
            return {"ok": False, "error": "execution client exited during container acquire"}
        return {"ok": True, "container": container_id, "session": session}

    def _reap_dead_execution_sessions(self) -> int:
        """Stop and release execution sessions whose exact client identity is gone.

        Cleanup stays serialized with acquire/release. If Docker or the registry refuses
        any part of cleanup, the session remains active and continues to block reuse; an
        uncertain cleanup must never permit two commands in one persistent container.
        """
        from bosn.engine import Engine
        from bosn.resources import process_alive

        reaped = 0
        with self._execution_lock:
            dead = [
                session
                for session, (pid, proc_start) in self._execution_owners.items()
                if session in self._execution_containers and not process_alive(pid, proc_start)
            ]
            for session in dead:
                container_id = self._execution_containers[session]
                engine_binary = self._execution_engines[session]
                try:
                    engine = Engine(engine_binary)
                    removed = engine.run(["container", "rm", "--force", container_id], timeout=10)
                    detail = removed.stderr or removed.stdout
                    if not removed.ok and "no such container" not in detail.lower():
                        raise RuntimeError(detail or "container removal failed")
                    for lease_id in self._execution_sessions[session]:
                        self.registry.release_lease(lease_id)
                    self.registry.delete_execution_session(session)
                except KeyboardInterrupt:
                    raise
                except Exception as exc:  # noqa: BLE001 - ambiguity must fail closed
                    self.registry.log_event(
                        "execution.orphan_reap.error",
                        f"session={session} container={container_id} {type(exc).__name__}: {exc}",
                    )
                    continue
                self._execution_sessions.pop(session, None)
                self._execution_containers.pop(session, None)
                self._execution_owners.pop(session, None)
                self._execution_engines.pop(session, None)
                self.registry.log_event(
                    "execution.orphan_reaped",
                    f"session={session} container={container_id}",
                )
                reaped += 1
        return reaped

    def _verb_execution_release(self, request: dict[str, Any]) -> dict[str, Any]:
        session = str(request.get("session") or "")
        with self._execution_lock:
            leases = self._execution_sessions.get(session)
            if leases is None:
                return {"ok": False, "error": "unknown execution session"}
            try:
                for lease_id in leases:
                    self.registry.release_lease(lease_id)
                self.registry.delete_execution_session(session)
            except KeyboardInterrupt:
                raise
            except Exception as exc:  # retain every proof so cleanup can be retried
                return {"ok": False, "error": f"execution release failed: {exc}"}
            self._execution_sessions.pop(session, None)
            self._execution_containers.pop(session, None)
            self._execution_owners.pop(session, None)
            self._execution_engines.pop(session, None)
        return {"ok": True}

    def _verb_compose_acquire(self, request: dict[str, Any]) -> dict[str, Any]:
        """Lease every resource currently registered for a Compose project's workspace.

        Both foreground execution paths carry the client's pid/start identity so a SIGKILL
        cannot create an immortal daemon-owned lease. Compose differs because its resources
        are independently safe to prune through the normal TTL-plus-liveness rule; a run or
        shell also owns a remote process inside a shared persistent container, so its
        orphan-recovery path must first force-remove that exact immutable container id.
        """
        from bosn.paths import normalize_workspace_path

        workspace = str(request.get("workspace") or "")
        if not workspace:
            return {"ok": False, "error": "compose-acquire requires workspace"}
        # Same boundary gap as `done`/`execution-acquire`: an un-normalized spelling here
        # would match zero resources and silently lease nothing.
        workspace = normalize_workspace_path(workspace)
        pid = request.get("pid")
        if not isinstance(pid, int) or pid <= 0:
            return {"ok": False, "error": "compose-acquire requires a client pid"}
        proc_start_raw = request.get("proc_start")
        proc_start = float(proc_start_raw) if proc_start_raw is not None else None
        session = str(uuid.uuid4())
        with self._execution_lock:
            if self._stopping:
                return {"ok": False, "error": "daemon is stopping; retry after it restarts"}
            # Reserve before the lease loop below, same as execution-acquire: shutdown must
            # see this session as active the instant it exists, not only once every
            # dependency's lease has landed.
            self._execution_sessions[session] = ()
        try:
            # Compare normalized against normalized rather than trusting a registered
            # resource's stored `workspace` to already be canonical: the Compose overlay
            # writes the raw `str(compose.parent.resolve())` into its labels (see
            # `_compose_overlay`), not a normalized identity, so an un-normalized comparison
            # here would silently match nothing on any platform where normalization is not
            # a no-op (Windows case-folding, MSYS/cygdrive spellings, a trailing slash).
            resources = [
                resource
                for resource in self.registry.list_resources()
                if normalize_workspace_path(resource.workspace) == workspace
            ]
            leases = tuple(
                self.registry.acquire_lease(resource.id, pid=pid, proc_start=proc_start)
                for resource in resources
            )
        except KeyboardInterrupt:
            # Same as execution-acquire: the reservation above must not outlive a failed
            # acquire, or a session the client never received a session id for pins the
            # daemon (`should_retire`/`request_stop` both key off `_execution_sessions`)
            # forever -- nobody left holding it can ever call compose-release.
            with self._execution_lock:
                self._execution_sessions.pop(session, None)
            raise
        except Exception:
            with self._execution_lock:
                self._execution_sessions.pop(session, None)
            raise
        with self._execution_lock:
            self._execution_sessions[session] = tuple(lease.id for lease in leases)
        return {"ok": True, "session": session, "leased": len(leases)}

    def _verb_compose_release(self, request: dict[str, Any]) -> dict[str, Any]:
        session = str(request.get("session") or "")
        with self._execution_lock:
            leases = self._execution_sessions.pop(session, None)
        if leases is None:
            # A failed compose-acquire returns no session at all, so the client's own
            # `finally` calls this with `None`/unknown; a double-release after a retried
            # request lands here too. Neither is a caller error worth surfacing -- there is
            # simply nothing left to release.
            return {"ok": True, "released": 0}
        for lease_id in leases:
            self.registry.release_lease(lease_id)
        return {"ok": True, "released": len(leases)}

    def _verb_converge(self, request: dict[str, Any]) -> Iterator[dict[str, Any]]:
        """Submit a converge under the coalescing policy, then stream the job it landed on.

        The submission decision itself is reported before any build output, so a client can
        always tell whether it started work, joined someone else's, or is queued behind an
        obsolete build it is about to wait out.
        """
        manifest_path = str(request.get("manifest") or "")
        if not manifest_path:
            raise DaemonError("converge requires a manifest path")

        from bosn.converge import generation_coalescing_key, workspace_of
        from bosn.engine import Engine
        from bosn.manifest import load
        from bosn.paths import normalize_workspace_path

        manifest = load(Path(manifest_path))
        stack = manifest.stack(request.get("stack") or None)
        engine_binary = str(request.get("engine") or self.engine_binary)
        coalescing_key = generation_coalescing_key(manifest, stack, Engine(engine_binary))
        # Same boundary gap as `done`: an un-normalized spelling here would both write
        # registry rows under an identity `done` can never match, and fragment the job
        # slot's (workspace, stack) coalescing key so two spellings of one workspace could
        # run unserialized -- the same per-worktree splitting issue #1 fixed for the CLI
        # path. `workspace_of` already normalizes; normalizing an explicit request value too
        # keeps both branches of this `or` producing the same canonical form.
        workspace = normalize_workspace_path(
            str(request.get("workspace") or workspace_of(manifest))
        )

        submission = self.jobs.submit(
            workspace=workspace,
            stack=stack.name,
            digest=coalescing_key,
            payload={
                "manifest": str(manifest.path),
                "stack": stack.name,
                "engine": engine_binary,
            },
        )
        job = submission.job
        self.registry.log_event("job.submitted", f"{job.id} {submission.disposition}")
        yield {
            "event": "submitted",
            "job": job.id,
            "state": job.state,
            "joined": submission.joined,
            "disposition": submission.disposition,
            "coalescing_key": coalescing_key,
            "workspace": workspace,
            "stack": stack.name,
            "superseded": submission.superseded.id if submission.superseded else None,
        }
        yield from self.jobs.follow(job)

    def _verb_attach(self, request: dict[str, Any]) -> Iterator[dict[str, Any]]:
        job = self.jobs.get(str(request.get("job") or ""))
        yield {"event": "attached", "job": job.id, "state": job.state}
        yield from self.jobs.follow(job)

    # -- the builder -------------------------------------------------------

    def _build(self, job: Job) -> BuildOutcome:
        """Run one converge to completion inside the daemon.

        This is where the registry actually becomes single-writer for converge: the build
        and the generation/resource rows it produces both happen here, in the process that
        outlives the CLI.

        The manifest is re-read here rather than carried over from submission, so the
        digest built is the one the spec has *now*. The job's own digest is a snapshot taken
        at submit time and is only ever a coalescing key -- if the Dockerfile changed again
        while this job sat in the pending slot, converging to the newer content is the
        right answer, and the ConvergeResult reports the digest actually built.
        """
        from bosn.converge import Converger
        from bosn.engine import Engine, EngineError
        from bosn.manifest import load

        manifest = load(Path(str(job.payload["manifest"])))
        engine = Engine(str(job.payload.get("engine") or self.engine_binary))
        converger = Converger(
            manifest,
            self.registry,
            engine,
            progress=lambda line: job.log(line),
            cancelled=job.cancelled,
        )
        try:
            result = converger.converge(
                str(job.payload.get("stack") or "") or None, workspace=job.workspace
            )
        except EngineError as exc:
            # Truncated: a build failure's message embeds the failed command's whole
            # output, which for `docker build` is the entire transcript -- already streamed
            # line by line. Logging it whole would duplicate the build into one enormous
            # event and evict real output from the ring buffer.
            job.log(_clip(str(exc)), stream="stderr")
            return BuildOutcome(returncode=1)
        return BuildOutcome(returncode=0, result=result.to_dict())

    def _verb_shutdown(self, _request: dict[str, Any]) -> dict[str, Any]:
        if not self.request_stop():
            return {
                "ok": False,
                "error": "daemon has active execution session(s); wait for run or shell to exit",
            }
        return {"ok": True, "stopping": True}


def _clip(text: str, limit: int = 2000) -> str:
    """Keep the tail of an over-long message -- the end of a build log is the useful part."""
    if len(text) <= limit:
        return text
    return f"[...{len(text) - limit} characters elided...] {text[-limit:]}"


# -- client side -----------------------------------------------------------


def _detach(state_dir: Path) -> int:
    """Start a detached `bosn __daemon`.

    bosn deliberately does **not** use running-process's broker/daemon framework for this.
    Both of its detachment entry points are clients of that framework rather than plain
    process launches: `daemon.spawn_daemon` needs a bundled trampoline binary plus its own
    runtime directory and sidecar JSON, and `launch_detached` dials a broker over
    `\\\\.\\pipe\\running-process-daemon-<user>`. bosn owns exactly one daemon whose
    singleton is already enforced by the port bind, so a second supervisor underneath it
    buys nothing and adds a dependency that must be running before ours can start.

    running-process remains our process API for *running* commands (see `engine.py`); this
    is only about detaching our own daemon.

    The Windows flag here is `CREATE_NO_WINDOW`, not `DETACHED_PROCESS`. `DETACHED_PROCESS`
    means "do not inherit the parent's console", and Windows honors it by giving the child a
    console of its own -- a terminal window pops up on the user's screen. `CREATE_NO_WINDOW`
    gives it no console at all, which is what a background supervisor wants.
    """
    # The port is passed explicitly rather than recomputed by the child. port_for() is
    # relative to default_state_dir(), which reads BOSN_STATE_DIR -- so a child with a
    # different environment can derive a different port for the same directory and then
    # bind somewhere the parent is not listening. Passing it makes the two agree by
    # construction, and for the same reason the child's environment is left alone.
    env = dict(os.environ)
    argv = [
        sys.executable,
        "-m",
        "bosn",
        "__daemon",
        "--state-dir",
        str(state_dir),
        "--port",
        str(port_for(state_dir)),
    ]

    kwargs: dict[str, Any] = {
        "env": env,
        "stdin": subprocess.DEVNULL,
        "stdout": subprocess.DEVNULL,
        "stderr": subprocess.DEVNULL,
        "close_fds": True,
    }
    if sys.platform.startswith("win"):
        # getattr: these constants only exist on Windows, and a type checker running on
        # Linux does not know them.
        kwargs["creationflags"] = getattr(subprocess, "CREATE_NO_WINDOW", 0) | getattr(
            subprocess, "CREATE_NEW_PROCESS_GROUP", 0
        )
    else:
        # setsid: the daemon leaves our session, so it survives the shell that started it
        # and never receives the terminal's Ctrl-C.
        kwargs["start_new_session"] = True

    return subprocess.Popen(argv, **kwargs).pid


def spawn(state_dir: Path | None = None, *, timeout: float = SPAWN_TIMEOUT_SECONDS) -> DaemonState:
    """Lazily spawn the daemon and wait until it answers. Idempotent.

    Racing spawns are harmless: whichever process wins the bind serves, and the losers exit
    with EADDRINUSE while their callers connect to the winner.
    """
    state_dir = state_dir or default_state_dir()
    if is_serving(state_dir):
        state = running_state(state_dir)
        if state is not None and state.pid:
            return state

    _detach(state_dir)

    deadline = time.time() + timeout
    while time.time() < deadline:
        state = running_state(state_dir)
        if state is not None and state.pid:
            return state
        time.sleep(0.1)
    raise DaemonError(f"daemon did not answer on port {port_for(state_dir)} within {timeout:.0f}s")


def request(
    verb: str,
    state_dir: Path | None = None,
    *,
    autostart: bool = True,
    request_timeout: float = ipc.DEFAULT_TIMEOUT,
    diagnostic: bool = False,
    **payload: Any,
) -> dict[str, Any]:
    """Send a verb to the daemon, spawning it first when allowed.

    Fails closed when the daemon is unreachable and autostart is off -- a fallback to raw
    Docker would recreate exactly the unregistered resources bosn exists to eliminate.
    ``diagnostic`` is intentionally opt-in for bounded read-only probes: it preserves a ping
    timeout as ``TransportTimeout`` instead of flattening it into "not serving". Normal
    autostart and every mutating call retain the historical Boolean liveness behavior.
    """
    state_dir = state_dir or default_state_dir()
    if not is_serving(
        state_dir,
        timeout=request_timeout if diagnostic else 2.0,
        preserve_timeout=diagnostic,
    ):
        if not autostart:
            raise DaemonError("no bosn daemon is running")
        spawn(state_dir)
    return ipc.send_request(
        port_for(state_dir),
        {"verb": verb, "auth": _secret(state_dir), "version": __version__, **payload},
        timeout=request_timeout,
    )


def stream(
    verb: str,
    state_dir: Path | None = None,
    *,
    autostart: bool = True,
    **payload: Any,
) -> Generator[dict[str, Any], None, None]:
    """Send a streaming verb and yield its events until the daemon sends `final`."""
    state_dir = state_dir or default_state_dir()
    if not is_serving(state_dir):
        if not autostart:
            raise DaemonError("no bosn daemon is running")
        spawn(state_dir)
    return ipc.stream_request(
        port_for(state_dir),
        {"verb": verb, "auth": _secret(state_dir), "version": __version__, **payload},
    )


def stop(state_dir: Path | None = None, *, timeout: float = 10.0) -> bool:
    """Ask a running daemon to stop. True if one was running and went away."""
    state_dir = state_dir or default_state_dir()
    if not is_serving(state_dir):
        return False
    try:
        reply = ipc.send_request(
            port_for(state_dir),
            {"verb": "shutdown", "auth": _secret(state_dir), "version": __version__},
            timeout=timeout,
        )
    except ipc.TransportError:
        pass
    else:
        if not reply.get("ok"):
            raise DaemonError(str(reply.get("error") or "daemon refused shutdown"))
    deadline = time.time() + timeout
    while time.time() < deadline:
        if not is_serving(state_dir):
            return True
        time.sleep(0.1)
    return not is_serving(state_dir)


def free_port() -> int:
    """Reserve and release an ephemeral loopback port (used by tests for a dead port)."""
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind((ipc.LOOPBACK, 0))
        return int(sock.getsockname()[1])
