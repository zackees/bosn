"""CLI <-> daemon transport.

A newline-delimited JSON protocol over loopback TCP. Loopback (not a unix socket) because
v1 targets cmd.exe, PowerShell, and MSYS Git Bash on Windows alongside macOS and Linux with
one mechanism. The listening port is published in the daemon's state file.

Most verbs are one request, one reply. Build jobs are the exception: the connection stays
open and the daemon writes a stream of events -- output lines, status changes, heartbeats
-- terminated by one message carrying `"final": true`. That is why reads go through
`MessageStream` rather than a single recv: a stream puts many messages on one socket, and
whatever arrives past the first newline is the next message, not garbage to discard.

Mutating verbs fail closed when the daemon is unreachable -- falling back to raw Docker
would recreate exactly the unregistered resources bosn exists to eliminate.
"""

from __future__ import annotations

import json
import socket
from collections.abc import Generator
from typing import Any

LOOPBACK = "127.0.0.1"
DEFAULT_TIMEOUT = 10.0
# Generous, because a cold build is silent for long stretches -- but not infinite, because
# a client must eventually notice a daemon that died. The daemon heartbeats well inside it.
STREAM_TIMEOUT = 120.0


class TransportError(RuntimeError):
    """The daemon could not be reached or spoke nonsense."""


class MessageStream:
    """Reads newline-delimited JSON objects off a socket, buffering across messages."""

    def __init__(self, sock: socket.socket, *, timeout: float | None = DEFAULT_TIMEOUT) -> None:
        self.sock = sock
        self._buffer = b""
        if timeout is not None:
            self.sock.settimeout(timeout)

    def read(self) -> dict[str, Any] | None:
        """The next message, or None at clean end of stream."""
        while b"\n" not in self._buffer:
            try:
                chunk = self.sock.recv(65536)
            except TimeoutError as exc:
                raise TransportError("timed out waiting for the daemon") from exc
            except OSError as exc:
                raise TransportError(f"connection to the daemon failed: {exc}") from exc
            if not chunk:
                if self._buffer.strip():
                    raise TransportError("daemon closed the connection mid-message")
                return None
            self._buffer += chunk
        raw, self._buffer = self._buffer.split(b"\n", 1)
        if not raw.strip():
            return None
        try:
            message = json.loads(raw.decode("utf-8"))
        except json.JSONDecodeError as exc:
            raise TransportError(f"malformed reply from daemon: {exc}") from exc
        if not isinstance(message, dict):
            raise TransportError("daemon reply was not a JSON object")
        return message

    def write(self, message: dict[str, Any]) -> None:
        self.sock.sendall((json.dumps(message) + "\n").encode("utf-8"))


def send_request(
    port: int,
    request: dict[str, Any],
    *,
    host: str = LOOPBACK,
    timeout: float = DEFAULT_TIMEOUT,
) -> dict[str, Any]:
    """One request, one reply."""
    try:
        with socket.create_connection((host, port), timeout=timeout) as sock:
            stream = MessageStream(sock, timeout=timeout)
            stream.write(request)
            reply = stream.read()
    except TransportError:
        raise
    except OSError as exc:
        raise TransportError(f"daemon unreachable on {host}:{port}: {exc}") from exc
    if reply is None:
        raise TransportError("daemon closed the connection without replying")
    return reply


def stream_request(
    port: int,
    request: dict[str, Any],
    *,
    host: str = LOOPBACK,
    timeout: float = STREAM_TIMEOUT,
) -> Generator[dict[str, Any], None, None]:
    """One request, many replies -- yields events until the daemon sends `final`.

    Disconnecting mid-stream is safe and expected: the job belongs to the daemon, so
    hanging up abandons the *view*, never the work.
    """
    try:
        sock = socket.create_connection((host, port), timeout=timeout)
    except OSError as exc:
        raise TransportError(f"daemon unreachable on {host}:{port}: {exc}") from exc
    with sock:
        stream = MessageStream(sock, timeout=timeout)
        stream.write(request)
        while True:
            message = stream.read()
            if message is None:
                raise TransportError("daemon closed the stream before the job ended")
            yield message
            if message.get("final"):
                return


def read_request(conn: socket.socket) -> dict[str, Any] | None:
    """Server side: read one newline-delimited JSON request, or None on clean EOF."""
    try:
        return MessageStream(conn, timeout=None).read()
    except TransportError:
        return None


def send_response(conn: socket.socket, response: dict[str, Any]) -> None:
    conn.sendall((json.dumps(response) + "\n").encode("utf-8"))
