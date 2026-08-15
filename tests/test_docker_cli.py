import json
import os
import re
from pathlib import Path
from typing import Any

import pytest

from bosn import daemon, docker_cli
from bosn.docker_cli import (
    DockerFrontDoorError,
    _compose_overlay,
    _run_compose,
    compose_to_manifest,
    main,
)
from bosn.frontdoor import Category, VerbSpec
from bosn.registry import Registry


def test_init_translates_compose_images(tmp_path: Path) -> None:
    compose = tmp_path / "compose.yaml"
    compose.write_text("services:\n  app:\n    image: alpine:3.20\n", encoding="utf-8")
    assert 'image = "alpine:3.20"' in compose_to_manifest(compose)


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
