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


def test_command_preserves_a_separator_that_belongs_to_the_child_argv() -> None:
    parsed = opts(["run", "--", "sh", "-c", "printf '%s\\n' \"$1\"", "--", "lint"])
    assert parsed.command == ["sh", "-c", "printf '%s\\n' \"$1\"", "--", "lint"]


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


def test_all_documented_policy_flags_are_typed() -> None:
    parsed = opts(
        [
            "--container-idle-stop",
            "1",
            "--container-remove",
            "2",
            "--warm-volume-ttl",
            "3",
            "--superseded-cap",
            "4",
            "--shared-cache-ceiling",
            "5",
            "--run-max-duration",
            "6",
            "--idle-retire-seconds",
            "7",
            "--max-builds",
            "8",
            "--build-ttl-seconds",
            "9",
            "status",
        ]
    )
    assert [
        parsed.container_idle_stop,
        parsed.container_remove,
        parsed.warm_volume_ttl,
        parsed.superseded_cap,
        parsed.shared_cache_ceiling,
        parsed.run_max_duration,
        parsed.idle_retire_seconds,
        parsed.max_builds,
        parsed.build_ttl_seconds,
    ] == [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8, 9.0]


def test_policy_flags_work_before_or_after_the_verb() -> None:
    assert opts(["--run-max-duration", "12", "run"]).run_max_duration == 12
    assert opts(["run", "--run-max-duration", "12", "--", "true"]).run_max_duration == 12
    assert opts(["--idle-retire-seconds", "12", "__daemon"]).idle_retire_seconds == 12


def test_gc_apply_flips_dry_run() -> None:
    assert opts(["gc"]).dry_run is True
    assert opts(["gc", "--apply"]).dry_run is False


def test_adopt_can_select_a_lost_registry_identity() -> None:
    assert opts(["adopt", "--from-registry", "lost-registry"]).source_registry == "lost-registry"


def test_adopt_can_select_exact_resources_for_transfer() -> None:
    assert opts(["adopt", "--transfer", "volume:cache"]).transfer == ("volume:cache",)


def test_adopt_legacy_family_defaults_to_none() -> None:
    assert opts(["adopt"]).legacy is None


def test_adopt_legacy_family_is_captured() -> None:
    assert opts(["adopt", "--legacy", "soldr"]).legacy == "soldr"


def test_adopt_yes_defaults_to_false() -> None:
    assert opts(["adopt"]).yes is False


def test_adopt_yes_flag_is_captured() -> None:
    assert opts(["adopt", "--legacy", "clud", "--yes"]).yes is True


def test_reconcile_volume_accepts_only_a_logical_manifest_volume() -> None:
    parsed = opts(["reconcile-volume", "--stack", "perf", "--volume", "target", "--apply", "--yes"])
    assert parsed.stack == "perf"
    assert parsed.volume == "target"
    assert parsed.reconcile_apply is True
    assert parsed.yes is True
