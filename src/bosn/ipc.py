"""CLI <-> daemon transport.

A newline-delimited JSON protocol over loopback TCP. Loopback (not a unix socket) because
v1 targets cmd.exe, PowerShell, and MSYS Git Bash on Windows alongside macOS and Linux with
one mechanism. The listening port is published in the daemon's state file.

Mutating verbs fail closed when the daemon is unreachable -- falling back to raw Docker
would recreate exactly the unregistered resources bosn exists to eliminate.
"""

from __future__ import annotations

import json
import socket
from typing import Any

LOOPBACK = "127.0.0.1"
DEFAULT_TIMEOUT = 10.0


class TransportError(RuntimeError):
    """The daemon could not be reached or spoke nonsense."""


def send_request(
    port: int,
    request: dict[str, Any],
    *,
    host: str = LOOPBACK,
    timeout: float = DEFAULT_TIMEOUT,
) -> dict[str, Any]:
    payload = (json.dumps(request) + "\n").encode("utf-8")
    try:
        with socket.create_connection((host, port), timeout=timeout) as sock:
            sock.settimeout(timeout)
            sock.sendall(payload)
            return _read_message(sock)
    except OSError as exc:
        raise TransportError(f"daemon unreachable on {host}:{port}: {exc}") from exc


def _read_message(sock: socket.socket) -> dict[str, Any]:
    chunks: list[bytes] = []
    while True:
        chunk = sock.recv(4096)
        if not chunk:
            break
        chunks.append(chunk)
        if b"\n" in chunk:
            break
    raw = b"".join(chunks).split(b"\n", 1)[0]
    if not raw:
        raise TransportError("daemon closed the connection without replying")
    try:
        message = json.loads(raw.decode("utf-8"))
    except json.JSONDecodeError as exc:
        raise TransportError(f"malformed reply from daemon: {exc}") from exc
    if not isinstance(message, dict):
        raise TransportError("daemon reply was not a JSON object")
    return message


def read_request(conn: socket.socket) -> dict[str, Any] | None:
    """Server side: read one newline-delimited JSON request, or None on clean EOF."""
    try:
        return _read_message(conn)
    except TransportError:
        return None


def send_response(conn: socket.socket, response: dict[str, Any]) -> None:
    conn.sendall((json.dumps(response) + "\n").encode("utf-8"))
