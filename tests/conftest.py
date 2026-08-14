"""Shared fixtures. Docker-marked tests skip themselves when no engine is reachable."""

from __future__ import annotations

import os
import warnings
from collections.abc import Iterator
from typing import Any

import pytest

from bosn.engine import Engine, EngineError, engine_reachable
from bosn.gc import _REMOVE_COMMANDS as GC_REMOVE_COMMANDS
from bosn.registry import Registry
from bosn.resources import ResourceScanner, process_start_time


@pytest.fixture(autouse=True)
def isolated_state_dir(tmp_path_factory, monkeypatch) -> None:
    """Never let a test touch the developer's real registry or spawn a real daemon there."""
    state_dir = tmp_path_factory.mktemp("bosn-state")
    monkeypatch.setenv("BOSN_STATE_DIR", str(state_dir))
    monkeypatch.delenv("BOSN_PORT", raising=False)


_LIVE_PROC_START: float | None = None
_LIVE_PROC_START_PROBED = False


def live_proc_start() -> float | None:
    """The real process creation time of the test process, for tests that hold a lease.

    A lease whose ``proc_start`` is a wall-clock guess no longer survives its TTL: liveness
    now proves holder identity, not just the PID. Tests that mean "this live process holds
    the lease" must therefore store the identity the probe will actually report. Probed once
    per session because the Windows probe spawns a PowerShell process.
    """
    global _LIVE_PROC_START, _LIVE_PROC_START_PROBED
    if not _LIVE_PROC_START_PROBED:
        _LIVE_PROC_START = process_start_time(os.getpid())
        _LIVE_PROC_START_PROBED = True
    return _LIVE_PROC_START


_ENGINE_REACHABLE: bool | None = None


def _reachable() -> bool:
    global _ENGINE_REACHABLE
    if _ENGINE_REACHABLE is None:
        _ENGINE_REACHABLE = engine_reachable()
    return bool(_ENGINE_REACHABLE)


@pytest.fixture(scope="session")
def engine() -> Engine:
    return Engine()


def pytest_runtest_setup(item: pytest.Item) -> None:
    if item.get_closest_marker("docker") and not _reachable():
        pytest.skip("no reachable Docker engine")


# -- Docker-marked test teardown (#84) --------------------------------------
#
# Each Docker-marked test builds a `Registry` inside pytest's `tmp_path`, converges a real
# stack, and creates real engine resources labeled with that registry's UUID. `tmp_path`
# is deleted at teardown, taking the sqlite registry with it -- and every live daemon then
# classifies the surviving engine resources as *foreign*, because `gc.py` refuses to delete
# anything it cannot re-prove ownership of from a *live* registry's own database. That
# refusal is correct and is exactly what makes the leak permanent: the one registry that
# could ever collect these resources is the one pytest just deleted.
#
# Removed in dependency order -- containers hold volume mounts and network endpoints, so
# they must go first; images are least constrained and go last. Mirrors
# `bosn.gc._REMOVAL_ORDER`.
_SWEEP_KINDS: tuple[str, ...] = ("container", "network", "volume", "image")


def _sweep_minted_registries(engine: Engine, registry_ids: set[str]) -> list[str]:
    """Remove every engine resource whose OWN labels name one of `registry_ids`.

    This is deliberately not `bosn.gc.Collector`: `Collector.collect()` is retention-gated
    -- a volume created seconds ago is `KEPT_WARM` for 72h, a machine-scoped cache only
    moves under storage pressure, a leased resource is protected indefinitely -- so it
    cannot honestly answer "delete every resource this test just created" without faking a
    clock or pressure state to defeat its own policy. What *is* reused from the real
    collection path is the ownership proof itself: `resource.complete` plus a registry-id
    match is the identical check `gc.Collector._ownership_proven` re-runs from the engine's
    labels before every delete, and the removal commands below are `gc.py`'s own
    `_REMOVE_COMMANDS`, not a bespoke `docker ... rm` invented for tests.

    Why this can never touch a live registry's resources: nothing is removed because of a
    name, a path, or a workspace label -- only because the resource's *own* label, freshly
    re-read from the engine, names an id present in `registry_ids`. That set is populated
    exclusively by wrapping `Registry.__init__` for the lifetime of one test (see the
    `_cleanup_docker_test_resources` fixture below), so it can only ever contain ids this
    test itself minted. A registry id nobody constructed in this process -- including this
    developer's real, live registry, which was never touched by the wrapped constructor --
    can never appear in `registry_ids`, and is therefore structurally unreachable by this
    sweep no matter what its resources are labeled or named.
    """
    if not registry_ids:
        return []
    scanner = ResourceScanner(engine)
    errors: list[str] = []
    for kind in _SWEEP_KINDS:
        args = GC_REMOVE_COMMANDS.get(kind)
        if args is None:
            continue
        for resource in scanner.discover(kind):
            if not resource.complete or resource.registry not in registry_ids:
                continue
            result = engine.run([*args, resource.name])
            if not result.ok:
                errors.append(f"{kind}:{resource.name}: {result.stderr or result.stdout}")
    return errors


@pytest.fixture(autouse=True)
def _cleanup_docker_test_resources(
    request: pytest.FixtureRequest, monkeypatch: pytest.MonkeyPatch
) -> Iterator[None]:
    """Autouse safety net: a Docker-marked test leaves no new engine resource behind.

    Only active for `@pytest.mark.docker` tests -- every other test never touches a real
    engine, so wrapping `Registry.__init__` for them would be pure overhead for no benefit.
    """
    if request.node.get_closest_marker("docker") is None:
        yield
        return

    minted_registry_ids: set[str] = set()
    real_init = Registry.__init__

    def _tracking_init(self: Registry, *args: Any, **kwargs: Any) -> None:
        real_init(self, *args, **kwargs)
        # Read-only opens never create resources, and every resource-creating flow already
        # has a writable `Registry` with the same id -- so a read-only open is excluded
        # from the minted set on purpose. This is what keeps a test that merely *reads* an
        # existing (possibly the developer's real) registry from ever poisoning the set
        # this sweep is allowed to delete against.
        if not self.read_only:
            minted_registry_ids.add(self.registry_id)

    monkeypatch.setattr(Registry, "__init__", _tracking_init)
    try:
        yield
    finally:
        try:
            errors = _sweep_minted_registries(Engine(), minted_registry_ids)
        except EngineError as exc:
            errors = [str(exc)]
        if errors:
            # Surfaced, never raised: a teardown failure must not turn a passing test red,
            # but it must not vanish silently either -- that is how "storage is bounded"
            # stops being true.
            warnings.warn(
                "docker test teardown could not remove some engine resources: " + "; ".join(errors),
                stacklevel=2,
            )
