"""Per-user login launchers for the maintenance daemon."""

from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

NAME = "bosn-daemon"
MAINTENANCE_INTERVAL_SECONDS = 300


def command() -> list[str]:
    return [sys.executable, "-m", "bosn", "__daemon"]


def path(*, platform: str | None = None, home: Path | None = None) -> Path:
    platform = platform or sys.platform
    home = home or Path.home()
    if platform.startswith("win"):
        appdata = Path(os.environ.get("APPDATA", home / "AppData" / "Roaming"))
        return appdata / "Microsoft" / "Windows" / "Start Menu" / "Programs" / "Startup"
    if platform == "darwin":
        return home / "Library" / "LaunchAgents" / "io.github.zackees.bosn.plist"
    return home / ".config" / "systemd" / "user" / "bosn-daemon.service"


def enable(*, platform: str | None = None, home: Path | None = None) -> Path:
    """Install a per-user launcher and return its manifest path."""
    platform = platform or sys.platform
    target = path(platform=platform, home=home)
    invocation = " ".join(f'"{part}"' for part in command())
    if platform.startswith("win"):
        target.mkdir(parents=True, exist_ok=True)
        launcher = target / "bosn-daemon.cmd"
        launcher.write_text(
            "@echo off\r\n:loop\r\n"
            f"{invocation}\r\ntimeout /t {MAINTENANCE_INTERVAL_SECONDS} /nobreak >nul\r\n"
            "goto loop\r\n",
            encoding="utf-8",
        )
        return launcher
    target.parent.mkdir(parents=True, exist_ok=True)
    if platform == "darwin":
        target.write_text(
            '<?xml version="1.0" encoding="UTF-8"?>\n'
            '<plist version="1.0"><dict><key>Label</key>'
            "<string>io.github.zackees.bosn</string><key>ProgramArguments</key><array>"
            f"{''.join(f'<string>{part}</string>' for part in command())}"
            "</array><key>RunAtLoad</key><true/><key>StartInterval</key>"
            f"<integer>{MAINTENANCE_INTERVAL_SECONDS}</integer></dict></plist>\n",
            encoding="utf-8",
        )
        return target
    target.write_text(
        "[Unit]\nDescription=bosn container lifecycle supervisor\n\n"
        + "[Service]\nType=simple\nExecStart="
        + invocation
        + "\n",
        encoding="utf-8",
    )
    timer = target.with_name("bosn-daemon.timer")
    timer.write_text(
        "[Unit]\nDescription=run bosn maintenance regularly\n\n[Timer]\n"
        "OnBootSec=1min\nOnUnitInactiveSec="
        + str(MAINTENANCE_INTERVAL_SECONDS)
        + "s\nPersistent=true\n\n[Install]\nWantedBy=timers.target\n",
        encoding="utf-8",
    )
    subprocess.run(["systemctl", "--user", "enable", "--now", timer.name], check=False)
    return target


def disable(*, platform: str | None = None, home: Path | None = None) -> Path:
    """Remove the per-user launcher; this never touches a system-wide service."""
    platform = platform or sys.platform
    target = path(platform=platform, home=home)
    if platform.startswith("win"):
        target = target / "bosn-daemon.cmd"
    elif platform != "darwin":
        timer = target.with_name("bosn-daemon.timer")
        subprocess.run(["systemctl", "--user", "disable", "--now", timer.name], check=False)
        timer.unlink(missing_ok=True)
    target.unlink(missing_ok=True)
    return target


def manifest_installed(*, platform: str | None = None, home: Path | None = None) -> bool:
    """Whether the recurring scheduler's launcher files are installed.

    This intentionally does not claim the operating-system scheduler is currently
    enabled: a user can disable a systemd timer after its unit files are written.
    """
    platform = platform or sys.platform
    target = path(platform=platform, home=home)
    if platform.startswith("win"):
        return (target / "bosn-daemon.cmd").exists()
    if platform == "darwin":
        return target.exists()
    return target.exists() and target.with_name("bosn-daemon.timer").exists()


def enabled(*, platform: str | None = None, home: Path | None = None) -> bool:
    """Backward-compatible alias for :func:`manifest_installed`."""
    return manifest_installed(platform=platform, home=home)
