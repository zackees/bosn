"""Tests that only mean something when executed on their native OS.

Issue #50: the repo claims Windows/macOS/Linux v1 support, but path identity, autostart,
process liveness, and secret-file permissions were validated "mostly by string fixtures,
not by their native shells or service managers." Every test in this module is either
skip-marked to a no-op on platforms where it cannot exercise the real thing, or it spawns
a real shell / real OS probe / real daemon process. None of it is fakeable with fixtures
alone -- that is the point: a green Linux-only run must not be able to hide a Windows- or
macOS-only regression, the way POSIX-style `os.kill(pid, 0)` terminating another Windows
process and `PRAGMA integrity_check` raising on Linux both did this week.
"""

from __future__ import annotations

import contextlib
import os
import re
import shutil
import stat
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

import pytest

from bosn import autostart
from bosn import daemon as daemon_mod
from bosn.daemon import Daemon
from bosn.paths import normalize_workspace_path
from bosn.registry import default_state_dir
from bosn.resources import process_alive, process_start_time

# -- 1. shell/path identity through real native shells ----------------------
#
# normalize_workspace_path() is documented as producing one canonical identity across
# "native Windows, MSYS, WSL and UNC spellings." tests/test_paths.py proves that with
# hardcoded strings; the tests below actually spawn cmd.exe, PowerShell, and Git Bash over
# the SAME real directory and assert their three different spellings normalize to one
# identity. A regression in the MSYS `/c/...` or `//?/` handling could pass every
# string-fixture test while still misidentifying a real Git Bash workspace as a distinct
# cache from its native-spelled counterpart -- this is the only kind of test that catches
# that.

_GIT_BASH = Path(r"C:\Program Files\Git\bin\bash.exe")


def _bash_pwd(directory: Path) -> str:
    """Git Bash's own spelling of `directory`, passed as argv so quoting is bash's job."""
    return subprocess.run(
        [str(_GIT_BASH), "-c", 'cd "$1" && pwd', "_", str(directory)],
        capture_output=True,
        text=True,
        check=True,
    ).stdout.strip()


def _msys_probe_dir(tmp_path: Path) -> tuple[Path, Path | None]:
    """A directory whose Git Bash spelling is the drive-rooted `/c/...` form.

    Prefers pytest's `tmp_path` and returns no cleanup owner for it. Some environments
    point TMPDIR at a directory that Git Bash remaps via /etc/fstab, so bash reports
    `/tmp/...` instead of `/c/Users/...` -- a spelling `normalize_workspace_path` never
    claimed to handle. Only then does this fall back to a home-rooted directory, which is
    never remapped, and return it as the caller's responsibility to remove.
    """
    if re.match(r"^/[a-zA-Z]/", _bash_pwd(tmp_path)):
        return tmp_path, None
    fallback = Path(tempfile.mkdtemp(prefix="bosn_shell_probe_", dir=str(Path.home())))
    return fallback, fallback


@pytest.mark.skipif(
    sys.platform != "win32", reason="native shell identity is only meaningful on Windows"
)
def test_native_windows_shells_agree_on_one_workspace_identity(tmp_path: Path) -> None:
    """cmd.exe, PowerShell, and MSYS Git Bash must resolve to one cache identity.

    The value here is not that a normalizer regression gets caught -- the string fixtures
    in test_paths.py already do that. It is that the *assumption baked into those fixtures*
    gets checked against what the shells really emit. A fixture can only encode spellings
    somebody already thought of; if a shell starts reporting a form nobody anticipated, a
    real workspace silently splits into two caches and every string test still passes.

    The distinct-spelling assertion below is what keeps this honest: cmd and PowerShell
    happen to emit byte-identical strings today, so without Git Bash's `/c/...` form this
    would degrade into asserting `normalize(x) == normalize(x)`.
    """
    if not _GIT_BASH.exists():
        # windows-latest ships Git Bash; its absence in CI is a broken runner, not a
        # reason to report success on a lane that then proves nothing about MSYS.
        if os.environ.get("CI"):
            pytest.fail(f"Git Bash missing at {_GIT_BASH} on a CI runner")
        pytest.skip(f"Git Bash not found at {_GIT_BASH}")

    probe, cleanup = _msys_probe_dir(tmp_path)
    try:
        spellings = [
            subprocess.run(
                ["cmd", "/c", "cd"], cwd=probe, capture_output=True, text=True, check=True
            ).stdout.strip(),
            subprocess.run(
                ["powershell", "-NoProfile", "-Command", "(Get-Location).Path"],
                cwd=probe,
                capture_output=True,
                text=True,
                check=True,
            ).stdout.strip(),
            _bash_pwd(probe),
        ]

        assert len(set(spellings)) >= 2, f"no cross-shell divergence to test: {spellings}"
        identities = {normalize_workspace_path(spelling) for spelling in spellings}
        assert len(identities) == 1, f"{spellings} split into {identities}"
    finally:
        if cleanup is not None:
            shutil.rmtree(cleanup, ignore_errors=True)


# -- 2. native process start identity ----------------------------------------
#
# process_start_time()'s macOS branch has never run on macOS in CI. This does not
# duplicate test_process_start_time_returns_a_plausible_epoch_for_this_process (which only
# checks that a plausible float comes back); it adds the alive/mismatch pairing that
# lease-expiry safety actually depends on (see resources.process_alive and
# lease_is_expired) -- a matching stored start time must read as alive past the TTL, and a
# stale/mismatched one (modeling PID reuse) must not.


@pytest.mark.skipif(
    sys.platform not in ("win32", "linux", "darwin"),
    reason="no native process-start probe implemented for this platform",
)
def test_native_process_start_pairs_matching_and_mismatched_identity_with_liveness() -> None:
    pid = os.getpid()
    start = process_start_time(pid)
    assert start is not None, "native OS probe produced no start time for this live process"

    assert process_alive(pid, proc_start=start) is True
    # Far outside PROCESS_START_TOLERANCE_SECONDS: models a reused pid belonging to a
    # different process, which must never read as the same live holder.
    assert process_alive(pid, proc_start=start - 10_000) is False


# -- 2b. Windows read-only liveness path ------------------------------------
#
# Windows cannot use the POSIX `os.kill(pid, 0)` idiom: CPython may translate it to
# TerminateProcess and kill the process being "checked" with a success code. The native
# tests prove both halves of the replacement: protected PID 4 fails open as live, and a
# normal child remains running after process_alive inspects it.


@pytest.mark.skipif(
    sys.platform != "win32", reason="the access-denied liveness branch is Windows-only"
)
def test_native_windows_access_denied_pid_reads_as_alive() -> None:
    """A live, other-user process must never read as dead.

    PID 4 is the Windows System process: always present, always owned by another user, and
    not queryable by an ordinary user. Access denied proves it exists and must fail open,
    because judging a live holder dead expires its lease under active work.
    """
    assert process_alive(4) is True


@pytest.mark.skipif(sys.platform != "win32", reason="Windows process probing is unique")
def test_native_windows_liveness_probe_does_not_terminate_the_target() -> None:
    child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)"])
    try:
        start = process_start_time(child.pid)
        assert start is not None
        assert process_alive(child.pid, start) is True
        assert child.poll() is None, "a read-only liveness check terminated its target"
    finally:
        child.terminate()
        child.wait(timeout=10)


# -- 3. autostart adapters without mutating system-wide state ----------------
#
# autostart.enable()'s Windows and darwin branches are pure file writes under the injected
# `home`; the Linux branch shells out to `systemctl --user enable --now`, so it is excluded
# here (test_autostart.py already exercises it, safely, only via an explicit platform=
# override with subprocess.run monkeypatched). The tests below call enable()/disable()
# without a platform= override -- i.e. through the real dispatch this OS actually takes at
# runtime -- while still keeping every write confined to tmp_path.


@pytest.mark.skipif(
    not (sys.platform.startswith("win") or sys.platform == "darwin"),
    # Mirrors autostart.path()'s dispatch rather than excluding "linux" by name: the
    # systemctl branch is the *fallback*, so any other POSIX platform takes it too.
    reason="only win/darwin enable() is a pure file write; the rest shell out to systemctl",
)
def test_native_autostart_enable_and_disable_stay_under_the_injected_home(
    monkeypatch, tmp_path: Path
) -> None:
    if sys.platform.startswith("win"):
        # autostart.path() prefers $APPDATA over `home` when the env var is set. Clearing
        # it is what keeps this call from ever touching the real per-user Startup folder.
        monkeypatch.delenv("APPDATA", raising=False)

    installed = autostart.enable(home=tmp_path)
    assert installed.exists()
    assert tmp_path in installed.parents or installed.parent == tmp_path

    if sys.platform.startswith("win"):
        assert installed.suffix == ".cmd"
    elif sys.platform == "darwin":
        assert installed.parent.name == "LaunchAgents"
        assert installed.suffix == ".plist"

    removed = autostart.disable(home=tmp_path)
    assert removed == installed
    assert not installed.exists()


# -- 4. owner-only secret protection ------------------------------------------
#
# Daemon.serve_forever() writes daemon.secret and chmods it 0o600 (bosn/daemon.py). On
# POSIX that permission bit is the entire protection against another local user reading the
# IPC secret; this is the first time it runs natively on macOS. On Windows, POSIX mode bits
# are meaningless (the real control is the ACL, which a hosted runner cannot meaningfully
# assert against), so the Windows-side test instead proves the file lands under the
# per-user state dir rather than some shared/system location.


def _wait_until(predicate, timeout: float = 15.0, interval: float = 0.05) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if predicate():
            return True
        time.sleep(interval)
    return predicate()


@contextlib.contextmanager
def _served_daemon(tmp_path: Path):
    """Yield a live Daemon so assertions run *before* shutdown deletes its files.

    ``Daemon.shutdown()`` unlinks ``daemon.secret`` on exit, so any assertion on the file
    must happen while the daemon is still serving -- returning a path computed after
    teardown would race the cleanup and observe a file that no longer exists.
    """
    daemon = Daemon(state_dir=tmp_path, idle_retire_seconds=3600)
    thread = threading.Thread(target=daemon.serve_forever, daemon=True)
    thread.start()
    try:
        assert _wait_until(lambda: daemon_mod.is_serving(tmp_path)), "daemon never came up"
        yield daemon
    finally:
        daemon.request_stop()
        thread.join(timeout=15)
        daemon.shutdown()


@pytest.mark.skipif(
    os.name == "nt", reason="POSIX file-mode bits are meaningless under Windows ACLs"
)
def test_secret_file_is_owner_only_on_posix(tmp_path: Path) -> None:
    with _served_daemon(tmp_path):
        secret_path = daemon_mod.secret_file(tmp_path)
        assert secret_path.exists()
        mode = stat.S_IMODE(secret_path.stat().st_mode)
        assert mode == 0o600


@pytest.mark.skipif(
    os.name != "nt", reason="Windows: assert location, not ACLs, on a hosted runner"
)
def test_secret_file_lives_under_the_user_state_dir_on_windows(monkeypatch, tmp_path: Path) -> None:
    with _served_daemon(tmp_path):
        secret_path = daemon_mod.secret_file(tmp_path)
        assert secret_path.exists()
        assert secret_path.parent == tmp_path

    # Separately (no daemon involved), prove the *real* default state dir the runtime picks
    # at startup resolves under this user's profile, not a shared/system-wide location.
    monkeypatch.delenv("BOSN_STATE_DIR", raising=False)
    real_default = default_state_dir()
    profile_root = os.environ.get("LOCALAPPDATA") or str(Path.home())
    assert str(real_default).lower().startswith(profile_root.lower())
