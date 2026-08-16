"""End-to-end conformance fixture for the whole Compose sub-verb lifecycle (#96).

#94 made all eight `bosn-docker compose` sub-verbs reachable and governed, backed by unit
coverage plus two manual point-checks against real Docker. Nothing exercised a real
multi-service project through its whole lifecycle -- and the interesting failures live in
the seams between verbs, not inside any one of them: does a `build`-produced image actually
land in the registry with the full label contract (never just on the container)? Does the
lease `up` acquires survive `logs`/`ps` against the live stack and get released exactly
once? Does `down -v` prune only the registry rows Compose actually removed, leaving
everything else alone? Do `profiles` really gate a service, not just parse?

This module drives one vendored project -- an image service, a build-only service with a
two-line inline Dockerfile, a named volume, a network, and a profile-gated service -- through
`bosn.docker_cli.compose_main` (the real front door), never by shelling out to `docker
compose` directly. A real bosn daemon is required (`_run_compose` calls
`daemon.request("status")`), so this module starts one in-process on its own thread, bound to
its own state dir and therefore its own deterministic port (`daemon.port_for`) -- verified,
not assumed, to differ from the developer's real daemon's port before anything else runs.

Docker-marked: Linux CI only. Every engine object this module creates is named with a
per-run uuid and removed in a `finally` (compose `down -v --remove-orphans` through the same
front door, plus explicit removal of the built image and the bystander volume) so a failure
mid-test cannot leak resources onto the host -- see `tests/conftest.py`'s
`_cleanup_docker_test_resources` for the autouse safety net this module's own cleanup backs
up, not replaces.
"""

from __future__ import annotations

import datetime as dt
import json
import threading
import time
import uuid
from collections.abc import Callable, Iterator
from pathlib import Path

import pytest
import yaml

from bosn import daemon as daemon_mod
from bosn import labels
from bosn.docker_cli import compose_main
from bosn.engine import Engine
from bosn.registry import Resource

pytestmark = pytest.mark.docker


def wait_until(
    predicate: Callable[[], object], timeout: float = 30.0, interval: float = 0.05
) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if predicate():
            return True
        time.sleep(interval)
    return bool(predicate())


@pytest.fixture
def engine() -> Engine:
    return Engine()


@pytest.fixture
def served(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Iterator[daemon_mod.Daemon]:
    """A real, isolated bosn daemon on its own thread -- `_run_compose` requires one.

    `tests/conftest.py`'s autouse `isolated_state_dir` fixture already points
    `BOSN_STATE_DIR` at a random tmp dir per test, but that alone is not the isolation
    property this module needs proven: `daemon.port_for` special-cases the *default* state
    dir to a fixed well-known port (`DEFAULT_PORT`) and only hashes a port for anything
    else. The trap is ordering -- computing the port *after* pointing `BOSN_STATE_DIR` at
    our own state dir would make our state dir equal `default_state_dir()`, tripping that
    special case and silently binding (and querying) the developer's real daemon's port.
    Computing it first, while `BOSN_STATE_DIR` still points at conftest's own throwaway
    dir, gets the genuinely hashed port; pinning it via `BOSN_PORT` (which `port_for` always
    prefers) then makes the bind and every client request agree on it regardless of any
    later default-dir coincidence.
    """
    state_dir = tmp_path / "daemon-state"
    port = daemon_mod.port_for(state_dir)
    assert port != daemon_mod.DEFAULT_PORT, (
        "a test daemon's port must never coincide with the well-known default port -- "
        "that would mean this test could bind or talk to a developer's real daemon"
    )
    monkeypatch.setenv("BOSN_STATE_DIR", str(state_dir))
    monkeypatch.setenv("BOSN_PORT", str(port))

    instance = daemon_mod.Daemon(state_dir=state_dir, idle_retire_seconds=3600)
    # A fresh registry has no stored maintenance deadline, so a pass is due on the very
    # first watchdog tick -- and that pass runs real GC against a reachable engine. This
    # module is about the compose lifecycle, not about unattended reclamation, so leaving
    # it armed means an unrelated GC pass can delete resources mid-test and fail an
    # assertion about compose's own behavior. It did exactly that on the Linux CI lane:
    # the bystander volume below was collected between its registration and the `down -v`
    # assertion that depends on it still being there. Pushed out of the way for the same
    # reason and in the same way the daemon tests do it (issue #95).
    instance._set_next_maintenance(instance.clock.now() + 3600)
    thread = threading.Thread(target=instance.serve_forever, daemon=True)
    thread.start()
    assert wait_until(lambda: daemon_mod.is_serving(state_dir), timeout=30), (
        "daemon did not come up in time"
    )
    try:
        yield instance
    finally:
        instance.request_stop()
        thread.join(timeout=30)
        instance.shutdown()


@pytest.fixture
def project(tmp_path: Path) -> tuple[Path, str, dict[str, str]]:
    """A minimal multi-service Compose project covering every shape #96 names.

    - `web`      a prebuilt-image service (`alpine`, already on the CI host)
    - `worker`   a build-only service (`build:`, no `image:`) with a 2-line Dockerfile
    - `sidecar`  behind a non-active `profiles:` entry -- the original #47 bug was
                 `unsupported compose key 'profiles'`; this keeps it covered end to end,
                 not just at parse time
    - `data`     a named volume, referenced only by `web`
    - `appnet`   a network, referenced by every service

    Volume/network names carry a per-run uuid suffix so a crashed earlier run's
    same-named engine objects (Compose itself already namespaces by project, but the
    volume/network *labels* this test asserts on are keyed by name, not project) can never
    collide with this run's.
    """
    run_id = uuid.uuid4().hex[:8]
    root = tmp_path / "proj"
    root.mkdir()
    worker_dir = root / "worker"
    worker_dir.mkdir()
    # The 2-line inline Dockerfile #96 asks for -- just enough to prove a build actually
    # happens, nothing more.
    (worker_dir / "Dockerfile").write_text(
        f"FROM alpine:3.20\nRUN echo {run_id} > /marker\n", encoding="utf-8"
    )
    names = {"volume": f"data-{run_id}", "network": f"appnet-{run_id}", "marker": run_id}
    compose_text = f"""\
services:
  web:
    image: alpine:3.20
    command: ["sleep", "600"]
    volumes:
      - {names["volume"]}:/data
    networks:
      - {names["network"]}
  worker:
    build:
      context: ./worker
    command: ["sleep", "600"]
    networks:
      - {names["network"]}
  sidecar:
    image: alpine:3.20
    profiles: ["extra"]
    command: ["sleep", "600"]
    networks:
      - {names["network"]}

volumes:
  {names["volume"]}:

networks:
  {names["network"]}:
"""
    compose_path = root / "compose.yaml"
    compose_path.write_text(compose_text, encoding="utf-8")
    return compose_path, run_id, names


def _run(compose_path: Path, *args: str) -> int:
    """Drive one sub-verb through the real front door -- `bosn-compose`'s own entry point."""
    return compose_main(["-f", str(compose_path), *args])


def _by_kind(resources: list[Resource], kind: str) -> list[Resource]:
    # `list_resources()` is already scoped to the one registry it was read from, so
    # filtering by kind is all a caller here ever needs.
    return [r for r in resources if r.kind == kind]


def _current_labeled_image_ids(engine: Engine, registry_id: str) -> list[str]:
    listed = engine.run(
        [
            "image",
            "ls",
            "--no-trunc",
            "--filter",
            f"label={labels.REGISTRY}={registry_id}",
            "--format",
            "{{.ID}}",
        ]
    )
    return [line.strip() for line in listed.stdout.splitlines() if line.strip()]


def test_compose_lifecycle_through_the_real_front_door(
    engine: Engine,
    served: daemon_mod.Daemon,
    project: tuple[Path, str, dict[str, str]],
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capfd: pytest.CaptureFixture[str],
) -> None:
    compose_path, run_id, names = project
    registry = served.registry
    registry_id = registry.registry_id

    # A bystander volume this daemon's registry owns but Compose never touches -- the
    # control for the `down -v` pruning assertion below. If pruning were a blanket wipe of
    # every owned row instead of "only what the scan can no longer find", this would vanish
    # too; it must not, because it still physically exists on the engine.
    bystander_workspace = str((tmp_path / "elsewhere").resolve())
    bystander_name = f"bystander-{run_id}"
    bystander_labels = labels.ResourceLabels(
        registry=registry_id,
        kind="volume",
        stack="bystander",
        generation="g",
        scope="spec",
        workspace=bystander_workspace,
        # Stamped now, not a fixed past date. A hardcoded timestamp ages relative to
        # whenever the suite happens to run, so it silently drifts past the 72h warm TTL
        # and turns this control volume into a GC candidate -- which is a property of the
        # calendar, not of anything this test means to assert.
        created=dt.datetime.now(dt.UTC).strftime("%Y-%m-%dT%H:%M:%SZ"),
    )
    engine.run(["volume", "create", *bystander_labels.to_docker_args(), bystander_name], check=True)

    try:
        # -- 1. build: a service's image lands in the registry with the full contract ----
        capfd.readouterr()
        code = _run(compose_path, "build")
        assert code == 0

        # Registration is not synchronous with the CLI returning: `_reconcile_after_compose`
        # gives the daemon's IPC round trip a fixed 10s budget (`ipc.DEFAULT_TIMEOUT`) and
        # treats a timeout as non-fatal by design (a bookkeeping hiccup must never mask
        # compose's own exit code) -- but the daemon keeps scanning in its own thread after
        # the client gives up, and the row lands whenever that scan actually finishes. A
        # bounded poll on the registry's own state is the real condition to wait on here
        # (see the module docstring's "prefer deterministic waits" guidance); a fixed sleep
        # would either flake under load or waste time when the daemon answers promptly.
        assert wait_until(
            lambda: len(_by_kind(registry.list_resources(), "image")) >= 1, timeout=90
        ), f"image was never registered; rows: {registry.list_resources()}"
        image_resources = _by_kind(registry.list_resources(), "image")
        assert len(image_resources) == 1, (
            "compose build must register exactly the one built image "
            f"(worker); got {image_resources}"
        )
        image_row = image_resources[0]
        assert image_row.stack == "worker"
        assert image_row.scope == "stack"

        listed = engine.run(
            [
                "image",
                "ls",
                "--no-trunc",
                "--filter",
                f"label={labels.REGISTRY}={registry_id}",
                "--filter",
                f"label={labels.KIND}=image",
                "--format",
                "{{.ID}}",
            ],
            check=True,
        )
        image_ids = [line.strip() for line in listed.stdout.splitlines() if line.strip()]
        assert len(image_ids) == 1, (
            f"expected exactly one labeled image on the engine, got {image_ids}"
        )
        image_id = image_ids[0]
        inspected = engine.run(
            ["image", "inspect", "--format", "{{json .Config.Labels}}", image_id], check=True
        )
        engine_labels = json.loads(inspected.stdout)
        assert labels.is_complete(engine_labels), f"incomplete label contract: {engine_labels}"
        assert engine_labels[labels.REGISTRY] == registry_id
        assert engine_labels[labels.KIND] == "image"
        assert engine_labels[labels.STACK] == "worker"
        # The regression #94 exists to prevent: a service's `labels:` land on the
        # container, never the image. Proving the image carries `kind=image` here -- not
        # `kind=container` -- is the direct check that the `build.labels` overlay actually
        # reached the image Compose produced.
        assert engine_labels[labels.KIND] != "container"

        assert registry.all_leases() == [], "build must not leave any lease open"

        # -- 2. config: renders with the overlay merged, and is valid YAML ---------------
        capfd.readouterr()
        code = _run(compose_path, "config")
        assert code == 0
        rendered = capfd.readouterr().out
        parsed_config = yaml.safe_load(rendered)
        assert labels.REGISTRY in json.dumps(parsed_config), (
            "the overlay's label contract must survive into the merged config"
        )
        # `docker compose config` filters by active profile, same as every other verb --
        # with no COMPOSE_PROFILES set yet, sidecar must not appear in the rendered set.
        assert "sidecar" not in parsed_config.get("services", {}), (
            "an inactive profile's service must not render in `compose config` either"
        )
        assert "worker" in parsed_config.get("services", {})
        worker_build = parsed_config["services"]["worker"].get("build", {})
        assert worker_build.get("labels", {}).get(labels.KIND) == "image", (
            "config must show the build.labels overlay merged onto the build-only service"
        )

        # -- 3. up -d --wait: acquires a project lease, releases it exactly once ---------
        code = _run(compose_path, "up", "-d", "--wait")
        assert code == 0
        assert registry.all_leases() == [], "up must release its project lease before returning"
        acquired = [e for e in registry.events(limit=10_000) if e["kind"] == "lease.acquired"]
        released = [e for e in registry.events(limit=10_000) if e["kind"] == "lease.released"]
        assert len(acquired) >= 1, "up must have leased at least the resources build registered"
        assert len(acquired) == len(released), (
            "every lease acquired so far must have been released exactly once, no more, no less"
        )

        running = engine.run(["compose", "-f", str(compose_path), "ps", "--format", "{{.Service}}"])
        # `docker compose` directly here is fine: this is a read-only assertion about what
        # actually came up, not bosn's front door under test.
        running_services = {line.strip() for line in running.stdout.splitlines() if line.strip()}
        assert "web" in running_services
        assert "worker" in running_services
        assert "sidecar" not in running_services

        # -- 4. logs / ps against the live stack: must not corrupt or double-release -----
        code = _run(compose_path, "logs")
        assert code == 0
        assert registry.all_leases() == [], "logs must not leave a lease dangling"
        code = _run(compose_path, "ps")
        assert code == 0
        assert registry.all_leases() == [], "ps must not leave a lease dangling"
        acquired_after = [e for e in registry.events(limit=10_000) if e["kind"] == "lease.acquired"]
        released_after = [e for e in registry.events(limit=10_000) if e["kind"] == "lease.released"]
        assert len(acquired_after) == len(released_after), (
            "logs/ps each acquire their own session lease and must release it exactly once; "
            "any mismatch here means a lease was corrupted or double-released"
        )
        # The image row is already registered by this point (phase 1), so `logs` and `ps`
        # must each have leased it under their own session -- not just "at least one of
        # them did" (a plain `>` would only prove that).
        assert len(acquired_after) >= len(acquired) + 2, (
            "logs and ps must each have leased their own session"
        )

        # -- 5. exec: reaches a real container with a real command, flags survive --------
        capfd.readouterr()
        code = _run(compose_path, "exec", "web", "ls", "-la", "/")
        assert code == 0, "exec against an already-up service must succeed"
        exec_output = capfd.readouterr().out
        # `ls -la` (not `ls`) is the whole point of this assertion: a long listing has a
        # leading "total N" line and permission strings, neither of which a bare `ls`
        # would print. If `-la` were swallowed by bosn's flag validation instead of passed
        # through to the container's argv, this would silently degrade to a bare `ls` and
        # still exit 0 -- so check the shape of the output, not just the exit code.
        assert "total" in exec_output, f"expected a long listing; got: {exec_output!r}"

        # -- 6. run: reaches a real container with a real command ------------------------
        capfd.readouterr()
        run_marker = f"run-ok-{run_id}"
        code = _run(compose_path, "run", "web", "echo", run_marker)
        assert code == 0
        run_output = capfd.readouterr().out
        assert run_marker in run_output, f"expected {run_marker!r} in run output: {run_output!r}"
        assert registry.all_leases() == [], "run must not leave a lease dangling"

        # -- 7. activate the profile, bring sidecar up too --------------------------------
        monkeypatch.setenv("COMPOSE_PROFILES", "extra")
        code = _run(compose_path, "up", "-d", "--wait")
        assert code == 0
        running = engine.run(["compose", "-f", str(compose_path), "ps", "--format", "{{.Service}}"])
        running_services = {line.strip() for line in running.stdout.splitlines() if line.strip()}
        assert "sidecar" in running_services, "activating its profile must bring sidecar up"

        # -- 8. down -v --remove-orphans: prunes exactly what Compose removed ------------
        # Same "registration is not synchronous" fact as phase 1 -- `up`'s own reconcile
        # is a background-completing scan on this host, so wait for both volume rows to
        # actually be present before using their absence/presence as the baseline below.
        #
        # The compose-managed volume is identified by its registry *stack* field, not its
        # engine name: `_compose_overlay`'s `label_block` writes `stack=<the compose file's
        # top-level volume key>` (`names["volume"]`), but Compose itself prefixes the engine
        # object's actual name with the project name (e.g. `proj_data-<run>`, not
        # `data-<run>`). The bystander volume has no such prefixing -- it was created
        # directly, not through Compose -- so its `.name` is exactly `bystander_name`.
        def _volume_stacks() -> set[str]:
            return {r.stack for r in registry.list_resources() if r.kind == "volume"}

        def _volume_names() -> set[str]:
            return {r.name for r in registry.list_resources() if r.kind == "volume"}

        assert wait_until(
            lambda: names["volume"] in _volume_stacks() and bystander_name in _volume_names(),
            timeout=90,
        ), (
            f"expected both volumes registered before down; "
            f"stacks={_volume_stacks()} names={_volume_names()}"
        )

        code = _run(compose_path, "down", "-v", "--remove-orphans")
        assert code == 0

        # Pruning is the closing reconcile's `prune_missing=True` path -- the same
        # background-scan timing applies, so poll for the compose-owned volume's row to
        # actually disappear rather than asserting immediately.
        assert wait_until(lambda: names["volume"] not in _volume_stacks(), timeout=90), (
            f"down -v never pruned {names['volume']!r}; stacks: {_volume_stacks()}"
        )

        post_down = registry.list_resources()
        post_down_volume_names = {r.name for r in post_down if r.kind == "volume"}
        post_down_image_names = {r.name for r in post_down if r.kind == "image"}
        assert bystander_name in post_down_volume_names, (
            "down -v must leave alone a registry row for a volume Compose did not touch -- "
            "a test that only checks removal cannot tell pruning from a blanket wipe"
        )
        assert image_row.name in post_down_image_names, (
            "down never removes images (no --rmi in this subset); its row must survive"
        )
    finally:
        monkeypatch.delenv("COMPOSE_PROFILES", raising=False)
        # Belt-and-braces teardown through the real front door, even if an assertion above
        # failed partway through the lifecycle -- exactly the #84-shaped leak this module
        # must never reproduce. `up`/`down` are each idempotent enough that calling `down`
        # on a project that never finished coming up is a normal no-op, not an error worth
        # failing the test over.
        try:
            _run(compose_path, "down", "-v", "--remove-orphans")
        except (OSError, RuntimeError):
            pass
        for image_id in _current_labeled_image_ids(engine, registry_id):
            engine.run(["image", "rm", "--force", image_id])
        engine.run(["volume", "rm", "--force", bystander_name])
