import re
from pathlib import Path
from typing import Any

import pytest

from bosn import daemon
from bosn.docker_cli import (
    DockerFrontDoorError,
    _compose_overlay,
    _run_compose,
    compose_to_manifest,
)
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

    def __init__(self, adopt_reply: dict[str, Any] | None = None) -> None:
        self.calls: list[str] = []
        self.adopt_reply = adopt_reply if adopt_reply is not None else {"ok": True, "adopted": []}

    def request(self, verb: str, *_args: Any, **_kwargs: Any) -> dict[str, Any]:
        self.calls.append(verb)
        if verb == "status":
            return {"ok": True, "registry_id": "reg-1"}
        if verb == "compose-adopt":
            return self.adopt_reply
        raise AssertionError(f"unexpected verb {verb!r}")


def _compose_file(tmp_path: Path) -> Path:
    compose = tmp_path / "compose.yaml"
    compose.write_text("services:\n  app:\n    image: alpine\n", encoding="utf-8")
    return compose


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
    assert fake_daemon.calls == ["status", "compose-adopt"]
