"""Phase 0 smoke tests: the console entry points exist and report a version."""

from __future__ import annotations

import subprocess
import sys

import pytest

import bosn
from bosn import cli, docker_cli


def test_version_string_is_populated() -> None:
    assert bosn.__version__
    assert bosn.__version__[0].isdigit()


@pytest.mark.parametrize("module", [cli, docker_cli])
def test_version_flag_exits_zero_and_prints_version(module, capsys) -> None:
    with pytest.raises(SystemExit) as exc:
        module.main(["--version"])
    assert exc.value.code == 0
    assert bosn.__version__ in capsys.readouterr().out


@pytest.mark.parametrize("module", [cli, docker_cli])
def test_bare_invocation_prints_help(module, capsys) -> None:
    assert module.main([]) == 0
    assert module.build_parser().prog in capsys.readouterr().out


def test_python_dash_m_entry_point() -> None:
    result = subprocess.run(
        [sys.executable, "-m", "bosn", "--version"],
        capture_output=True,
        text=True,
    )
    assert result.returncode == 0
    assert bosn.__version__ in result.stdout
