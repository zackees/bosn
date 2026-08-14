"""Temporary Windows diagnostic for issue #68 — delete once the answer is recorded.

`process_alive(4)` returns True on a Windows 10 developer box and False on a hosted
`windows-latest` runner. False is the dangerous direction: it means "holder is gone", which
lets a lease expire and its resources be collected, so a live holder could have its
resources reaped. Two candidate explanations have to be told apart before anything is
asserted or "fixed":

1. PID 4 is not the System process on a hosted runner (or is not visible at all), so the
   probe is answering honestly about a process that is not there. Test assumption wrong.
2. `os.kill(4, 0)` raises something that reaches the `except OSError: return False` branch
   instead of a fail-open branch. Real product bug.

This prints the evidence rather than guessing. It never fails the build.
"""

from __future__ import annotations

import os
import subprocess
import sys


def _probe(pid: int) -> None:
    print(f"--- pid {pid} ---")
    try:
        os.kill(pid, 0)
        print("  os.kill(pid, 0): returned with NO exception")
    except KeyboardInterrupt:
        raise
    except BaseException as exc:  # noqa: BLE001 - a diagnostic must report anything at all
        print(f"  os.kill(pid, 0): {type(exc).__name__}: {exc}")
        print(f"    is OSError:      {isinstance(exc, OSError)}")
        print(f"    errno:           {getattr(exc, 'errno', None)}")
        print(f"    winerror:        {getattr(exc, 'winerror', None)}")

    from bosn.resources import process_alive, process_start_time

    print(f"  process_alive({pid}):      {process_alive(pid)}")
    print(f"  process_start_time({pid}): {process_start_time(pid)}")


def main() -> int:
    print(f"platform={sys.platform} executable={sys.executable}")
    print(f"os.getpid()={os.getpid()}")

    if sys.platform == "win32":
        # Does PID 4 exist on this host at all, and what is it?
        listing = subprocess.run(
            ["tasklist", "/FI", "PID eq 4"], capture_output=True, text=True, check=False
        )
        print("--- tasklist /FI 'PID eq 4' ---")
        print(listing.stdout.strip() or "(no output)")
        if listing.stderr.strip():
            print(f"stderr: {listing.stderr.strip()}")

        whoami = subprocess.run(["whoami", "/groups"], capture_output=True, text=True, check=False)
        elevated = "S-1-16-12288" in whoami.stdout  # High Mandatory Level
        print(f"--- elevated (High Mandatory Level): {elevated} ---")

    for pid in (4, os.getpid()):
        _probe(pid)

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
