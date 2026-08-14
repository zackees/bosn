"""Tests that only mean something when executed on their native OS.

Issue #50: the repo claims Windows/macOS/Linux v1 support, but path identity, autostart,
process liveness, and secret-file permissions were validated "mostly by string fixtures,
not by their native shells or service managers." Every test in this module is either
skip-marked to a no-op on platforms where it cannot exercise the real thing, or it spawns
a real shell / real OS probe / real daemon process. None of it is fakeable with fixtures
alone -- that is the point: a green Linux-only run must not be able to hide a Windows- or
macOS-only regression, the way `os.kill(pid, 0)` raising `SystemError` on Windows and
`PRAGMA integrity_check` raising on Linux both did this week.
"""

from __future__ import annotations

import contextlib
import os
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


def _probe_dir() -> Path:
    """A real temp directory outside any MSYS bind-mount alias.

    Deliberately rooted under the user's home rather than the OS temp dir: some Git Bash
    installs remap /tmp to a non-cygdrive mount in /etc/fstab, which would make bash's pwd
    output a spelling this module's normalizer was never meant to understand. Home is not
    remapped, so `/c/Users/...` is what Git Bash actually reports there.
    """
    return Path(tempfile.mkdtemp(prefix="bosn_shell_probe_", dir=str(Path.home())))


@pytest.mark.skipif(
    sys.platform != "win32", reason="native shell identity is only meaningful on Windows"
)
def test_cmd_and_powershell_agree_on_one_path_identity() -> None:
    probe = _probe_dir()
    try:
        cmd_out = subprocess.run(
            ["cmd", "/c", "cd"], cwd=probe, capture_output=True, text=True, check=True
        ).stdout.strip()
        ps_out = subprocess.run(
            ["powershell", "-NoProfile", "-Command", "(Get-Location).Path"],
            cwd=probe,
            capture_output=True,
            text=True,
            check=True,
        ).stdout.strip()

        expected = normalize_workspace_path(cmd_out)
        assert normalize_workspace_path(ps_out) == expected
    finally:
        shutil.rmtree(probe, ignore_errors=True)


@pytest.mark.skipif(
    sys.platform != "win32", reason="native shell identity is only meaningful on Windows"
)
def test_git_bash_msys_spelling_agrees_with_the_native_identity() -> None:
    """Skips gracefully (not a silent pass) when Git Bash is absent from this runner."""
    if not _GIT_BASH.exists():
        pytest.skip(f"Git Bash not found at {_GIT_BASH}")

    probe = _probe_dir()
    try:
        cmd_out = subprocess.run(
            ["cmd", "/c", "cd"], cwd=probe, capture_output=True, text=True, check=True
        ).stdout.strip()
        expected = normalize_workspace_path(cmd_out)

        forward = str(probe).replace("\\", "/")
        bash_out = subprocess.run(
            [str(_GIT_BASH), "-c", f"cd '{forward}' && pwd"],
            capture_output=True,
            text=True,
            check=True,
        ).stdout.strip()

        assert normalize_workspace_path(bash_out) == expected
    finally:
        shutil.rmtree(probe, ignore_errors=True)


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


# -- 3. autostart adapters without mutating system-wide state ----------------
#
# autostart.enable()'s Windows and darwin branches are pure file writes under the injected
# `home`; the Linux branch shells out to `systemctl --user enable --now`, so it is excluded
# here (test_autostart.py already exercises it, safely, only via an explicit platform=
# override with subprocess.run monkeypatched). The tests below call enable()/disable()
# without a platform= override -- i.e. through the real dispatch this OS actually takes at
# runtime -- while still keeping every write confined to tmp_path.


@pytest.mark.skipif(
    sys.platform == "linux",
    reason="linux enable() shells out to systemctl; already covered safely in test_autostart.py",
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
