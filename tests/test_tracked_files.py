"""The lints' file listing works where git cannot read the checkout: a git
worktree inside the isolated test container (its .git file points outside
the mount)."""

from __future__ import annotations

import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "ci"))
from tracked_files import tracked_files


def write(root: Path, files: dict[str, str]) -> None:
    for name, text in files.items():
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)


def names(root: Path, paths: list[Path]) -> list[str]:
    return sorted(path.relative_to(root).as_posix() for path in paths)


def test_a_checkout_git_cannot_read_is_walked_without_build_outputs(tmp_path: Path) -> None:
    write(
        tmp_path,
        {
            "src/lib.rs": "",
            "ci/tool.py": "",
            "target/debug/build.rs": "",
            ".venv/lib/x.py": "",
            ".cargo/registry/src/crate.rs": "",
        },
    )
    # A worktree's .git file naming a gitdir that is not there.
    (tmp_path / ".git").write_text("gitdir: /elsewhere/.git/worktrees/x\n")
    assert names(tmp_path, tracked_files(tmp_path)) == ["ci/tool.py", "src/lib.rs"]
    assert names(tmp_path, tracked_files(tmp_path, "*.rs")) == ["src/lib.rs"]


def test_a_readable_checkout_lists_only_tracked_files(tmp_path: Path) -> None:
    write(tmp_path, {"src/lib.rs": "", "scratch.rs": ""})
    subprocess.run(["git", "init", "-q"], cwd=tmp_path, check=True)
    subprocess.run(["git", "add", "src/lib.rs"], cwd=tmp_path, check=True)
    assert names(tmp_path, tracked_files(tmp_path, "*.rs")) == ["src/lib.rs"]
