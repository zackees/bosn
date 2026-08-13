"""The KeyboardInterrupt checker itself is tested -- a silent linter is worse than none."""

from __future__ import annotations

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "ci"))

import lint_kbi  # noqa: E402


def codes(source: str) -> list[str]:
    return [v.code for v in lint_kbi.check_source(Path("sample.py"), source)]


def test_broad_except_without_keyboard_interrupt_is_flagged() -> None:
    assert codes("try:\n    work()\nexcept Exception:\n    log()\n") == ["KBI001"]


def test_base_exception_is_flagged_too() -> None:
    assert codes("try:\n    work()\nexcept BaseException:\n    log()\n") == ["KBI001"]


def test_broad_except_with_a_reraising_kbi_sibling_is_clean() -> None:
    source = (
        "try:\n    work()\nexcept KeyboardInterrupt:\n    raise\nexcept Exception:\n    log()\n"
    )
    assert codes(source) == []


def test_kbi_handler_that_swallows_the_interrupt_is_flagged() -> None:
    source = (
        "try:\n    work()\nexcept KeyboardInterrupt:\n    log()\nexcept Exception:\n    log()\n"
    )
    assert codes(source) == ["KBI002"]


def test_kbi_handler_calling_interrupt_main_is_accepted() -> None:
    source = (
        "import _thread\n"
        "try:\n"
        "    work()\n"
        "except KeyboardInterrupt:\n"
        "    _thread.interrupt_main()\n"
        "except Exception:\n"
        "    log()\n"
    )
    assert codes(source) == []


def test_bare_except_without_reraise_is_flagged() -> None:
    assert codes("try:\n    work()\nexcept:\n    log()\n") == ["KBI003"]


def test_bare_except_that_reraises_is_accepted() -> None:
    assert codes("try:\n    work()\nexcept:\n    raise\n") == []


def test_narrow_excepts_are_never_flagged() -> None:
    assert codes("try:\n    work()\nexcept ValueError:\n    log()\n") == []


def test_noqa_suppresses_the_finding() -> None:
    assert codes("try:\n    work()\nexcept Exception:  # noqa: KBI001\n    log()\n") == []


def test_bare_noqa_suppresses_the_finding() -> None:
    assert codes("try:\n    work()\nexcept Exception:  # noqa\n    log()\n") == []


def test_noqa_for_a_different_code_does_not_suppress() -> None:
    assert codes("try:\n    work()\nexcept Exception:  # noqa: E501\n    log()\n") == ["KBI001"]


def test_the_repo_itself_is_clean() -> None:
    """The checker must pass on bosn's own source, or it is decoration."""
    files = lint_kbi.iter_python_files(["src", "ci", "tests"], [".venv", "__pycache__"])
    assert files, "checker found no files to scan"
    violations = [v for path in files for v in lint_kbi.check_file(path)]
    assert violations == [], "\n".join(v.render() for v in violations)
