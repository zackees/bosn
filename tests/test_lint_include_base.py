"""The include-base gate: an embedded file's directory is written once per file."""

from __future__ import annotations

import importlib.util
import subprocess
import sys
from pathlib import Path

# The lints import their sibling helper (ci/tracked_files.py).
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "ci"))

SPEC = importlib.util.spec_from_file_location("lint_include_base", Path("ci/lint_include_base.py"))
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


def found(root: Path) -> list[str]:
    return [problem for _, problem in lint.violations(lint.tracked_files(root), root)]


def test_a_directory_repeated_across_literal_includes_fails(tmp_path: Path) -> None:
    root = repo(
        tmp_path,
        {
            "crates/a/src/page.rs": 'const A: &str = include_str!("assets/a.js");\n'
            'const B: &[u8] = include_bytes!("assets/b.png");\n',
        },
    )
    assert found(root) == ["`assets/` is written in 2 includes; reach it through one base macro"]


def test_one_file_included_twice_fails(tmp_path: Path) -> None:
    root = repo(
        tmp_path,
        {
            "crates/a/src/t.rs": 'let a = include_str!("../tests/x.json");\n'
            'let b = include_str!("../tests/x.json");\n',
        },
    )
    assert found(root) == ["`../tests/` is written in 2 includes; reach it through one base macro"]


def test_a_base_macro_and_single_includes_pass(tmp_path: Path) -> None:
    root = repo(
        tmp_path,
        {
            "crates/a/src/page.rs": "macro_rules! asset {\n"
            '    ($name:literal) => { include_str!(concat!("assets/", $name)) };\n'
            "}\n"
            'const A: &str = asset!("a.js");\n'
            'const B: &str = asset!("b.js");\n'
            'const S: &str = include_str!("schema.json");\n'
            'const F: &str = include_str!("../tests/fixture.json");\n',
            "_vender/x/src/lib.rs": 'include_str!("d/a"); include_str!("d/b");\n',
        },
    )
    assert found(root) == []


def test_a_base_outside_src_fails_because_publishing_cannot_carry_it(tmp_path: Path) -> None:
    root = repo(
        tmp_path,
        {
            "crates/a/src/t.rs": "macro_rules! fx {\n"
            '    ($n:literal) => { include_str!(concat!("../tests/", $n)) };\n'
            "}\n",
        },
    )
    assert found(root) == [
        "base `../tests/` leaves src/; publish_amalgamate only relocates literal includes"
    ]


def test_the_repository_passes() -> None:
    assert lint.main(["--root", "."]) == 0
