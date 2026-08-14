from __future__ import annotations

from pathlib import Path

import pytest

from bosn import autostart


@pytest.mark.parametrize("platform", ["win32", "darwin", "linux"])
def test_per_user_login_launcher_can_be_enabled_and_removed(
    monkeypatch, tmp_path: Path, platform: str
) -> None:
    """Every supported OS has a reversible, per-user login launcher."""
    monkeypatch.setattr(autostart.subprocess, "run", lambda *_args, **_kwargs: None)
    installed = autostart.enable(platform=platform, home=tmp_path)
    assert installed.exists()
    text = installed.read_text(encoding="utf-8")
    assert "bosn" in text
    assert autostart.disable(platform=platform, home=tmp_path) == installed
    assert not installed.exists()
