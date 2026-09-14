"""Unit coverage for the installed Python-to-Rust CLI launcher."""

from __future__ import annotations

from pathlib import Path

import pytest

import bosn.native_cli as native_cli


def test_windows_launcher_waits_for_native_process_and_propagates_exit(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    """Windows must not use exec emulation, which detaches the native child."""

    executable = tmp_path / "bosn-native.exe"
    calls: list[tuple[object, ...]] = []

    monkeypatch.setattr(native_cli.os, "name", "nt")
    monkeypatch.setattr(
        native_cli.os,
        "spawnv",
        lambda mode, path, command: calls.append((mode, path, command)) or 23,
    )
    monkeypatch.setattr(native_cli.os, "execv", lambda *_: pytest.fail("must not exec on Windows"))

    with pytest.raises(SystemExit) as exited:
        native_cli._execute_native(executable, ["--version"])

    assert exited.value.code == 23
    assert calls == [(native_cli.os.P_WAIT, str(executable), [str(executable), "--version"])]


def test_unix_launcher_replaces_itself_with_native_process(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    executable = tmp_path / "bosn-native"
    calls: list[tuple[object, object]] = []

    monkeypatch.setattr(native_cli.os, "name", "posix")
    monkeypatch.setattr(native_cli.os, "execv", lambda path, argv: calls.append((path, argv)))
    monkeypatch.setattr(native_cli.os, "spawnv", lambda *_: pytest.fail("must not spawn on Unix"))

    native_cli._execute_native(executable, ["doctor"])

    assert calls == [(executable, [str(executable), "doctor"])]
