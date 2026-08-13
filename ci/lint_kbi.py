"""KeyboardInterrupt (Ctrl-C) lint checks.

Ctrl-C and Python do not mix well: a broad `except Exception` silently swallows the user's
interrupt, and a process that eats SIGINT becomes unkillable-by-habit. This is an AST
checker in the shape FastLED settled on after abandoning its flake8 plugin -- ruff's BLE001
is not precise enough, since the problem is not the blind except itself but the *missing
KeyboardInterrupt path* next to it.

Error codes:
    KBI001  try/except catches Exception or BaseException (or bare except) with no
            sibling `except KeyboardInterrupt` handler
    KBI002  an `except KeyboardInterrupt` handler neither re-raises nor calls
            _thread.interrupt_main()
    KBI003  a bare `except:` that does not re-raise at all

Suppress a line with `# noqa` or `# noqa: KBI001`.
"""

from __future__ import annotations

import argparse
import ast
import re
import sys
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DEFAULT_PATHS = ["src", "ci", "tests"]
NOQA_RE = re.compile(r"#\s*noqa(?::\s*(?P<codes>[A-Z]+\d+(?:\s*,\s*[A-Z]+\d+)*))?", re.IGNORECASE)

BROAD = {"Exception", "BaseException"}


@dataclass(frozen=True)
class Violation:
    path: Path
    line: int
    code: str
    message: str

    def render(self) -> str:
        return f"{self.path}:{self.line}: {self.code} {self.message}"


def _suppressed(source_lines: list[str], line: int, code: str) -> bool:
    if line - 1 >= len(source_lines):
        return False
    match = NOQA_RE.search(source_lines[line - 1])
    if match is None:
        return False
    codes = match.group("codes")
    if codes is None:
        return True
    return code.upper() in {part.strip().upper() for part in codes.split(",")}


def _handler_names(handler: ast.ExceptHandler) -> set[str]:
    node = handler.type
    if node is None:
        return set()
    parts = node.elts if isinstance(node, ast.Tuple) else [node]
    names: set[str] = set()
    for part in parts:
        if isinstance(part, ast.Name):
            names.add(part.id)
        elif isinstance(part, ast.Attribute):
            names.add(part.attr)
    return names


def _reraises(handler: ast.ExceptHandler) -> bool:
    return any(isinstance(node, ast.Raise) for node in ast.walk(handler))


def _interrupts_main(handler: ast.ExceptHandler) -> bool:
    for node in ast.walk(handler):
        if not isinstance(node, ast.Call):
            continue
        func = node.func
        name = func.attr if isinstance(func, ast.Attribute) else getattr(func, "id", "")
        if name in {"interrupt_main", "notify_main_thread", "handle_keyboard_interrupt"}:
            return True
    return False


class _Visitor(ast.NodeVisitor):
    def __init__(self, path: Path, source_lines: list[str]) -> None:
        self.path = path
        self.source_lines = source_lines
        self.violations: list[Violation] = []

    def _add(self, node: ast.AST, code: str, message: str) -> None:
        line = getattr(node, "lineno", 0)
        if _suppressed(self.source_lines, line, code):
            return
        self.violations.append(Violation(self.path, line, code, message))

    def visit_Try(self, node: ast.Try) -> None:
        handles_kbi = any("KeyboardInterrupt" in _handler_names(h) for h in node.handlers)

        for handler in node.handlers:
            names = _handler_names(handler)

            if "KeyboardInterrupt" in names:
                if not _reraises(handler) and not _interrupts_main(handler):
                    self._add(
                        handler,
                        "KBI002",
                        "KeyboardInterrupt handler must re-raise or call "
                        "_thread.interrupt_main(); swallowing Ctrl-C strands the user",
                    )
                continue

            if not names:  # bare `except:`
                if not _reraises(handler):
                    self._add(
                        handler,
                        "KBI003",
                        "bare `except:` swallows KeyboardInterrupt; catch Exception and "
                        "re-raise KeyboardInterrupt explicitly",
                    )
                continue

            if names & BROAD and not handles_kbi:
                caught = ", ".join(sorted(names & BROAD))
                self._add(
                    handler,
                    "KBI001",
                    f"`except {caught}` has no sibling `except KeyboardInterrupt`; "
                    "add one that re-raises so Ctrl-C is not swallowed",
                )

        self.generic_visit(node)


def check_source(path: Path, source: str) -> list[Violation]:
    try:
        tree = ast.parse(source, filename=str(path))
    except SyntaxError as exc:
        return [Violation(path, exc.lineno or 0, "KBI000", f"syntax error: {exc.msg}")]
    visitor = _Visitor(path, source.splitlines())
    visitor.visit(tree)
    return visitor.violations


def check_file(path: Path) -> list[Violation]:
    return check_source(path, path.read_text(encoding="utf-8"))


def iter_python_files(paths: list[str], exclude: list[str]) -> list[Path]:
    found: list[Path] = []
    for raw in paths:
        target = Path(raw)
        if not target.is_absolute():
            target = ROOT / target
        if target.is_file() and target.suffix == ".py":
            found.append(target)
        elif target.is_dir():
            found.extend(sorted(target.rglob("*.py")))
    return [p for p in found if not any(token in p.as_posix() for token in exclude)]


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("paths", nargs="*", default=DEFAULT_PATHS)
    parser.add_argument("--exclude", nargs="*", default=[".venv", "__pycache__"])
    ns = parser.parse_args(argv)

    violations: list[Violation] = []
    for path in iter_python_files(ns.paths or DEFAULT_PATHS, ns.exclude):
        violations.extend(check_file(path))

    for violation in sorted(violations, key=lambda v: (str(v.path), v.line)):
        print(violation.render())

    if violations:
        print(f"\n{len(violations)} KeyboardInterrupt issue(s) found", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
