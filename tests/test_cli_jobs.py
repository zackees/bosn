"""How the CLI renders a daemon-owned job, and what it exits with.

The rule under test is "fail closed, stay visible": a request that was superseded,
cancelled, or failed must exit non-zero with a message naming what happened. Silently
exiting 0 having done nothing is the failure this whole design is meant to rule out.
"""

from __future__ import annotations

import pytest

from bosn import cli


def events(*items):
    return iter(items)


def end(state: str, **extra):
    return {"event": "end", "final": True, "state": state, **extra}


# -- rendering -------------------------------------------------------------


def test_build_output_goes_to_stderr_not_stdout(capsys) -> None:
    """`bosn run`'s stdout belongs to the command being run, not to build noise."""
    cli._drive_job(events({"event": "log", "line": "step 1/2"}, end("succeeded")))
    captured = capsys.readouterr()
    assert "step 1/2" in captured.err
    assert captured.out == ""


def test_joining_an_existing_build_is_announced(capsys) -> None:
    cli._drive_job(events({"event": "submitted", "job": "j1", "joined": True}, end("succeeded")))
    assert "joined in-flight build j1" in capsys.readouterr().err


def test_being_queued_behind_an_obsolete_build_is_announced(capsys) -> None:
    """The one cost of depth-1 coalescing, so the user is told about it rather than left
    wondering why a build has not started."""
    cli._drive_job(
        events(
            {"event": "submitted", "job": "j2", "joined": False, "disposition": "pending"},
            end("succeeded"),
        )
    )
    assert "queued as j2" in capsys.readouterr().err


def test_a_stream_that_ends_without_a_result_is_an_error() -> None:
    with pytest.raises(cli.JobFailed, match="without a result"):
        cli._drive_job(events({"event": "log", "line": "..."}))


# -- exit codes ------------------------------------------------------------


def test_a_successful_job_yields_the_converge_result() -> None:
    result = cli._result_or_raise(
        end(
            "succeeded",
            result={"stack": "dev", "digest": "sha256:abc", "image_tag": "bosn/dev:abc"},
        )
    )
    assert result.stack == "dev"
    assert result.image_tag == "bosn/dev:abc"


def test_a_superseded_request_exits_nonzero_naming_what_happened() -> None:
    with pytest.raises(cli.JobFailed) as raised:
        cli._result_or_raise(end("superseded", error="superseded: digest aaa replaced by bbb"))
    assert raised.value.exit_code == cli.SUPERSEDED_EXIT
    assert raised.value.exit_code != 0
    assert "superseded" in str(raised.value)
    assert "bbb" in str(raised.value), "the message must name the digest that replaced it"


def test_a_cancelled_build_exits_with_its_own_code() -> None:
    with pytest.raises(cli.JobFailed) as raised:
        cli._result_or_raise(end("cancelled", error="the bosn daemon is shutting down"))
    assert raised.value.exit_code == cli.CANCELLED_EXIT
    assert "shutting down" in str(raised.value)


def test_a_failed_build_exits_one() -> None:
    with pytest.raises(cli.JobFailed) as raised:
        cli._result_or_raise(end("failed", error="build exited 1"))
    assert raised.value.exit_code == cli.BUILD_FAILED_EXIT


def test_the_three_outcomes_are_distinguishable() -> None:
    """An agent has to be able to tell 'your build broke' from 'your request was dropped'."""
    codes = {cli.BUILD_FAILED_EXIT, cli.SUPERSEDED_EXIT, cli.CANCELLED_EXIT}
    assert len(codes) == 3
    assert 0 not in codes
