"""Phase 4: enumeration, ownership bucketing, leases, adoption.

Unit tests drive a fake engine, so they run everywhere without Docker.
"""

from __future__ import annotations

import datetime as dt
import json
import os
import subprocess
import sys
import time
from pathlib import Path

import pytest

from bosn import labels, resources
from bosn.clock import FakeClock
from bosn.engine import EngineResult
from bosn.registry import Registry
from bosn.resources import DiscoveredResource, ResourceScanner, ScanResult

OURS = "our-registry-uuid"
THEIRS = "someone-elses-uuid"


def _dead(pid, start=None):
    return False


def label_dict(registry: str = OURS, **overrides: str) -> dict[str, str]:
    base = labels.ResourceLabels(
        registry=registry,
        kind="volume",
        stack="test",
        generation="sha256:abc",
        scope="spec",
        workspace="/w",
        created="2026-08-13T00:00:00Z",
    ).to_dict()
    base.update(overrides)
    return base


class FakeEngine:
    """Records commands and replays canned output."""

    def __init__(self, listings: dict[str, list[dict]], inspects: dict[str, dict] | None = None):
        self.listings = listings
        self.inspects = inspects or {}
        self.commands: list[list[str]] = []

    def run(self, args: list[str], *, check: bool = False) -> EngineResult:
        self.commands.append(list(args))
        if "inspect" in args:
            name = args[-1]
            return EngineResult(0, json.dumps(self.inspects.get(name, {})), "")
        kind = (
            "volume"
            if args[0] == "volume"
            else "image"
            if args[0] == "images"
            else "container"
            if args[0] == "ps"
            else None
        )
        rows = self.listings.get(kind or "", [])
        return EngineResult(0, "\n".join(json.dumps(row) for row in rows), "")


# -- ownership bucketing ---------------------------------------------------


def test_scan_sorts_resources_into_owned_foreign_and_unlabeled() -> None:
    engine = FakeEngine(
        {
            "volume": [
                {"Name": "ours", "Labels": json.dumps(label_dict())},
                {"Name": "theirs", "Labels": json.dumps(label_dict(registry=THEIRS))},
                {"Name": "naked", "Labels": ""},
            ]
        }
    )
    scan = ResourceScanner(engine).scan(OURS, kinds=["volume"])  # type: ignore[arg-type]

    assert [r.name for r in scan.owned] == ["ours"]
    assert [r.name for r in scan.foreign] == ["theirs"]
    assert [r.name for r in scan.unlabeled] == ["naked"]
    assert scan.foreign_registries == {THEIRS}
    assert scan.counts() == {"owned": 1, "foreign": 1, "unlabeled": 1}


def test_partially_labeled_resources_are_unlabeled_not_owned() -> None:
    """An incomplete label set is never ownership proof, even with our registry id."""
    partial = label_dict()
    del partial[labels.STACK]
    engine = FakeEngine({"volume": [{"Name": "partial", "Labels": json.dumps(partial)}]})
    scan = ResourceScanner(engine).scan(OURS, kinds=["volume"])  # type: ignore[arg-type]

    assert scan.owned == []
    assert [r.name for r in scan.unlabeled] == ["partial"]


def test_a_bosn_name_prefix_alone_is_not_ownership() -> None:
    engine = FakeEngine({"volume": [{"Name": "bosn-target-cache", "Labels": ""}]})
    scan = ResourceScanner(engine).scan(OURS, kinds=["volume"])  # type: ignore[arg-type]
    assert scan.owned == []
    assert len(scan.unlabeled) == 1


def test_labels_are_confirmed_by_inspect_when_the_listing_truncates_them() -> None:
    engine = FakeEngine(
        {"volume": [{"Name": "ours", "Labels": ""}]},
        inspects={"ours": label_dict()},
    )
    scan = ResourceScanner(engine).scan(OURS, kinds=["volume"])  # type: ignore[arg-type]
    assert [r.name for r in scan.owned] == ["ours"]
    assert any("inspect" in cmd for cmd in engine.commands)


def test_image_discovery_requests_and_keeps_the_full_immutable_id() -> None:
    image_id = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
    engine = FakeEngine(
        {"image": [{"ID": image_id, "Labels": json.dumps(label_dict(kind="image"))}]}
    )

    scan = ResourceScanner(engine).scan(OURS, kinds=["image"])  # type: ignore[arg-type]

    assert [resource.name for resource in scan.owned] == [image_id]
    assert ["images", "--no-trunc", "--format", "{{json .}}"] in engine.commands


@pytest.mark.parametrize(
    "blob",
    ["", "null", "<no value>", "map[]", "not json at all {", "[1,2,3]"],
)
def test_unparseable_label_blobs_never_produce_ownership(blob: str) -> None:
    assert resources._parse_labels(blob) == {} or not labels.is_owned_by(
        resources._parse_labels(blob), OURS
    )


def test_comma_separated_label_format_is_parsed() -> None:
    raw = ",".join(f"{k}={v}" for k, v in label_dict().items())
    parsed = resources._parse_labels(raw)
    assert labels.is_owned_by(parsed, OURS)


def test_engine_failure_yields_no_resources_rather_than_guesses() -> None:
    class Failing:
        def run(self, args, *, check=False):
            return EngineResult(1, "", "engine down")

    scan = ResourceScanner(Failing()).scan(OURS, kinds=["volume"])  # type: ignore[arg-type]
    assert scan.counts() == {"owned": 0, "foreign": 0, "unlabeled": 0}


# -- leases ----------------------------------------------------------------


@pytest.fixture
def clock() -> FakeClock:
    return FakeClock()


@pytest.fixture
def registry(tmp_path: Path, clock: FakeClock):
    with Registry(tmp_path / "r.sqlite3", clock=clock) as reg:
        yield reg


def _resource(registry: Registry):
    return registry.register_resource(
        kind="volume", name="v", stack="s", generation="g", scope="spec", workspace="/w"
    )


def test_a_live_holder_keeps_its_lease_past_the_ttl(registry: Registry, clock: FakeClock) -> None:
    """A 40-minute build is never collected out from under itself."""
    resource = _resource(registry)
    lease = registry.acquire_lease(resource.id, pid=999, proc_start=1.0, ttl_seconds=60)
    clock.advance(10_000)

    assert lease.expired_by_time(clock.now())  # TTL elapsed...
    assert not resources.lease_is_expired(  # ...but the holder is alive
        lease, clock.now(), alive_probe=lambda pid, start=None: True
    )
    assert resources.resource_is_leased(
        registry, resource.id, alive_probe=lambda pid, start=None: True
    )


def test_a_dead_holder_releases_after_one_ttl(registry: Registry, clock: FakeClock) -> None:
    resource = _resource(registry)
    lease = registry.acquire_lease(resource.id, pid=999, proc_start=1.0, ttl_seconds=60)

    clock.advance(59)
    assert not resources.lease_is_expired(lease, clock.now(), alive_probe=_dead)

    clock.advance(2)
    assert resources.lease_is_expired(lease, clock.now(), alive_probe=_dead)
    assert not resources.resource_is_leased(registry, resource.id, alive_probe=_dead)


def test_pid_reuse_with_a_different_process_start_releases_expired_lease(
    registry: Registry, clock: FakeClock, monkeypatch
) -> None:
    resource = _resource(registry)
    lease = registry.acquire_lease(resource.id, pid=os.getpid(), proc_start=1.0, ttl_seconds=60)
    monkeypatch.setattr(resources, "process_start_time", lambda _pid: 99.0)

    clock.advance(61)
    assert resources.lease_is_expired(lease, clock.now())


def test_process_alive_probe_is_true_for_this_process() -> None:
    assert resources.process_alive(os.getpid())
    assert not resources.process_alive(2**31 - 1)
    assert not resources.process_alive(0)


def test_matching_pid_and_process_start_stays_protected_beyond_the_ttl(
    registry: Registry, clock: FakeClock, monkeypatch
) -> None:
    """A holder whose stored identity still matches reality is never mistaken for reuse."""
    resource = _resource(registry)
    lease = registry.acquire_lease(resource.id, pid=os.getpid(), proc_start=1.0, ttl_seconds=60)
    monkeypatch.setattr(resources, "process_start_time", lambda _pid: 1.0)

    clock.advance(10_000)
    assert lease.expired_by_time(clock.now())  # TTL elapsed...
    assert not resources.lease_is_expired(lease, clock.now())  # ...but identity matches
    assert resources.resource_is_leased(registry, resource.id)


def test_acquire_lease_accepts_a_none_proc_start(registry: Registry, clock: FakeClock) -> None:
    """A failed identity probe at acquire time stores None rather than a wall-clock guess.

    ``process_alive`` treats a None ``proc_start`` as PID-only liveness, so a lease acquired
    without a usable identity probe still protects a live holder past its TTL.
    """
    resource = _resource(registry)
    lease = registry.acquire_lease(resource.id, pid=os.getpid(), proc_start=None, ttl_seconds=60)
    assert lease.proc_start is None
    stored = registry.get_lease(lease.id)
    assert stored is not None
    assert stored.proc_start is None

    clock.advance(10_000)
    assert lease.expired_by_time(clock.now())
    assert not resources.lease_is_expired(lease, clock.now())
    assert resources.resource_is_leased(registry, resource.id)


# -- process start parsing helpers ------------------------------------------


def test_parse_windows_process_start_converts_dotnet_ticks() -> None:
    # 2026-08-13T09:41:02.500Z as .NET DateTime.Ticks (100ns units since 0001-01-01).
    epoch_ticks = 62_135_596_800
    seconds_since_epoch = 1_786_699_262.5
    ticks = int((seconds_since_epoch + epoch_ticks) * 10_000_000)
    result = resources._parse_windows_process_start(f"{ticks}\r\n")
    assert result is not None
    assert abs(result - seconds_since_epoch) < 1e-3


@pytest.mark.parametrize("raw", ["", "not-a-number", "NaN", "Get-Process : Cannot find a process"])
def test_parse_windows_process_start_rejects_malformed_input(raw: str) -> None:
    assert resources._parse_windows_process_start(raw) is None


def test_parse_linux_process_start_computes_epoch_from_ticks_and_boot() -> None:
    # A realistic /proc/<pid>/stat line: comm can contain spaces/parens, hence the
    # rsplit-on-")" parsing. Field 22 (index 19 after the split) is starttime in ticks.
    stat_text = (
        "1234 (my proc (weird)) S 1 1234 1234 0 -1 4194304 100 0 0 0 "
        "10 2 0 0 20 0 4 0 5000 123456789 0 0 0 0 0 0 0 0 0 0 0 0 0 17 2 0 0 0 0 0"
    )
    proc_stat_text = "cpu  0 0 0 0 0 0 0 0 0 0\nbtime 1700000000\nprocesses 500\n"
    result = resources._parse_linux_process_start(stat_text, proc_stat_text, 100.0)
    assert result == 1700000000 + (5000 / 100.0)


@pytest.mark.parametrize(
    "stat_text,proc_stat_text,clk_tck",
    [
        ("", "btime 1700000000\n", 100.0),
        ("garbage no parens or fields", "btime 1700000000\n", 100.0),
        ("1234 (ok) S 1 1234", "btime 1700000000\n", 100.0),  # too few fields
        ("1234 (ok) S " + " ".join(["0"] * 30), "no btime line here\n", 100.0),
    ],
)
def test_parse_linux_process_start_rejects_malformed_input(
    stat_text: str, proc_stat_text: str, clk_tck: float
) -> None:
    assert resources._parse_linux_process_start(stat_text, proc_stat_text, clk_tck) is None


def test_parse_darwin_process_start_parses_lstart_format() -> None:
    result = resources._parse_darwin_process_start("Thu Aug 13 09:41:02 2026\n")
    assert result is not None

    expected = dt.datetime.strptime("Thu Aug 13 09:41:02 2026", "%a %b %d %H:%M:%S %Y").timestamp()
    assert result == expected


@pytest.mark.parametrize("raw", ["", "not a date", "2026-08-13 09:41:02"])
def test_parse_darwin_process_start_rejects_malformed_input(raw: str) -> None:
    assert resources._parse_darwin_process_start(raw) is None


def test_process_start_time_returns_a_plausible_epoch_for_this_process() -> None:
    """Integration: the current platform's dispatch produces a real, recent epoch time."""
    result = resources.process_start_time(os.getpid())
    assert result is not None
    now = time.time()
    assert result <= now + resources.PROCESS_START_TOLERANCE_SECONDS
    # Generous ceiling: CI machines can have long uptimes, but this *process* is young.
    assert now - result < 30 * 24 * 3600


# -- probe robustness (#45 pre-push review findings) ------------------------


def test_process_alive_treats_windows_access_denied_system_error_as_alive(monkeypatch) -> None:
    """os.kill(pid, 0) raises SystemError (not OSError) for another user's process on
    Windows -- CPython returns a result with ERROR_ACCESS_DENIED still set as an exception.
    SystemError escapes every existing except clause, so a poisoned lease aborted the whole
    prune_dead_leases loop. Access-denied means the process exists: treat it as alive, same
    fail-open posture as the PermissionError branch.
    """

    def _raises_system_error(pid: int, sig: int) -> None:
        raise SystemError("<class 'OSError'> returned a result with an exception set")

    monkeypatch.setattr(os, "kill", _raises_system_error)
    assert resources.process_alive(4) is True


def test_windows_process_start_probe_uses_cim_not_get_process(monkeypatch) -> None:
    """Get-Process's .StartTime throws access-denied on another user's process, silently
    yielding an empty stdout (stderr is discarded). Get-CimInstance does not have this
    problem, and the code comment already claimed it was in use -- assert the real command.
    """
    captured: dict[str, list[str]] = {}

    def _fake_run(args, **kwargs):
        captured["args"] = args
        return subprocess.CompletedProcess(args, 0, stdout="0\n", stderr="")

    monkeypatch.setattr(os, "name", "nt")
    monkeypatch.setattr(subprocess, "run", _fake_run)
    resources.process_start_time(4)

    command = captured["args"][-1]
    assert "Get-CimInstance" in command
    assert "Win32_Process" in command
    assert "Get-Process" not in command


def test_windows_probe_subprocess_call_has_a_timeout(monkeypatch) -> None:
    captured: dict[str, object] = {}

    def _fake_run(args, **kwargs):
        captured.update(kwargs)
        return subprocess.CompletedProcess(args, 0, stdout="0\n", stderr="")

    monkeypatch.setattr(os, "name", "nt")
    monkeypatch.setattr(subprocess, "run", _fake_run)
    resources.process_start_time(4)

    assert captured.get("timeout") == 5


def test_windows_probe_wedged_or_missing_powershell_returns_none_not_raise(monkeypatch) -> None:
    def _times_out(args, **kwargs):
        raise subprocess.TimeoutExpired(cmd=args, timeout=5)

    monkeypatch.setattr(os, "name", "nt")
    monkeypatch.setattr(subprocess, "run", _times_out)
    assert resources.process_start_time(4) is None

    def _missing(args, **kwargs):
        raise FileNotFoundError("powershell not found")

    monkeypatch.setattr(subprocess, "run", _missing)
    assert resources.process_start_time(4) is None


def test_darwin_probe_subprocess_call_has_a_timeout_and_pins_locale(monkeypatch) -> None:
    captured: dict[str, object] = {}

    def _fake_run(args, **kwargs):
        captured.update(kwargs)
        return subprocess.CompletedProcess(args, 0, stdout="Thu Aug 13 09:41:02 2026\n", stderr="")

    monkeypatch.setattr(os, "name", "posix")
    monkeypatch.setattr(sys, "platform", "darwin")
    monkeypatch.setattr(subprocess, "run", _fake_run)
    resources.process_start_time(4)

    assert captured.get("timeout") == 5
    env = captured.get("env")
    assert isinstance(env, dict)
    assert env.get("LC_ALL") == "C"


def test_darwin_probe_missing_ps_returns_none_not_raise(monkeypatch) -> None:
    def _missing(args, **kwargs):
        raise FileNotFoundError("ps not found")

    monkeypatch.setattr(os, "name", "posix")
    monkeypatch.setattr(sys, "platform", "darwin")
    monkeypatch.setattr(subprocess, "run", _missing)
    assert resources.process_start_time(4) is None


def test_parse_linux_process_start_rejects_zero_clock_ticks() -> None:
    """A zero SC_CLK_TCK would otherwise raise ZeroDivisionError, escaping process_alive."""
    stat_text = (
        "1234 (proc) S 1 1234 1234 0 -1 4194304 100 0 0 0 "
        "10 2 0 0 20 0 4 0 5000 123456789 0 0 0 0 0 0 0 0 0 0 0 0 0 17 2 0 0 0 0 0"
    )
    proc_stat_text = "cpu  0 0 0 0 0 0 0 0 0 0\nbtime 1700000000\nprocesses 500\n"
    assert resources._parse_linux_process_start(stat_text, proc_stat_text, 0.0) is None


# -- dead lease pruning ------------------------------------------------------


def test_prune_dead_leases_deletes_and_logs_an_expired_confirmed_dead_lease(
    registry: Registry, clock: FakeClock
) -> None:
    resource = _resource(registry)
    lease = registry.acquire_lease(resource.id, pid=999, proc_start=1.0, ttl_seconds=60)

    clock.advance(61)
    pruned = resources.prune_dead_leases(registry, alive_probe=_dead)

    assert pruned == [lease.id]
    assert registry.get_lease(lease.id) is None
    assert any(
        row["kind"] == "lease.pruned" and row["detail"] == lease.id for row in registry.events()
    )


def test_prune_dead_leases_never_touches_a_live_or_unexpired_lease(
    registry: Registry, clock: FakeClock
) -> None:
    resource = _resource(registry)
    live_holder = registry.acquire_lease(resource.id, pid=999, proc_start=1.0, ttl_seconds=60)
    unexpired = registry.acquire_lease(resource.id, pid=998, proc_start=1.0, ttl_seconds=6000)
    clock.advance(61)

    # Expired by time, but the holder is still reported alive: never pruned.
    always_alive = lambda pid, start=None: True  # noqa: E731
    pruned = resources.prune_dead_leases(registry, alive_probe=always_alive)
    assert pruned == []
    assert registry.get_lease(live_holder.id) is not None
    assert registry.get_lease(unexpired.id) is not None

    # Confirmed dead, but the TTL has not elapsed for the long-lived lease: still untouched.
    pruned = resources.prune_dead_leases(registry, alive_probe=_dead)
    assert pruned == [live_holder.id]
    assert registry.get_lease(unexpired.id) is not None


def test_prune_dead_leases_is_idempotent(registry: Registry, clock: FakeClock) -> None:
    resource = _resource(registry)
    registry.acquire_lease(resource.id, pid=999, proc_start=1.0, ttl_seconds=60)
    clock.advance(61)

    first = resources.prune_dead_leases(registry, alive_probe=_dead)
    assert len(first) == 1

    second = resources.prune_dead_leases(registry, alive_probe=_dead)
    assert second == []


# -- adoption --------------------------------------------------------------


def test_adoption_rebuilds_registry_rows_from_labels(registry: Registry, clock: FakeClock) -> None:
    """Losing the database is survivable: ownership lives in the labels."""
    scan = ScanResult(
        owned=[
            DiscoveredResource("volume", "ours-1", label_dict()),
            DiscoveredResource("volume", "ours-2", label_dict()),
        ],
        foreign=[DiscoveredResource("volume", "theirs", label_dict(registry=THEIRS))],
        unlabeled=[DiscoveredResource("volume", "naked", {})],
    )
    adopted = resources.adopt(registry, scan, clock=clock)

    assert sorted(adopted) == ["ours-1", "ours-2"]
    names = {r.name for r in registry.list_resources()}
    assert names == {"ours-1", "ours-2"}, "foreign and unlabeled must never be adopted"


def test_adoption_is_idempotent(registry: Registry, clock: FakeClock) -> None:
    scan = ScanResult(owned=[DiscoveredResource("volume", "ours-1", label_dict())])
    assert resources.adopt(registry, scan, clock=clock) == ["ours-1"]
    assert resources.adopt(registry, scan, clock=clock) == []
    assert len(registry.list_resources()) == 1


def test_startup_reconciliation_repairs_create_before_registry_crash(
    registry: Registry, clock: FakeClock
) -> None:
    """A complete owned engine object is sufficient to repair a missing SQLite row."""
    scan = ScanResult(owned=[DiscoveredResource("volume", "created-first", label_dict())])

    assert resources.reconcile_owned(registry, scan) == ["created-first"]
    recovered = registry.get_resource_by_engine_identity("volume", "created-first")
    assert recovered is not None
    assert recovered.state == "adopted"
    assert registry.generation_recorded(recovered.generation, recovered.stack, recovered.workspace)

    assert resources.reconcile_owned(registry, scan) == []
    assert len(registry.list_resources()) == 1


def test_startup_reconciliation_repairs_remove_before_registry_crash(
    registry: Registry,
) -> None:
    stale = registry.register_resource(
        kind="volume",
        name="removed-first",
        stack="dev",
        generation="digest",
        scope="spec",
        workspace="workspace",
    )
    prior = registry.list_resources()
    scan = ScanResult(scanned_kinds={"volume"})

    assert resources.reconcile_owned(registry, scan, prior_resources=prior) == []
    assert registry.get_resource(stale.id) is None


def test_recovery_recomputes_current_local_manifest_generation(
    registry: Registry, tmp_path: Path
) -> None:
    (tmp_path / "Dockerfile").write_text("FROM scratch\n", encoding="utf-8")
    (tmp_path / "bosn.toml").write_text(
        "[stack.dev]\ndockerfile = 'Dockerfile'\ndefault = true\n", encoding="utf-8"
    )
    stale = label_dict(
        **{
            labels.WORKSPACE: str(tmp_path),
            labels.STACK: "dev",
            labels.GENERATION: "sha256:old",
        }
    )
    scan = ScanResult(owned=[DiscoveredResource("volume", "cache", stale)])

    assert resources.recompute_manifest_generations(registry, scan) == 1
    from bosn.manifest import generation_digest, load

    manifest = load(tmp_path)
    assert registry.generation_recorded(
        generation_digest(manifest, manifest.stack("dev")), "dev", str(tmp_path)
    )


def test_adopted_resources_are_protected_by_the_quiet_period(clock: FakeClock) -> None:
    """Recovery is never followed by a mass age-out."""
    adopted_at = clock.now()
    assert resources.within_quiet_period(adopted_at, clock.advance(23 * 3600))
    assert not resources.within_quiet_period(adopted_at, clock.advance(2 * 3600))


def test_explicit_volume_transfer_stages_data_before_recreating_labels(
    registry: Registry,
) -> None:
    class TransferEngine:
        def __init__(self) -> None:
            self.commands: list[list[str]] = []

        def run(self, args: list[str], *, check: bool = False) -> EngineResult:
            self.commands.append(args)
            return EngineResult(0, "", "")

    engine = TransferEngine()
    resource = DiscoveredResource("volume", "foreign-cache", label_dict(registry=THEIRS))

    assert resources.transfer_volume(registry, engine, resource) == "foreign-cache"  # type: ignore[arg-type]
    assert engine.commands[0] == ["ps", "--all", "--filter", "volume=foreign-cache", "--quiet"]
    assert engine.commands[1][:2] == ["volume", "create"]
    assert engine.commands[1][2].startswith("bosn-transfer-")
    recreated = next(
        command
        for command in engine.commands
        if command[:2] == ["volume", "create"] and command[-1] == "foreign-cache"
    )
    assert any(f"{labels.REGISTRY}={registry.registry_id}" in arg for arg in recreated)


def test_explicit_volume_transfer_refuses_an_attached_volume(registry: Registry) -> None:
    class AttachedEngine:
        def run(self, args: list[str], *, check: bool = False) -> EngineResult:
            return EngineResult(0, "container-id", "")

    resource = DiscoveredResource("volume", "foreign-cache", label_dict(registry=THEIRS))
    with pytest.raises(resources.TransferError, match="attached"):
        resources.transfer_volume(registry, AttachedEngine(), resource)  # type: ignore[arg-type]
