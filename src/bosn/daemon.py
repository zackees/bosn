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
import os
import socket
import socketserver
import subprocess
import sys
import threading
import time
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any

from bosn import __version__, ipc
from bosn.registry import Registry, default_state_dir

DAEMON_NAME = "bosn-daemon"
DEFAULT_PORT = 47764
# Deterministic per-state-dir ports live in the IANA dynamic range, above the default.
PORT_RANGE_START = 47765
PORT_RANGE_SIZE = 1024

DEFAULT_IDLE_RETIRE_SECONDS = 900.0
SPAWN_TIMEOUT_SECONDS = 30.0


class DaemonError(RuntimeError):
    """The daemon could not be started, reached, or stopped."""


def state_file(state_dir: Path | None = None) -> Path:
    return (state_dir or default_state_dir()) / "daemon.json"


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


def is_serving(state_dir: Path | None = None, *, timeout: float = 2.0) -> bool:
    """True when something answers the ping on this state dir's port."""
    try:
        reply = ipc.send_request(port_for(state_dir), {"verb": "ping"}, timeout=timeout)
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
        # Serving but no readable metadata: report what we can prove.
        return DaemonState(pid=0, port=port_for(state_dir), started_at=0.0, version="unknown")
    return state


class _Handler(socketserver.StreamRequestHandler):
    def handle(self) -> None:
        request = ipc.read_request(self.connection)
        if request is None:
            return
        daemon_ref: Daemon = self.server.daemon_ref  # type: ignore[attr-defined]
        daemon_ref.note_activity()
        verb = str(request.get("verb", ""))
        try:
            response = daemon_ref.dispatch(verb, request)
        except KeyboardInterrupt:
            # Ctrl-C on the daemon means shut down, not "this one request failed".
            daemon_ref.request_stop()
            raise
        except Exception as exc:  # noqa: BLE001 - every failure is observable, none fatal
            response = {"ok": False, "error": f"{type(exc).__name__}: {exc}"}
        ipc.send_response(self.connection, response)


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
    ) -> None:
        self.state_dir = state_dir or default_state_dir()
        self.bind_port = port_for(self.state_dir) if port is None else port
        self.idle_retire_seconds = idle_retire_seconds
        self.started_at = time.time()
        self.last_activity = self.started_at
        self.heartbeat_at = self.started_at
        self._server: _Server | None = None
        self._stop = threading.Event()
        self.registry = Registry(self.state_dir / "registry.sqlite3")

    # -- lifecycle ---------------------------------------------------------

    @property
    def port(self) -> int:
        if self._server is None:
            return self.bind_port
        return int(self._server.server_address[1])

    def note_activity(self) -> None:
        self.last_activity = time.time()
        self.heartbeat_at = self.last_activity

    def idle_seconds(self) -> float:
        return time.time() - self.last_activity

    def should_retire(self) -> bool:
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
        self.registry.log_event("daemon.started", f"pid={os.getpid()} port={self.port}")

        threading.Thread(target=self._idle_watchdog, daemon=True).start()
        try:
            self._server.serve_forever(poll_interval=0.2)
        finally:
            self.shutdown()
        return 0

    def _idle_watchdog(self) -> None:
        while not self._stop.wait(0.5):
            if self.should_retire():
                self.registry.log_event("daemon.idle_retired", f"idle={self.idle_seconds():.0f}s")
                self.request_stop()
                return

    def request_stop(self) -> None:
        self._stop.set()
        if self._server is not None:
            threading.Thread(target=self._server.shutdown, daemon=True).start()

    def shutdown(self) -> None:
        self._stop.set()
        if self._server is not None:
            self._server.server_close()
            self._server = None
        state_file(self.state_dir).unlink(missing_ok=True)
        try:
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
            "shutdown": self._verb_shutdown,
        }.get(verb)
        if handler is None:
            return {"ok": False, "error": f"unknown daemon verb {verb!r}"}
        return handler(request)

    def _verb_ping(self, _request: dict[str, Any]) -> dict[str, Any]:
        return {"ok": True, "pong": True, "pid": os.getpid(), "version": __version__}

    def _verb_status(self, _request: dict[str, Any]) -> dict[str, Any]:
        return {
            "ok": True,
            "pid": os.getpid(),
            "port": self.port,
            "version": __version__,
            "registry_id": self.registry.registry_id,
            "uptime_seconds": time.time() - self.started_at,
            "idle_seconds": self.idle_seconds(),
            "resources": len(self.registry.list_resources()),
        }

    def _verb_jobs(self, _request: dict[str, Any]) -> dict[str, Any]:
        return {"ok": True, "jobs": []}

    def _verb_shutdown(self, _request: dict[str, Any]) -> dict[str, Any]:
        self.request_stop()
        return {"ok": True, "stopping": True}


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
        kwargs["creationflags"] = subprocess.CREATE_NO_WINDOW | subprocess.CREATE_NEW_PROCESS_GROUP
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
        if state is not None:
            return state

    _detach(state_dir)

    deadline = time.time() + timeout
    while time.time() < deadline:
        state = running_state(state_dir)
        if state is not None:
            return state
        time.sleep(0.1)
    raise DaemonError(f"daemon did not answer on port {port_for(state_dir)} within {timeout:.0f}s")


def request(
    verb: str,
    state_dir: Path | None = None,
    *,
    autostart: bool = True,
    **payload: Any,
) -> dict[str, Any]:
    """Send a verb to the daemon, spawning it first when allowed.

    Fails closed when the daemon is unreachable and autostart is off -- a fallback to raw
    Docker would recreate exactly the unregistered resources bosn exists to eliminate.
    """
    state_dir = state_dir or default_state_dir()
    if not is_serving(state_dir):
        if not autostart:
            raise DaemonError("no bosn daemon is running")
        spawn(state_dir)
    return ipc.send_request(port_for(state_dir), {"verb": verb, **payload})


def stop(state_dir: Path | None = None, *, timeout: float = 10.0) -> bool:
    """Ask a running daemon to stop. True if one was running and went away."""
    state_dir = state_dir or default_state_dir()
    if not is_serving(state_dir):
        return False
    try:
        ipc.send_request(port_for(state_dir), {"verb": "shutdown"}, timeout=timeout)
    except ipc.TransportError:
        pass
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
