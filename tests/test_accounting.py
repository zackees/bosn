"""Engine storage inventory is queried once and preserves Docker's ownership boundaries."""

from __future__ import annotations

import json

from bosn.accounting import StorageInventory, desktop_vhdx, engine_storage_path
from bosn.engine import EngineResult


class Engine:
    def __init__(self) -> None:
        self.calls: list[list[str]] = []

    def run(self, args: list[str]) -> EngineResult:
        self.calls.append(args)
        return EngineResult(
            0,
            json.dumps(
                {
                    "Volumes": [{"Name": "work", "Size": "12.5MB"}],
                    "Images": [
                        {
                            "ID": "sha256:a",
                            "Size": "100MB",
                            "SharedSize": "90MB",
                            "UniqueSize": "10MB",
                        },
                        {
                            "ID": "sha256:b",
                            "Size": "100MB",
                            "SharedSize": "90MB",
                            "UniqueSize": "10MB",
                        },
                    ],
                    "Containers": [{"ID": "c", "Size": "4kB"}],
                }
            ),
            "",
        )


class RootEngine:
    def __init__(self, root: str) -> None:
        self.root = root

    def run(self, args: list[str]) -> EngineResult:
        assert args == ["info", "--format", "{{.DockerRootDir}}"]
        return EngineResult(0, self.root, "")


def test_system_df_inventory_attributes_volume_without_a_positional_argument() -> None:
    engine = Engine()
    inventory = StorageInventory.collect(engine)  # type: ignore[arg-type]

    assert engine.calls == [["system", "df", "-v", "--format", "{{json .}}"]]
    assert inventory.sizes[("volume", "work")] == 12_500_000
    assert (
        inventory.sizes[("image", "sha256:a")] + inventory.sizes[("image", "sha256:b")]
        == 20_000_000
    )
    assert inventory.sizes[("container", "c")] == 4_000


def test_engine_storage_path_prefers_the_engine_root_over_registry_state(tmp_path) -> None:
    root = tmp_path / "engine-data"
    root.mkdir()
    state = tmp_path / "state"
    state.mkdir()

    assert engine_storage_path(RootEngine(str(root)), state) == root  # type: ignore[arg-type]


def test_desktop_vhdx_finds_docker_data_disk_in_configured_directory(tmp_path) -> None:
    data = tmp_path / "disk"
    data.mkdir()
    small = data / "ancillary.vhdx"
    small.write_bytes(b"x")
    docker_data = data / "docker_data.vhdx"
    docker_data.write_bytes(b"xx")

    assert desktop_vhdx(tmp_path) == docker_data
