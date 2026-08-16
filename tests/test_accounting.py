"""Engine storage inventory is queried once and preserves Docker's ownership boundaries."""

from __future__ import annotations

import json

from bosn.accounting import StorageInventory, desktop_vhdx, engine_storage_path, resource_bytes
from bosn.engine import EngineResult
from bosn.registry import Resource


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


class FailingEngine:
    """Simulates `docker system df -v` failing outright (non-zero exit)."""

    def __init__(self) -> None:
        self.calls: list[list[str]] = []

    def run(self, args: list[str]) -> EngineResult:
        self.calls.append(args)
        return EngineResult(1, "", "Cannot connect to the Docker daemon")


class RaisingEngine:
    """Simulates the engine call itself raising (e.g. `EngineError` on timeout)."""

    def run(self, args: list[str]) -> EngineResult:
        raise RuntimeError("docker system df -v exceeded its 60-second deadline")


class UnparseableEngine:
    """Simulates `ok` output that is not valid JSON."""

    def run(self, args: list[str]) -> EngineResult:
        return EngineResult(0, "not json", "")


class EmptyOutputEngine:
    """Simulates a successful exit with no output at all -- not the documented healthy
    shape (one JSON line, even on an empty host), so it must not be trusted either."""

    def run(self, args: list[str]) -> EngineResult:
        return EngineResult(0, "", "")


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


def test_a_successful_measurement_is_marked_measured() -> None:
    """Regression guard: the common, healthy path must not start reporting `measured=False`."""
    inventory = StorageInventory.collect(Engine())  # type: ignore[arg-type]
    assert inventory.measured is True


def test_a_nonzero_exit_is_unmeasured_not_zero_bytes() -> None:
    """Issue #106: `docker system df -v` failing must not read as "measured, and it is
    zero" -- an empty `sizes` dict on failure was indistinguishable from a healthy, empty
    host until `measured` existed to tell them apart."""
    inventory = StorageInventory.collect(FailingEngine())  # type: ignore[arg-type]
    assert inventory.sizes == {}
    assert inventory.measured is False


def test_an_engine_exception_is_unmeasured() -> None:
    inventory = StorageInventory.collect(RaisingEngine())  # type: ignore[arg-type]
    assert inventory.sizes == {}
    assert inventory.measured is False


def test_unparseable_output_is_unmeasured() -> None:
    inventory = StorageInventory.collect(UnparseableEngine())  # type: ignore[arg-type]
    assert inventory.sizes == {}
    assert inventory.measured is False


def test_a_successful_exit_with_no_output_at_all_is_unmeasured() -> None:
    """A `returncode == 0` exit that produced zero parseable lines is not the documented
    healthy shape (one JSON object per invocation, even on an empty host) -- treat it as
    unmeasured rather than trusting a result that cannot be explained."""
    inventory = StorageInventory.collect(EmptyOutputEngine())  # type: ignore[arg-type]
    assert inventory.sizes == {}
    assert inventory.measured is False


def test_a_genuinely_empty_host_is_still_measured() -> None:
    """The legitimate "nothing to report" case (a fresh host, or every category empty)
    must stay indistinguishable from any other successful small measurement -- it is not
    itself evidence of a problem, and must not be conflated with collection failure."""

    class EmptyHostEngine:
        def run(self, args: list[str]) -> EngineResult:
            return EngineResult(0, json.dumps({"Volumes": [], "Containers": [], "Images": []}), "")

    inventory = StorageInventory.collect(EmptyHostEngine())  # type: ignore[arg-type]
    assert inventory.sizes == {}
    assert inventory.measured is True


def test_engine_storage_path_prefers_the_engine_root_over_registry_state(tmp_path) -> None:
    root = tmp_path / "engine-data"
    root.mkdir()
    state = tmp_path / "state"
    state.mkdir()

    assert engine_storage_path(RootEngine(str(root)), state) == root  # type: ignore[arg-type]


def test_network_bytes_are_known_zero_not_permanently_unmeasured() -> None:
    """`system df -v` has no row shape for networks; that must read as measured-zero.

    `gc.collect` refuses to declare byte pressure resolved while any resource is
    unmeasured (`resource_bytes(...) is None`). If a network fell through to that
    "unknown" path, every project with a Compose-created network would wedge byte
    pressure resolution forever -- there is no inventory row that could ever fill it in.
    """
    engine = Engine()
    network = Resource(
        id="r1",
        kind="network",
        name="proj_default",
        stack="s",
        generation="g",
        scope="spec",
        workspace="/w",
        created_at=0.0,
        last_used=0.0,
    )

    size = resource_bytes(engine, network)  # type: ignore[arg-type]

    assert size == 0
    # No inventory lookup was needed to answer -- and none is possible for a network.
    assert engine.calls == []


def test_desktop_vhdx_finds_docker_data_disk_in_configured_directory(tmp_path) -> None:
    data = tmp_path / "disk"
    data.mkdir()
    small = data / "ancillary.vhdx"
    small.write_bytes(b"x")
    docker_data = data / "docker_data.vhdx"
    docker_data.write_bytes(b"xx")

    assert desktop_vhdx(tmp_path) == docker_data
