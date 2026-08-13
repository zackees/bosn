"""Options is the fully typed replacement for argparse's dynamic Namespace."""

from __future__ import annotations

import dataclasses
from dataclasses import fields
from pathlib import Path

import pytest

from bosn.cli import build_parser
from bosn.options import Options, from_namespace


def opts(argv: list[str]) -> Options:
    return from_namespace(build_parser().parse_args(argv))


def test_defaults_are_concrete_for_every_field() -> None:
    """No field is left undefined, whichever subparser ran."""
    parsed = opts(["doctor"])
    for f in fields(Options):
        assert hasattr(parsed, f.name)


def test_every_verb_produces_a_complete_options_object() -> None:
    """A Namespace's attributes depend on the subparser; Options' never do."""
    from bosn.cli import VERBS

    for verb in VERBS:
        parsed = opts([verb])
        assert parsed.verb == verb
        assert isinstance(parsed.engine, str)
        assert parsed.dry_run in (True, False)


def test_paths_are_paths_not_strings(tmp_path: Path) -> None:
    parsed = opts(["--state-dir", str(tmp_path), "status"])
    assert isinstance(parsed.state_dir, Path)


def test_absent_paths_are_none_not_empty_strings() -> None:
    parsed = opts(["status"])
    assert parsed.state_dir is None
    assert parsed.manifest is None


def test_verb_local_state_dir_wins_over_the_global_one(tmp_path: Path) -> None:
    global_dir = tmp_path / "global"
    local_dir = tmp_path / "local"
    parsed = from_namespace(
        build_parser().parse_args(
            ["--state-dir", str(global_dir), "__daemon", "--state-dir", str(local_dir)]
        )
    )
    assert parsed.state_dir == local_dir


def test_command_strips_the_argparse_separator() -> None:
    parsed = opts(["run", "--", "echo", "hi"])
    assert parsed.command == ["echo", "hi"]
    assert "--" not in parsed.command


def test_options_are_frozen() -> None:
    parsed = opts(["run"])
    with pytest.raises(dataclasses.FrozenInstanceError):
        parsed.verb = "gc"  # type: ignore[misc]


def test_with_command_returns_a_new_object() -> None:
    parsed = opts(["run"])
    replaced = parsed.with_command(["sh"])
    assert replaced.command == ["sh"]
    assert parsed is not replaced


def test_daemon_numeric_flags_are_typed() -> None:
    parsed = opts(["__daemon", "--port", "5000", "--idle-retire-seconds", "2.5"])
    assert parsed.port == 5000 and isinstance(parsed.port, int)
    assert parsed.idle_retire_seconds == 2.5
    assert isinstance(parsed.idle_retire_seconds, float)


def test_gc_apply_flips_dry_run() -> None:
    assert opts(["gc"]).dry_run is True
    assert opts(["gc", "--apply"]).dry_run is False
