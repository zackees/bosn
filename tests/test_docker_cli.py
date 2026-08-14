import re
from pathlib import Path

import pytest

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
