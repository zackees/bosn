"""The core-workspace webview gate flags toolkits in a lockfile."""

from __future__ import annotations

import importlib.util
import sys
from pathlib import Path

SPEC = importlib.util.spec_from_file_location(
    "lint_no_webview", Path("ci/lint_no_webview_in_core.py")
)
assert SPEC is not None and SPEC.loader is not None
lint = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = lint
SPEC.loader.exec_module(lint)


def test_webview_toolkits_in_the_lock_are_reported(tmp_path: Path) -> None:
    lock = tmp_path / "Cargo.lock"
    lock.write_text('[[package]]\nname = "serde"\n\n[[package]]\nname = "webkit2gtk"\n')
    assert lint.offenders(lock) == ["webkit2gtk"]
    lock.write_text('[[package]]\nname = "serde"\n')
    assert lint.offenders(lock) == []


def test_the_real_core_lock_is_clean() -> None:
    assert lint.offenders(Path("Cargo.lock")) == []
