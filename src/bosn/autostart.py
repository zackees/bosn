"""Per-user login launchers for the maintenance daemon."""

from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

NAME = "bosn-daemon"


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
        launcher.write_text(f"@echo off\r\n{invocation}\r\n", encoding="utf-8")
        return launcher
    target.parent.mkdir(parents=True, exist_ok=True)
    if platform == "darwin":
        target.write_text(
            '<?xml version="1.0" encoding="UTF-8"?>\n'
            '<plist version="1.0"><dict><key>Label</key>'
            "<string>io.github.zackees.bosn</string><key>ProgramArguments</key><array>"
            f"{''.join(f'<string>{part}</string>' for part in command())}"
            "</array><key>RunAtLoad</key><true/></dict></plist>\n",
            encoding="utf-8",
        )
        return target
    target.write_text(
        "[Unit]\nDescription=bosn container lifecycle supervisor\n\n"
        "[Service]\nType=simple\nExecStart=" + invocation + "\nRestart=on-failure\n\n"
        "[Install]\nWantedBy=default.target\n",
        encoding="utf-8",
    )
    subprocess.run(["systemctl", "--user", "enable", "--now", target.name], check=False)
    return target


def disable(*, platform: str | None = None, home: Path | None = None) -> Path:
    """Remove the per-user launcher; this never touches a system-wide service."""
    platform = platform or sys.platform
    target = path(platform=platform, home=home)
    if platform.startswith("win"):
        target = target / "bosn-daemon.cmd"
    elif platform != "darwin":
        subprocess.run(["systemctl", "--user", "disable", "--now", target.name], check=False)
    target.unlink(missing_ok=True)
    return target
