from __future__ import annotations

import pathlib
from pathlib import Path

import pytest

from bosn import shims


@pytest.fixture(autouse=True)
def _isolated_shim_dir(monkeypatch, tmp_path: Path) -> Path:
    """Route every default-directory lookup at a throwaway path.

    `BOSN_SHIM_DIR` is the override `default_directory()` checks first (mirroring
    `bosn.config`'s `BOSN_CONFIG`), so this is what keeps every test in this file off the
    real user profile -- a bug here would mean tests writing `docker.cmd`/`docker` next to
    a developer's actual Docker install.
    """
    directory = tmp_path / "shims"
    monkeypatch.setenv("BOSN_SHIM_DIR", str(directory))
    return directory


def _docker_name() -> str:
    return shims._filename("docker")


def test_install_creates_shim_status_reports_it_uninstall_restores_prior_state(
    monkeypatch, tmp_path: Path
) -> None:
    monkeypatch.setenv("PATH", str(tmp_path / "empty-path"))
    target = shims.default_directory() / _docker_name()
    assert not target.exists()

    installed = shims.install()
    assert installed.installed is True
    assert "docker" in installed.shimmed
    assert target.exists()

    reported = shims.status()
    assert reported.installed is True
    assert reported.shimmed == installed.shimmed
    assert reported.conflicts == ()

    removed = shims.uninstall()
    assert removed.installed is False
    assert removed.shimmed == ()
    # Prior state (nothing at this path) is restored exactly.
    assert not target.exists()


def test_install_is_idempotent(monkeypatch, tmp_path: Path) -> None:
    monkeypatch.setenv("PATH", str(tmp_path / "empty-path"))
    first = shims.install()
    target = shims.default_directory() / _docker_name()
    first_text = target.read_text(encoding="utf-8")

    second = shims.install()
    second_text = target.read_text(encoding="utf-8")

    assert first.shimmed == second.shimmed == ("docker", "docker-compose")
    assert first_text == second_text


def test_uninstall_on_clean_system_is_a_noop_not_an_error(monkeypatch, tmp_path: Path) -> None:
    monkeypatch.setenv("PATH", str(tmp_path / "empty-path"))
    # Directory does not even exist yet.
    result = shims.uninstall()
    assert result.installed is False
    assert result.shimmed == ()

    # Nor does a second uninstall raise once the directory exists but is empty.
    shims.default_directory().mkdir(parents=True)
    result_again = shims.uninstall()
    assert result_again.installed is False


def test_preexisting_foreign_file_is_a_conflict_never_touched(monkeypatch, tmp_path: Path) -> None:
    monkeypatch.setenv("PATH", str(tmp_path / "empty-path"))
    directory = shims.default_directory()
    directory.mkdir(parents=True)
    foreign = directory / _docker_name()
    original_content = "#!/bin/sh\necho 'this is somebody else's docker'\n"
    foreign.write_text(original_content, encoding="utf-8")

    installed = shims.install()
    assert "docker" in installed.conflicts
    assert "docker" not in installed.shimmed
    assert foreign.read_text(encoding="utf-8") == original_content

    removed = shims.uninstall()
    assert "docker" in removed.conflicts
    assert foreign.exists()
    assert foreign.read_text(encoding="utf-8") == original_content


def test_status_never_raises_on_missing_directory(monkeypatch, tmp_path: Path) -> None:
    monkeypatch.setenv("PATH", str(tmp_path / "empty-path"))
    missing = tmp_path / "does" / "not" / "exist"
    monkeypatch.setenv("BOSN_SHIM_DIR", str(missing))
    result = shims.status()
    assert result.installed is False
    assert result.shimmed == ()
    assert result.conflicts == ()
    assert isinstance(result.detail, str) and result.detail


def test_status_never_raises_on_unreadable_directory(monkeypatch, tmp_path: Path) -> None:
    monkeypatch.setenv("PATH", str(tmp_path / "empty-path"))
    directory = shims.default_directory()
    directory.mkdir(parents=True)

    real_is_dir = pathlib.Path.is_dir

    def boom(self: pathlib.Path) -> bool:
        if self == directory:
            raise PermissionError("denied")
        return real_is_dir(self)

    monkeypatch.setattr(pathlib.Path, "is_dir", boom)

    result = shims.status()
    assert result.installed is False
    assert result.shimmed == ()


def test_status_never_raises_with_no_docker_anywhere_on_path(monkeypatch, tmp_path: Path) -> None:
    empty_path_dir = tmp_path / "empty-path"
    empty_path_dir.mkdir()
    monkeypatch.setenv("PATH", str(empty_path_dir))
    result = shims.status()
    assert result.real_engine is None
    assert isinstance(result.detail, str) and result.detail


def test_generated_shim_actually_references_bosn_docker(monkeypatch, tmp_path: Path) -> None:
    monkeypatch.setenv("PATH", str(tmp_path / "empty-path"))
    shims.install()
    target = shims.default_directory() / _docker_name()
    content = target.read_text(encoding="utf-8")
    assert "bosn-docker" in content
    # Not just mentioned in a comment: it is the command the shim actually invokes.
    invocation_lines = [
        line
        for line in content.splitlines()
        if "bosn-docker" in line and not line.strip().startswith(("#", "rem"))
    ]
    assert invocation_lines, f"no non-comment line invokes bosn-docker in: {content!r}"

    compose_target = shims.default_directory() / shims._filename("docker-compose")
    compose_content = compose_target.read_text(encoding="utf-8")
    assert "bosn-compose" in compose_content


def test_status_real_engine_skips_the_shim_directory_itself(monkeypatch, tmp_path: Path) -> None:
    """`status()`'s real_engine must not report bosn's own shim back as "the real docker".

    If it did, `doctor` would tell the user their real engine is a file that just execs
    `bosn-docker` -- exactly the layout #46 warns against: bosn losing any way to reach
    the actual engine once its own shim is the first (and only, in this test) result PATH
    resolution would find.
    """
    directory = shims.default_directory()
    directory.mkdir(parents=True)
    shim_name = _docker_name()
    (directory / shim_name).write_text("@echo off\r\n", encoding="utf-8")
    if not shim_name.endswith((".cmd", ".bat", ".exe")):
        (directory / shim_name).chmod(0o755)

    # Only the shim directory is on PATH -- no real engine reachable anywhere.
    monkeypatch.setenv("PATH", str(directory))
    result = shims.status()
    assert result.real_engine is None


def test_status_real_engine_finds_the_real_docker_past_the_shim(
    monkeypatch, tmp_path: Path
) -> None:
    directory = shims.default_directory()
    directory.mkdir(parents=True)
    real_dir = tmp_path / "real-docker-bin"
    real_dir.mkdir()
    shim_name = _docker_name()
    real_binary = real_dir / shim_name
    real_binary.write_text("@echo off\r\n", encoding="utf-8")
    if not shim_name.endswith((".cmd", ".bat", ".exe")):
        real_binary.chmod(0o755)

    import os

    monkeypatch.setenv("PATH", str(directory) + os.pathsep + str(real_dir))
    result = shims.status()
    assert result.real_engine is not None
    assert result.real_engine.resolve() == real_binary.resolve()
