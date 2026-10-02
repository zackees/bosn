"""The file-length gate counts the right files and fails at the limit."""

from __future__ import annotations

import importlib.util
import subprocess
import sys
from pathlib import Path

SPEC = importlib.util.spec_from_file_location("lint_file_length", Path("ci/lint_file_length.py"))
assert SPEC is not None and SPEC.loader is not None
lint = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = lint
SPEC.loader.exec_module(lint)


def repo(tmp_path: Path, files: dict[str, str]) -> Path:
    for name, text in files.items():
        path = tmp_path / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
    subprocess.run(["git", "init", "-q"], cwd=tmp_path, check=True)
    subprocess.run(["git", "add", "-A"], cwd=tmp_path, check=True)
    return tmp_path


def test_files_at_the_limit_fail_and_below_it_pass(tmp_path: Path) -> None:
    root = repo(
        tmp_path,
        {
            "src/big.rs": "x\n" * lint.MAX_LINES,
            "src/ok.rs": "x\n" * (lint.MAX_LINES - 1),
            "tool.py": "x\n" * (lint.MAX_LINES + 5),
            "run": "#!/bin/bash\n" + "x\n" * lint.MAX_LINES,
            "notes.md": "x\n" * 5000,
            "_vender/thirdparty.rs": "x\n" * 5000,
        },
    )
    found = dict(lint.violations(lint.tracked_files(root), root))
    assert found == {"src/big.rs": 1000, "tool.py": 1005, "run": 1001}
    assert lint.main(["--root", str(root)]) == 1


def test_a_clean_tree_passes(tmp_path: Path) -> None:
    root = repo(tmp_path, {"a.rs": "fn main() {}\n", "b.sh": "echo hi\n"})
    assert lint.main(["--root", str(root)]) == 0
