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
