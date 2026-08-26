import json
import os
import re
from pathlib import Path
from typing import Any

import pytest

from bosn import daemon, docker_cli, ipc
from bosn.docker_cli import (
    DockerFrontDoorError,
    _compose_overlay,
    _run_compose,
    compose_to_manifest,
    main,
)
from bosn.frontdoor import COMPOSE_COMMANDS, Category, VerbSpec
from bosn.manifest import load
from bosn.registry import Registry


def test_init_translates_compose_images(tmp_path: Path) -> None:
    compose = tmp_path / "compose.yaml"
    compose.write_text("services:\n  app:\n    image: alpine:3.20\n", encoding="utf-8")
    assert 'image = "alpine:3.20"' in compose_to_manifest(compose)


def test_init_preserves_short_form_tmpfs_and_accepts_project_name(tmp_path: Path) -> None:
    """Issue #115: the generated native stack must keep tmpfs's disposable semantics."""
    compose = tmp_path / "compose.yaml"
    compose.write_text(
        "name: twp-e2e\n"
        "services:\n"
        "  mysql:\n"
        "    image: mysql:8.0.45\n"
        "    tmpfs: [/var/lib/mysql]\n",
        encoding="utf-8",
    )

    generated = compose_to_manifest(compose)

    assert 'tmpfs = ["/var/lib/mysql"]' in generated
    manifest_path = tmp_path / "bosn.toml"
    manifest_path.write_text(generated, encoding="utf-8")
    assert load(manifest_path).stacks["mysql"].tmpfs[0].destination == "/var/lib/mysql"


def test_compose_overlay_has_the_complete_label_contract(tmp_path: Path) -> None:
    compose = tmp_path / "compose.yaml"
    compose.write_text("services:\n  app:\n    image: alpine\n", encoding="utf-8")
    with Registry(tmp_path / "registry.sqlite3") as registry:
        overlay = _compose_overlay(registry, compose)
        try:
            text = overlay.read_text(encoding="utf-8")
            assert "com.zackees.bosn.registry" in text
            assert "com.zackees.bosn.workspace" in text
        finally:
            overlay.unlink()


def test_compose_subset_refuses_unknown_argument(tmp_path: Path) -> None:
    with pytest.raises(DockerFrontDoorError, match="--scale"):
        _run_compose("up", tmp_path / "compose.yaml", ["--scale"])


# -- every Compose-created resource is governed (#48) ------------------------


def _labelled(text: str) -> dict[str, dict[str, str]]:
    """Map each overlay entry to its label dict, ignoring how scalars are quoted.

    Deliberately quoting-agnostic: an assertion that pins `kind: "volume"` passes only
    for one quoting style, so it would have enshrined the double-quoted emission that
    made this very overlay unparseable on Windows.
    """
    entries: dict[str, dict[str, str]] = {}
    current: str | None = None
    for raw in text.splitlines():
        if not raw.strip():
            continue
        entry = re.match(r"^  ([A-Za-z0-9_-]+):\s*$", raw)
        if entry:
            current = entry.group(1)
            entries.setdefault(entry.group(1), {})
            continue
        pair = re.match(r"^      com\.zackees\.bosn\.([a-z]+):\s*(.*)$", raw)
        if pair and current:
            entries[current][pair.group(1)] = pair.group(2).strip().strip("'\"")
    return entries


def _overlay_text(tmp_path: Path, body: str) -> str:
    compose = tmp_path / "compose.yaml"
    compose.write_text(body, encoding="utf-8")
    with Registry(tmp_path / "registry.sqlite3") as registry:
        overlay = _compose_overlay(registry, compose)
        try:
            return overlay.read_text(encoding="utf-8")
        finally:
            overlay.unlink()


def test_top_level_volumes_and_networks_are_labeled_with_their_own_kind(tmp_path: Path) -> None:
    """Compose volumes were the class of object the 452 GiB incident was made of."""
    entries = _labelled(
        _overlay_text(
            tmp_path,
            "services:\n  app:\n    image: alpine\n\nvolumes:\n  data:\n\nnetworks:\n  backend:\n",
        )
    )

    assert entries["app"]["kind"] == "container"
    assert entries["data"]["kind"] == "volume"
    assert entries["backend"]["kind"] == "network"


def test_a_services_nested_volumes_are_not_mistaken_for_declarations(tmp_path: Path) -> None:
    """A service's `volumes:` lists what it *uses*; the top-level block *declares*.

    Both sit at a two-space-ish indent, so a file-wide regex reads a service's mount
    reference as a top-level declaration and labels it `kind="container"`.
    """
    entries = _labelled(
        _overlay_text(
            tmp_path,
            "services:\n"
            "  app:\n"
            "    image: alpine\n"
            "    volumes:\n"
            "      - data:/data\n"
            "    networks:\n"
            "      - backend\n"
            "\n"
            "volumes:\n"
            "  data:\n"
            "\n"
            "networks:\n"
            "  backend:\n",
        )
    )

    containers = [name for name, entry in entries.items() if entry.get("kind") == "container"]
    assert containers == ["app"], f"a nested reference was labeled as a service: {containers}"
    assert entries["data"]["kind"] == "volume"
    assert entries["backend"]["kind"] == "network"


def test_composes_implicit_default_network_is_labeled(tmp_path: Path) -> None:
    """Compose creates `default` even when the file declares no networks at all.

    Left unlabeled it is exactly the ungoverned resource #48 exists to prevent.
    """
    entries = _labelled(_overlay_text(tmp_path, "services:\n  app:\n    image: alpine\n"))

    assert entries["default"]["kind"] == "network"


def test_an_anonymous_service_volume_is_refused_before_compose_runs(tmp_path: Path) -> None:
    """It has no top-level key an overlay can attach labels to, so it cannot be governed.

    #48 requires such a resource be reported as ungovernable *before* execution rather
    than silently created.
    """
    with pytest.raises(DockerFrontDoorError, match="anonymous|ungovernable|volume"):
        _overlay_text(
            tmp_path,
            "services:\n  app:\n    image: alpine\n    volumes:\n      - /data\n",
        )


def test_label_values_survive_a_yaml_round_trip_on_windows(tmp_path: Path) -> None:
    r"""The overlay must parse as YAML, and a Windows workspace is the hard case.

    Label values were emitted double-quoted, where YAML processes escapes: a workspace of
    `C:\Users\...` contains `\U`, read as the start of an 8-hex-digit unicode escape and
    rejected outright. Compose could not parse its own generated overlay on the platform
    bosn is developed on, so the front door was inert there.
    """
    workdir = tmp_path / "Users" / "new"
    workdir.mkdir(parents=True)
    text = _overlay_text(workdir, "services:\n  app:\n    image: alpine\n")

    workspace = _labelled(text)["app"]["workspace"]
    assert workspace == str(workdir.resolve()), "the path did not round-trip"
    # Single-quoted YAML performs no escape processing; double-quoted does.
    assert "workspace: '" in text, "values must be single-quoted to survive backslashes"


# -- reconcile after every Compose invocation, not just a clean `up` (#48) ---


class _FakeCompleted:
    def __init__(self, returncode: int) -> None:
        self.returncode = returncode


class _FakeDaemon:
    """Records every verb sent to the daemon and answers with canned replies."""

    def __init__(
        self,
        adopt_reply: dict[str, Any] | None = None,
        acquire_reply: dict[str, Any] | None = None,
    ) -> None:
        self.calls: list[str] = []
        self.requests: list[dict[str, Any]] = []
        self.adopt_reply = adopt_reply if adopt_reply is not None else {"ok": True, "adopted": []}
        self.acquire_reply = (
            acquire_reply if acquire_reply is not None else {"ok": True, "session": "sess-1"}
        )

    def request(self, verb: str, *_args: Any, **kwargs: Any) -> dict[str, Any]:
        self.calls.append(verb)
        self.requests.append(kwargs)
        if verb == "status":
            return {"ok": True, "registry_id": "reg-1"}
        if verb == "compose-adopt":
            return self.adopt_reply
        if verb == "compose-acquire":
            return self.acquire_reply
        if verb == "compose-release":
            return {"ok": True, "released": 0}
        raise AssertionError(f"unexpected verb {verb!r}")


def _compose_file(tmp_path: Path) -> Path:
    compose = tmp_path / "compose.yaml"
    compose.write_text("services:\n  app:\n    image: alpine\n", encoding="utf-8")
    return compose


@pytest.fixture(autouse=True)
def _fixed_process_start_time(monkeypatch: pytest.MonkeyPatch) -> None:
    """Every test below fakes `subprocess.run` for the compose command itself, and that
    monkeypatch replaces the one process-wide `subprocess` module object -- there is no
    separate copy to leave alone. `process_start_time()` also shells out (`tasklist`/`ps`)
    to probe the client's own liveness, so without this fixture it would silently receive
    the compose-command fake instead and blow up on a missing `.stdout` attribute.
    """
    monkeypatch.setattr("bosn.docker_cli.process_start_time", lambda pid: 123.0)


@pytest.fixture(autouse=True)
def _fake_real_engine(monkeypatch: pytest.MonkeyPatch) -> None:
    """`_run_compose` now resolves the real engine through the same guard `_forward` uses
    (the fork-bomb fix below), instead of spawning a bare `"docker"` off PATH. Fake that
    resolution here so the many existing compose tests do not depend on a real `docker`
    being installed in CI -- per the no-Docker-in-tests rule, only `subprocess.run` itself
    is faked by each test; engine *resolution* is faked once, here, for all of them.
    Individual recursion-guard tests below override this fixture's patches themselves.
    """
    monkeypatch.setattr(docker_cli, "_resolve_real_engine", lambda binary="docker": Path("docker"))


def test_a_failed_up_still_reconciles(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """RED before the fix: adoption only ran when `up` exited 0.

    A service image that fails to pull leaves Compose's earlier containers, plus the
    network and volumes it already created, fully labeled and unregistered -- this is
    the exact failure #48 describes.
    """
    fake_daemon = _FakeDaemon()
    monkeypatch.setattr(daemon, "request", fake_daemon.request)
    monkeypatch.setattr(
        "bosn.docker_cli.subprocess.run", lambda *a, **k: _FakeCompleted(returncode=1)
    )

    returncode = _run_compose("up", _compose_file(tmp_path), [])

    assert returncode == 1
    assert "compose-adopt" in fake_daemon.calls


def test_compose_adopt_uses_its_own_timeout_not_the_shared_ipc_default(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """#99: `compose-adopt`'s daemon-side scan cost is a poor fit for `ipc.DEFAULT_TIMEOUT`.

    Batching `_discover`'s inspect fallback (#99) made the scan O(kinds) instead of
    O(host-object-count), but a batched `docker image inspect` call is still measurably slow
    on an object-heavy host -- comfortably past 10s in repeated on-host measurement -- so
    `_reconcile_after_compose` gives this one verb its own named budget via `request_timeout`
    rather than the global default every other verb still uses. Regression guard: a future
    edit that reverted to the bare `daemon.request("compose-adopt", ...)` call would still
    pass every other compose test in this file, since none of them assert on the timeout.
    """
    fake_daemon = _FakeDaemon()
    monkeypatch.setattr(daemon, "request", fake_daemon.request)
    monkeypatch.setattr(
        "bosn.docker_cli.subprocess.run", lambda *a, **k: _FakeCompleted(returncode=0)
    )

    _run_compose("up", _compose_file(tmp_path), [])

    adopt_requests = [
        req
        for verb, req in zip(fake_daemon.calls, fake_daemon.requests, strict=True)
        if verb == "compose-adopt"
    ]
    assert adopt_requests, "compose-adopt was never called"
    for req in adopt_requests:
        assert req.get("request_timeout") == docker_cli.COMPOSE_ADOPT_TIMEOUT_SECONDS
        assert req["request_timeout"] > ipc.DEFAULT_TIMEOUT


def test_an_interrupted_up_still_reconciles_and_the_interrupt_still_propagates(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Ctrl-C during a foreground `up` is the common way a run never reaches a clean exit."""
    fake_daemon = _FakeDaemon()
    monkeypatch.setattr(daemon, "request", fake_daemon.request)

    def _interrupted(*_a: Any, **_k: Any) -> _FakeCompleted:
        raise KeyboardInterrupt

    monkeypatch.setattr("bosn.docker_cli.subprocess.run", _interrupted)

    with pytest.raises(KeyboardInterrupt):
        _run_compose("up", _compose_file(tmp_path), [])

    assert "compose-adopt" in fake_daemon.calls


@pytest.mark.parametrize("command", ["down", "logs", "ps"])
def test_non_up_commands_also_reconcile(
    command: str, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Reconcile is unconditional: it is cheap and idempotent, so paying for it on
    `down`/`logs`/`ps` too is harmless -- those commands don't label new resources, so
    adoption simply finds nothing to do.
    """
    fake_daemon = _FakeDaemon()
    monkeypatch.setattr(daemon, "request", fake_daemon.request)
    monkeypatch.setattr(
        "bosn.docker_cli.subprocess.run", lambda *a, **k: _FakeCompleted(returncode=0)
    )

    returncode = _run_compose(command, _compose_file(tmp_path), [])

    assert returncode == 0
    assert "compose-adopt" in fake_daemon.calls


def test_a_reconcile_failure_is_reported_but_does_not_mask_composes_exit_code(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    """The user asked whether `up` succeeded, not whether bookkeeping succeeded."""
    fake_daemon = _FakeDaemon(adopt_reply={"ok": False, "error": "engine unreachable"})
    monkeypatch.setattr(daemon, "request", fake_daemon.request)
    monkeypatch.setattr(
        "bosn.docker_cli.subprocess.run", lambda *a, **k: _FakeCompleted(returncode=0)
    )

    returncode = _run_compose("up", _compose_file(tmp_path), [])

    assert returncode == 0, "a reconcile failure must not override compose's own exit code"
    err = capsys.readouterr().err
    assert "engine unreachable" in err
    assert "may be unregistered" in err


# -- Compose project leases (#48) --------------------------------------------


def test_compose_acquire_is_sent_before_compose_runs_and_released_after(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    fake_daemon = _FakeDaemon()
    monkeypatch.setattr(daemon, "request", fake_daemon.request)
    calls_at_compose_run: list[str] = []

    def _record_and_run(*_a: Any, **_k: Any) -> _FakeCompleted:
        calls_at_compose_run.extend(fake_daemon.calls)
        return _FakeCompleted(returncode=0)

    monkeypatch.setattr("bosn.docker_cli.subprocess.run", _record_and_run)

    returncode = _run_compose("up", _compose_file(tmp_path), [])

    assert returncode == 0
    assert calls_at_compose_run == ["status", "compose-adopt", "compose-acquire"]
    assert fake_daemon.calls[-2:] == ["compose-release", "compose-adopt"]


def test_the_acquire_request_carries_the_clients_own_pid_and_proc_start_not_the_daemons(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """`bosn-docker` must send its own identity: the daemon has no long-lived holder for a
    Compose lease the way it does for `execution-acquire`, so a daemon-side pid would pin
    the lease forever if this client were killed before it could release explicitly.
    """
    fake_daemon = _FakeDaemon()
    monkeypatch.setattr(daemon, "request", fake_daemon.request)
    monkeypatch.setattr(
        "bosn.docker_cli.subprocess.run", lambda *a, **k: _FakeCompleted(returncode=0)
    )

    _run_compose("up", _compose_file(tmp_path), [])

    acquire_request = next(
        req
        for call, req in zip(fake_daemon.calls, fake_daemon.requests, strict=True)
        if call == "compose-acquire"
    )
    assert acquire_request["pid"] == os.getpid()
    assert acquire_request["proc_start"] == 123.0  # from the _fixed_process_start_time fixture


def test_release_happens_even_when_compose_exits_non_zero(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    fake_daemon = _FakeDaemon()
    monkeypatch.setattr(daemon, "request", fake_daemon.request)
    monkeypatch.setattr(
        "bosn.docker_cli.subprocess.run", lambda *a, **k: _FakeCompleted(returncode=1)
    )

    returncode = _run_compose("up", _compose_file(tmp_path), [])

    assert returncode == 1
    assert "compose-release" in fake_daemon.calls


def test_release_happens_on_keyboard_interrupt_and_the_interrupt_still_propagates(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    fake_daemon = _FakeDaemon()
    monkeypatch.setattr(daemon, "request", fake_daemon.request)

    def _interrupted(*_a: Any, **_k: Any) -> _FakeCompleted:
        raise KeyboardInterrupt

    monkeypatch.setattr("bosn.docker_cli.subprocess.run", _interrupted)

    with pytest.raises(KeyboardInterrupt):
        _run_compose("up", _compose_file(tmp_path), [])

    assert "compose-release" in fake_daemon.calls


def test_a_failed_acquire_still_lets_compose_run_and_releases_cleanly(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    """Leasing is a protection, not a precondition: a project with nothing registered yet
    (or an unreachable daemon) must not block the compose command itself, and the release
    that follows a session-less acquire must not raise or be reported as a run failure.
    """
    fake_daemon = _FakeDaemon(acquire_reply={"ok": False, "error": "no such workspace"})
    monkeypatch.setattr(daemon, "request", fake_daemon.request)
    monkeypatch.setattr(
        "bosn.docker_cli.subprocess.run", lambda *a, **k: _FakeCompleted(returncode=0)
    )

    returncode = _run_compose("up", _compose_file(tmp_path), [])

    assert returncode == 0
    assert "compose-acquire" in fake_daemon.calls
    assert "compose-release" not in fake_daemon.calls, "nothing to release without a session"
    err = capsys.readouterr().err
    assert "no such workspace" in err
    assert "unprotected against pressure eviction" in err


def test_a_clean_up_reconciles_exactly_as_before(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    fake_daemon = _FakeDaemon()
    monkeypatch.setattr(daemon, "request", fake_daemon.request)
    monkeypatch.setattr(
        "bosn.docker_cli.subprocess.run", lambda *a, **k: _FakeCompleted(returncode=0)
    )

    returncode = _run_compose("up", _compose_file(tmp_path), [])

    assert returncode == 0
    # An initial reconcile registers whatever the project already has, then the lease is
    # acquired and released around the compose command itself, then a closing reconcile
    # adopts anything newly labeled during this run.
    assert fake_daemon.calls == [
        "status",
        "compose-adopt",
        "compose-acquire",
        "compose-release",
        "compose-adopt",
    ]


# -- the decided v1 compose flag subset (#47) --------------------------------


@pytest.mark.parametrize("flag", ["-d", "--detach", "--wait"])
def test_up_flags_reach_the_engine_intact(
    flag: str, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    fake_daemon = _FakeDaemon()
    monkeypatch.setattr(daemon, "request", fake_daemon.request)
    calls: list[list[str]] = []
    monkeypatch.setattr(
        "bosn.docker_cli.subprocess.run",
        lambda argv, **_k: (calls.append(argv), _FakeCompleted(returncode=0))[1],
    )

    returncode = _run_compose("up", _compose_file(tmp_path), [flag])

    assert returncode == 0
    assert calls[0][-1] == flag
    assert calls[0][-2] == "up"


@pytest.mark.parametrize("flag", ["-v", "--volumes", "--remove-orphans"])
def test_down_flags_reach_the_engine_intact(
    flag: str, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    fake_daemon = _FakeDaemon()
    monkeypatch.setattr(daemon, "request", fake_daemon.request)
    calls: list[list[str]] = []
    monkeypatch.setattr(
        "bosn.docker_cli.subprocess.run",
        lambda argv, **_k: (calls.append(argv), _FakeCompleted(returncode=0))[1],
    )

    returncode = _run_compose("down", _compose_file(tmp_path), [flag])

    assert returncode == 0
    assert calls[0][-1] == flag
    assert calls[0][-2] == "down"


def test_an_unsupported_flag_still_refuses_naming_it(tmp_path: Path) -> None:
    with pytest.raises(DockerFrontDoorError, match="--scale"):
        _run_compose("up", tmp_path / "compose.yaml", ["--scale"])


def test_a_flag_valid_for_the_other_verb_still_refuses(tmp_path: Path) -> None:
    """`-v`/`--volumes` only means something on `down`; `up -v` is not in the decided
    subset and must refuse exactly like any other unrecognized argument."""
    with pytest.raises(DockerFrontDoorError, match="-v"):
        _run_compose("up", tmp_path / "compose.yaml", ["-v"])


def test_logs_and_ps_still_accept_no_arguments(tmp_path: Path) -> None:
    """`logs`/`ps` never grew an accepted subset -- only `up`/`down` did."""
    with pytest.raises(DockerFrontDoorError, match="-f"):
        _run_compose("logs", tmp_path / "compose.yaml", ["-f"])


# -- the four verbs #47 declared but never wired: build/run/exec/config ------


def test_parser_accepts_exactly_what_compose_commands_declares(tmp_path: Path) -> None:
    """The parser used to hardcode `choices=["up", "down", "logs", "ps"]` -- four of the
    eight sub-verbs `frontdoor.COMPOSE_COMMANDS` (and therefore the generated docs and
    `--supported --json`) already declared, so `build`/`run`/`exec`/`config` were
    advertised as supported and rejected by argparse before ever reaching dispatch.

    Asserted directly against the table, not a literal copy of its current contents, so a
    future ninth sub-verb added to `COMPOSE_FLAGS` without the parser being touched fails
    this test instead of silently going unimplemented again. The negative half is exercised
    against `_run_compose`, not the parser: an invalid sub-verb must not raise `SystemExit`
    (argparse's own bare exit-2 usage dump) -- it must refuse through the same
    `DockerFrontDoorError` path every other compose refusal in this module uses. See
    `test_an_undeclared_compose_subcommand_refuses_with_the_structured_envelope` below.
    """
    parser = docker_cli._parse_compose_args
    for command in COMPOSE_COMMANDS:
        ns = parser([command])
        assert ns.command == command


def test_an_undeclared_compose_subcommand_refuses_with_the_structured_envelope(
    tmp_path: Path,
) -> None:
    """An unrecognized compose sub-verb (a typo, or one this table has never heard of) must
    refuse the same way every other unsupported compose flag/argument does: a
    `DockerFrontDoorError` `main()` turns into prose-on-stderr and exit code 1, not
    argparse's bare `SystemExit(2)` usage dump the parser alone would give.
    """
    with pytest.raises(DockerFrontDoorError, match="frobnicate"):
        _run_compose("frobnicate", _compose_file(tmp_path), [])


def test_main_refuses_an_undeclared_compose_subcommand(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    """Same refusal, exercised end to end through `main()` -- the compose branch has no
    `--json` envelope of its own (see `_run_compose`'s dispatch comment), so this is prose
    on stderr and exit code 1, exactly like `test_compose_subset_refuses_unknown_argument`.
    """

    def _fail_if_called(*_a: Any, **_k: Any) -> _FakeCompleted:
        raise AssertionError("an unrecognized sub-verb must never reach the engine")

    monkeypatch.setattr(docker_cli.subprocess, "run", _fail_if_called)

    returncode = main(["compose", "frobnicate"])

    assert returncode == 1
    assert "frobnicate" in capsys.readouterr().err


def test_a_global_file_flag_typed_after_the_subverb_still_refuses_naming_it(
    tmp_path: Path,
) -> None:
    """`-f`/`--file` is parsed ahead of the sub-verb (`compose -f FILE up`, not
    `compose up -f FILE`). It is `resolve_compose_flag`'s ACCEPTED global row for every
    sub-verb except `logs` (which overrides it), so a naive ACCEPTED/REFUSED check on that
    lookup alone would let it silently pass validation here and reach the engine positioned
    after the sub-verb, where Compose does not read it as bosn intends. Must still refuse,
    naming the flag, exactly like any other out-of-place argument.
    """
    with pytest.raises(DockerFrontDoorError, match="-f"):
        _run_compose("up", _compose_file(tmp_path), ["-f", "other-compose.yaml"])

    with pytest.raises(DockerFrontDoorError, match="--file"):
        _run_compose("down", _compose_file(tmp_path), ["--file", "other-compose.yaml"])


@pytest.mark.parametrize(
    ("command", "extra"),
    [
        ("build", []),
        ("config", []),
        # `run`/`exec` take a mandatory SERVICE (`COMPOSE_SERVICE_COMMANDS`); passing none
        # is its own refusal, covered below, so the reach-the-engine case has to supply one.
        ("exec", ["app", "sh"]),
        ("run", ["app"]),
    ],
)
def test_the_four_newly_governed_verbs_reach_the_engine_with_the_verb_intact(
    command: str, extra: list[str], tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """`build`/`config`/`exec`/`run` used to refuse before reaching `_run_compose` at all
    (blocked by the hardcoded `choices=` above); now they route through the same governed
    path `up`/`down`/`logs`/`ps` already use.
    """
    fake_daemon = _FakeDaemon()
    monkeypatch.setattr(daemon, "request", fake_daemon.request)
    calls: list[list[str]] = []
    monkeypatch.setattr(
        "bosn.docker_cli.subprocess.run",
        lambda argv, **_k: (calls.append(argv), _FakeCompleted(returncode=0))[1],
    )

    returncode = _run_compose(command, _compose_file(tmp_path), list(extra))

    assert returncode == 0
    # The verb sits immediately before its own arguments, and those arrive in order --
    # asserting on the tail rather than `[-1]` so the SERVICE/COMMAND tokens are pinned too.
    assert calls[0][-1 - len(extra) :] == [command, *extra]


def test_a_built_services_image_carries_the_bosn_label_contract_in_the_overlay(
    tmp_path: Path,
) -> None:
    """A service's plain `labels:` land on the *container* Compose creates, never on an
    image `build:` produces -- `compose build` would otherwise hand back a fully
    unlabeled, unregistered image, exactly the ungoverned resource #48 exists to prevent.
    The overlay must also carry a `build.labels` block, `kind="image"`, for any service
    with a `build:` key.
    """
    (tmp_path / "ctx").mkdir()
    (tmp_path / "ctx" / "Dockerfile").write_text("FROM alpine\n", encoding="utf-8")
    text = _overlay_text(
        tmp_path,
        "services:\n  app:\n    build: ./ctx\n    labels:\n      existing: yes\n",
    )
    # The container-side contract is untouched.
    assert _labelled(text)["app"]["kind"] == "container"

    build_labels_index = text.index("build:")
    assert build_labels_index != -1
    build_block = text[build_labels_index:]
    kind_line = next(line for line in build_block.splitlines() if "com.zackees.bosn.kind" in line)
    assert kind_line.split(":", 1)[1].strip().strip("'\"") == "image"


def test_a_build_only_service_also_gets_an_image_label_block(tmp_path: Path) -> None:
    """`is_build_only` services (`build:` with no `image:`) are exactly the class of
    service `compose build`/an implicit build under `up` targets -- they must be governed
    too, not just services that also declare an `image:`.
    """
    (tmp_path / "ctx").mkdir()
    (tmp_path / "ctx" / "Dockerfile").write_text("FROM alpine\n", encoding="utf-8")
    text = _overlay_text(
        tmp_path,
        "services:\n  app:\n    build: ./ctx\n",
    )
    assert "build:" in text
    services_section = text.split("networks:", 1)[0]
    assert services_section.count("com.zackees.bosn.kind") == 2  # container + image


def test_a_service_with_no_build_key_gets_no_build_labels_block(tmp_path: Path) -> None:
    text = _overlay_text(tmp_path, "services:\n  app:\n    image: alpine\n")
    assert "build:" not in text


def test_run_acquires_and_releases_the_project_lease_like_up(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """`run` creates a container, so it must be leased and reconciled exactly like `up` --
    verified against a real `docker compose run` merge, which does apply a service's
    `labels:` to the container it starts (same mechanism `up` relies on), so no separate
    label plumbing is needed for `run` -- only the same lease/reconcile treatment.
    """
    fake_daemon = _FakeDaemon()
    monkeypatch.setattr(daemon, "request", fake_daemon.request)
    monkeypatch.setattr(
        "bosn.docker_cli.subprocess.run", lambda *a, **k: _FakeCompleted(returncode=0)
    )

    returncode = _run_compose("run", _compose_file(tmp_path), ["app"])

    assert returncode == 0
    assert fake_daemon.calls == [
        "status",
        "compose-adopt",
        "compose-acquire",
        "compose-release",
        "compose-adopt",
    ]


def test_a_container_command_survives_flag_validation_intact(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Everything after the SERVICE token belongs to the *container*, not to `compose exec`.

    `exec app ls -la` must reach the engine with `-la` attached to `ls`. Running trailing
    tokens through `resolve_compose_flag` would refuse `-la` as an unsupported compose flag
    and make every non-trivial command line unusable -- so this pins the boundary against a
    future "validate all the args" simplification.
    """
    fake_daemon = _FakeDaemon()
    monkeypatch.setattr(daemon, "request", fake_daemon.request)
    calls: list[list[str]] = []
    monkeypatch.setattr(
        "bosn.docker_cli.subprocess.run",
        lambda argv, **_k: (calls.append(argv), _FakeCompleted(returncode=0))[1],
    )

    assert _run_compose("exec", _compose_file(tmp_path), ["app", "ls", "-la"]) == 0
    assert calls[0][-4:] == ["exec", "app", "ls", "-la"]


@pytest.mark.parametrize("command", ["run", "exec"])
def test_a_service_scoped_verb_refuses_when_no_service_is_named(
    command: str, tmp_path: Path
) -> None:
    """Docker itself errors on `compose run` with no service, so bosn refuses in its own
    envelope rather than spending an engine spawn and an overlay render to learn nothing.
    """
    with pytest.raises(DockerFrontDoorError, match="requires a service name"):
        _run_compose(command, _compose_file(tmp_path), [])


@pytest.mark.parametrize("command", ["run", "exec"])
def test_a_service_scoped_verb_still_refuses_flags_before_the_service(
    command: str, tmp_path: Path
) -> None:
    """The SERVICE split must not become a hole: tokens *before* the service are still
    flags, and `run`/`exec` accept none, so they refuse exactly as they did before.
    """
    with pytest.raises(DockerFrontDoorError, match="-d"):
        _run_compose(command, _compose_file(tmp_path), ["-d", "app"])

    with pytest.raises(DockerFrontDoorError, match="must come before the sub-verb"):
        _run_compose(command, _compose_file(tmp_path), ["-f", "other.yaml", "app"])


# -- detached `up -d`/`up --wait` still releases its lease immediately (#47) --


@pytest.mark.parametrize("flag", ["-d", "--wait"])
def test_detached_up_still_releases_the_lease_immediately(
    flag: str, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The decided posture: a detached `up` releases its lease exactly like a blocking one,
    because the lease is held under *this* process's identity and this process is about to
    exit -- keeping it "alive" here would only delay reclaim by one TTL, not protect the
    project durably, and re-homing it onto the daemon would be the permanent pin leases
    exist to prevent (see `_verb_compose_acquire`'s docstring). A detached project's
    containers are left protected only by their age tier after this call returns; that gap
    is documented in `_run_compose` and tracked as a follow-up, not silently absorbed here.
    """
    fake_daemon = _FakeDaemon()
    monkeypatch.setattr(daemon, "request", fake_daemon.request)
    monkeypatch.setattr(
        "bosn.docker_cli.subprocess.run", lambda *a, **k: _FakeCompleted(returncode=0)
    )

    returncode = _run_compose("up", _compose_file(tmp_path), [flag])

    assert returncode == 0
    assert fake_daemon.calls == [
        "status",
        "compose-adopt",
        "compose-acquire",
        "compose-release",
        "compose-adopt",
    ]


# -- `down -v` leaves the registry consistent (#47) ---------------------------


def test_down_v_sends_prune_missing_on_the_closing_reconcile_only(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The pre-command reconcile runs before Compose has touched anything this invocation
    and must stay additive; only the closing reconcile, after Compose has actually deleted
    the volumes, may prune rows for what is now gone.
    """
    fake_daemon = _FakeDaemon()
    monkeypatch.setattr(daemon, "request", fake_daemon.request)
    monkeypatch.setattr(
        "bosn.docker_cli.subprocess.run", lambda *a, **k: _FakeCompleted(returncode=0)
    )

    returncode = _run_compose("down", _compose_file(tmp_path), ["-v"])

    assert returncode == 0
    adopt_requests = [
        req
        for call, req in zip(fake_daemon.calls, fake_daemon.requests, strict=True)
        if call == "compose-adopt"
    ]
    assert len(adopt_requests) == 2
    assert adopt_requests[0]["prune_missing"] is False
    assert adopt_requests[1]["prune_missing"] is True


def test_plain_down_never_sends_prune_missing(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    fake_daemon = _FakeDaemon()
    monkeypatch.setattr(daemon, "request", fake_daemon.request)
    monkeypatch.setattr(
        "bosn.docker_cli.subprocess.run", lambda *a, **k: _FakeCompleted(returncode=0)
    )

    _run_compose("down", _compose_file(tmp_path), [])

    adopt_requests = [
        req
        for call, req in zip(fake_daemon.calls, fake_daemon.requests, strict=True)
        if call == "compose-adopt"
    ]
    assert all(req["prune_missing"] is False for req in adopt_requests)


def test_down_v_removes_the_registry_row_for_a_volume_compose_actually_deleted(
    tmp_path: Path,
) -> None:
    """The correctness test for Problem 2: after `down -v`, a volume the registry still has
    a row for but that no longer exists on the engine must be gone from the registry too.

    Exercises the real daemon handler (`Daemon._verb_compose_adopt`), not the `_FakeDaemon`
    test double the other tests in this file use -- this is exactly the crash-boundary
    staleness `reconcile_owned` (#41) was written to repair, applied here to a resource
    Compose itself removed rather than one that vanished from an engine crash.
    """
    from bosn import daemon as daemon_module
    from bosn.resources import ScanResult

    instance = daemon_module.Daemon(state_dir=tmp_path / "state")
    try:
        instance.registry.register_resource(
            kind="volume",
            name="proj_data",
            stack="proj",
            generation="gen-1",
            scope="stack",
            workspace=str(tmp_path),
        )
        assert any(r.name == "proj_data" for r in instance.registry.list_resources())

        class _EmptyVolumeScan:
            def scan(self, _registry_id: str) -> ScanResult:
                # The volume is gone from the engine -- Compose just deleted it -- but the
                # scan still covers the "volume" kind, so its absence here is what
                # `reconcile_owned` reads as "remove the row", not "engine unreachable".
                return ScanResult(owned=[], scanned_kinds={"volume", "container", "network"})

        def _fake_scanner(*_a: object, **_k: object) -> _EmptyVolumeScan:
            return _EmptyVolumeScan()

        import bosn.resources as resources_module

        original_scanner = resources_module.ResourceScanner
        resources_module.ResourceScanner = _fake_scanner  # type: ignore[assignment]
        try:
            reply = instance._verb_compose_adopt({"prune_missing": True})
        finally:
            resources_module.ResourceScanner = original_scanner  # type: ignore[assignment]

        assert reply["ok"] is True
        assert not any(r.name == "proj_data" for r in instance.registry.list_resources())
    finally:
        instance.registry.close()


def test_a_plain_compose_adopt_never_prunes_rows_for_a_resource_the_scan_missed(
    tmp_path: Path,
) -> None:
    """The default (`prune_missing` absent/False) must stay purely additive: a failed or
    partial engine listing is never permission to forget a row for a resource that never
    actually left the engine.
    """
    from bosn import daemon as daemon_module
    from bosn.resources import ScanResult

    instance = daemon_module.Daemon(state_dir=tmp_path / "state")
    try:
        instance.registry.register_resource(
            kind="volume",
            name="proj_data",
            stack="proj",
            generation="gen-1",
            scope="stack",
            workspace=str(tmp_path),
        )

        class _EmptyVolumeScan:
            def scan(self, _registry_id: str) -> ScanResult:
                return ScanResult(owned=[], scanned_kinds={"volume"})

        def _fake_scanner(*_a: object, **_k: object) -> _EmptyVolumeScan:
            return _EmptyVolumeScan()

        import bosn.resources as resources_module

        original_scanner = resources_module.ResourceScanner
        resources_module.ResourceScanner = _fake_scanner  # type: ignore[assignment]
        try:
            reply = instance._verb_compose_adopt({})
        finally:
            resources_module.ResourceScanner = original_scanner  # type: ignore[assignment]

        assert reply["ok"] is True
        assert any(r.name == "proj_data" for r in instance.registry.list_resources())
    finally:
        instance.registry.close()


# -- category-table dispatch (#46) --------------------------------------------
#
# `bosn.frontdoor` supplies the category table itself (a separate agent's slice of #46,
# landed alongside this one). `bosn.docker_cli` dispatches through its `resolve()` -- the
# fail-closed wrapper that always returns a `VerbSpec`, so an unknown verb is just another
# REFUSE row rather than a `None` a caller could mistake for "safe to forward". These tests
# do not depend on the real table's contents -- every test below monkeypatches
# `docker_cli.resolve` (and, where relevant, `docker_cli.supported`) with a small fake of
# its own, per the brief, so they do not break when the real table gains entries.


def _fake_spec(verb: str, category: Category, summary: str, remedy: str | None) -> VerbSpec:
    return VerbSpec(verb=verb, category=category, summary=summary, remedy=remedy)


def test_a_governed_verb_still_runs_its_implementation_through_dispatch(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """`compose` is GOVERNED and implemented directly in `main()`, not through the table
    dispatch -- this exercises the new `main()` -> `_parse_compose_args` -> `_run_compose`
    path end to end, proving the table rewiring didn't disturb it.
    """
    fake_daemon = _FakeDaemon()
    monkeypatch.setattr(daemon, "request", fake_daemon.request)
    monkeypatch.setattr("bosn.docker_cli.process_start_time", lambda pid: 123.0)
    monkeypatch.setattr(
        "bosn.docker_cli.subprocess.run", lambda *a, **k: _FakeCompleted(returncode=0)
    )
    compose = _compose_file(tmp_path)

    returncode = main(["compose", "-f", str(compose), "up"])

    assert returncode == 0
    assert "compose-adopt" in fake_daemon.calls


def test_a_forward_verb_invokes_the_real_engine_with_argv_passed_verbatim(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    real_docker = tmp_path / "real-docker"
    real_docker.write_text("", encoding="utf-8")
    monkeypatch.setattr(
        docker_cli, "resolve", lambda verb: _fake_spec("version", Category.FORWARD, "v", None)
    )
    monkeypatch.setattr(docker_cli, "_resolve_real_engine", lambda binary="docker": real_docker)
    calls: list[list[str]] = []
    envs: list[dict[str, str]] = []

    def _fake_run(argv: list[str], **kwargs: Any) -> _FakeCompleted:
        calls.append(argv)
        envs.append(kwargs["env"])
        return _FakeCompleted(returncode=0)

    monkeypatch.setattr(docker_cli.subprocess, "run", _fake_run)

    returncode = main(["version", "--format", "{{.Server.Version}}"])

    assert returncode == 0
    assert calls == [[str(real_docker), "version", "--format", "{{.Server.Version}}"]]
    # The recursion guard's first layer: every forwarded child must carry the marker, or a
    # future `docker` shim resolving back to this program would never see it and loop.
    assert envs[0][docker_cli._RECURSION_GUARD_ENV] == str(os.getpid())


def test_a_refuse_verb_emits_the_envelope_with_the_specs_remedy_and_exits_non_zero(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    monkeypatch.setattr(
        docker_cli,
        "resolve",
        lambda verb: _fake_spec(
            "rm", Category.REFUSE, "forcibly removes containers bosn tracks", "use `bosn done`"
        ),
    )

    returncode = main(["rm", "--json", "some-container"])

    assert returncode != 0
    envelope = json.loads(capsys.readouterr().out)
    assert envelope["ok"] is False
    assert envelope["next"] == "use `bosn done`"
    assert "rm" in envelope["message"]


def test_an_unknown_verb_refuses_and_never_forwards(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    """The safety property: a verb absent from the table must never reach the engine.

    Mirrors what the real `frontdoor.resolve()` does for a verb it has never heard of:
    return an explicit REFUSE row rather than `None` -- this is the fail-closed contract
    dispatch relies on, exercised here with a fake so the test does not depend on the
    real table's contents.
    """
    monkeypatch.setattr(
        docker_cli,
        "resolve",
        lambda verb: _fake_spec(
            verb, Category.REFUSE, "unrecognized docker verb", "see --supported"
        ),
    )

    def _fail_if_called(*_a: Any, **_k: Any) -> _FakeCompleted:
        raise AssertionError("an unknown verb must never invoke the real engine")

    monkeypatch.setattr(docker_cli.subprocess, "run", _fail_if_called)

    returncode = main(["totally-made-up-verb"])

    assert returncode != 0
    err = capsys.readouterr().err
    assert "totally-made-up-verb" in err
    assert "unrecognized" in err.lower()


def test_an_unknown_verb_refuses_and_never_forwards_against_the_real_table(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    """Same safety property, exercised against the real `bosn.frontdoor` table this time
    (no monkeypatch of `resolve`) -- proves the wiring, not just a fake standing in for it.
    """

    def _fail_if_called(*_a: Any, **_k: Any) -> _FakeCompleted:
        raise AssertionError("an unknown verb must never invoke the real engine")

    monkeypatch.setattr(docker_cli.subprocess, "run", _fail_if_called)

    returncode = main(["totally-made-up-verb"])

    assert returncode != 0
    assert "totally-made-up-verb" in capsys.readouterr().err


def test_supported_json_emits_parseable_json(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    fake_table = {"verbs": [{"verb": "init", "category": "governed"}]}
    monkeypatch.setattr(docker_cli, "supported", lambda: fake_table)

    returncode = main(["--supported", "--json"])

    assert returncode == 0
    assert json.loads(capsys.readouterr().out) == fake_table


# -- recursion guard: absolute real-engine resolution (#46) -------------------


def test_recursion_guard_refuses_when_the_environment_marker_is_already_set(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    """A child of a bosn-docker forward that is itself asked to forward -- because the
    resolved `docker` turned out to be a shim calling back into bosn-docker -- must refuse
    immediately, before even resolving PATH again.
    """
    monkeypatch.setattr(
        docker_cli, "resolve", lambda verb: _fake_spec("version", Category.FORWARD, "v", None)
    )
    monkeypatch.setenv(docker_cli._RECURSION_GUARD_ENV, "1234")

    def _fail_if_called(*_a: Any, **_k: Any) -> Path | None:
        raise AssertionError("must not resolve PATH again once the recursion marker is set")

    monkeypatch.setattr(docker_cli, "_resolve_real_engine", _fail_if_called)

    def _fail_if_run(*_a: Any, **_k: Any) -> _FakeCompleted:
        raise AssertionError("must not execute the resolved engine")

    monkeypatch.setattr(docker_cli.subprocess, "run", _fail_if_run)

    returncode = main(["version", "--json"])

    assert returncode != 0
    envelope = json.loads(capsys.readouterr().out)
    assert envelope["ok"] is False
    assert envelope["code"] == "docker.recursion"


def test_recursion_guard_refuses_when_the_resolved_docker_is_bosns_own_shim(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    """The first-hop case: PATH's `docker` already is bosn-docker (a future shim), before
    any forwarding -- and therefore any inherited environment marker -- has happened.
    """
    monkeypatch.setattr(
        docker_cli, "resolve", lambda verb: _fake_spec("version", Category.FORWARD, "v", None)
    )
    monkeypatch.setattr(docker_cli, "_resolve_real_engine", lambda binary="docker": Path("shim"))
    monkeypatch.setattr(docker_cli, "_is_this_program", lambda candidate: True)

    def _fail_if_run(*_a: Any, **_k: Any) -> _FakeCompleted:
        raise AssertionError("must not execute a `docker` that resolves to bosn-docker itself")

    monkeypatch.setattr(docker_cli.subprocess, "run", _fail_if_run)

    returncode = main(["version", "--json"])

    assert returncode != 0
    envelope = json.loads(capsys.readouterr().out)
    assert envelope["ok"] is False
    assert envelope["code"] == "docker.recursion"
    assert "bosn-docker" in envelope["message"]


def test_run_compose_refuses_when_the_resolved_docker_is_bosns_own_shim(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The fork bomb this guard exists to close: once a `docker` shim that *is*
    bosn-docker exists on PATH, an unguarded `_run_compose` spawning bare
    `["docker", "compose", ...]` would resolve to that shim, which re-enters
    `bosn-docker compose`, which spawns bare `docker compose` again -- forever. Without
    the fix, `_fail_if_called` below would fire, proving `subprocess.run` was reached;
    with it, `_run_compose` must refuse before ever spawning anything.
    """
    monkeypatch.setattr(
        docker_cli, "_resolve_real_engine", lambda binary="docker": Path(tmp_path / "docker-shim")
    )
    monkeypatch.setattr(docker_cli, "_is_this_program", lambda candidate: True)

    def _fail_if_called(*_a: Any, **_k: Any) -> _FakeCompleted:
        raise AssertionError(
            "compose must refuse before spawning the engine, or this is a fork bomb"
        )

    monkeypatch.setattr(docker_cli.subprocess, "run", _fail_if_called)

    with pytest.raises(DockerFrontDoorError, match="bosn-docker itself"):
        _run_compose("up", _compose_file(tmp_path), [])


def test_run_compose_refuses_when_the_recursion_marker_is_already_set(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The second layer of the same guard: a child spawned mid-forward that is somehow
    re-invoked as `bosn-docker compose` must refuse immediately from the inherited
    environment marker, without resolving PATH again.
    """
    monkeypatch.setenv(docker_cli._RECURSION_GUARD_ENV, "1234")

    def _fail_if_resolved(binary: str = "docker") -> Path | None:
        raise AssertionError("must not resolve PATH again once the recursion marker is set")

    monkeypatch.setattr(docker_cli, "_resolve_real_engine", _fail_if_resolved)

    def _fail_if_called(*_a: Any, **_k: Any) -> _FakeCompleted:
        raise AssertionError(
            "compose must refuse before spawning the engine, or this is a fork bomb"
        )

    monkeypatch.setattr(docker_cli.subprocess, "run", _fail_if_called)

    with pytest.raises(DockerFrontDoorError, match="already inside a bosn-docker forward"):
        _run_compose("up", _compose_file(tmp_path), [])


def test_run_compose_passes_the_resolved_engine_and_recursion_marker_to_the_child(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The happy path of the same fix: a real (non-shim) engine still gets invoked, now via
    its resolved absolute path and carrying the recursion marker for its own children.
    """
    fake_daemon = _FakeDaemon()
    monkeypatch.setattr(daemon, "request", fake_daemon.request)
    real_docker = tmp_path / "real-docker"
    monkeypatch.setattr(docker_cli, "_resolve_real_engine", lambda binary="docker": real_docker)
    calls: list[list[str]] = []
    envs: list[dict[str, str]] = []

    def _fake_run(argv: list[str], **kwargs: Any) -> _FakeCompleted:
        calls.append(argv)
        envs.append(kwargs["env"])
        return _FakeCompleted(returncode=0)

    monkeypatch.setattr(docker_cli.subprocess, "run", _fake_run)

    returncode = _run_compose("up", _compose_file(tmp_path), [])

    assert returncode == 0
    assert calls[0][0] == str(real_docker)
    assert calls[0][1] == "compose"
    assert envs[0][docker_cli._RECURSION_GUARD_ENV] == str(os.getpid())


def test_compose_main_entry_point_dispatches_to_bosn_docker_compose(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """`bosn-compose up` (#46's declared-but-missing entry point) must reuse the exact
    same `_run_compose` path `bosn-docker compose up` takes, not a second implementation.
    """
    fake_daemon = _FakeDaemon()
    monkeypatch.setattr(daemon, "request", fake_daemon.request)
    monkeypatch.setattr(
        "bosn.docker_cli.subprocess.run", lambda *a, **k: _FakeCompleted(returncode=0)
    )
    compose = _compose_file(tmp_path)

    returncode = docker_cli.compose_main(["-f", str(compose), "up"])

    assert returncode == 0
    assert "compose-adopt" in fake_daemon.calls


def test_json_refusals_emit_the_envelope_non_json_emit_readable_prose(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    monkeypatch.setattr(
        docker_cli,
        "resolve",
        lambda verb: _fake_spec("kill", Category.REFUSE, "kills containers bosn tracks", "x"),
    )

    prose_returncode = main(["kill"])
    prose_err = capsys.readouterr().err
    assert prose_returncode != 0
    assert "kill" in prose_err
    with pytest.raises(json.JSONDecodeError):
        json.loads(prose_err)

    json_returncode = main(["kill", "--json"])
    envelope = json.loads(capsys.readouterr().out)
    assert json_returncode != 0
    assert envelope["ok"] is False
