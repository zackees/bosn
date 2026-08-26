"""``adopt --legacy``: documented clud/soldr/zccache contracts, never name guessing.

This is the acceptance test for GitHub issue #41's last unimplemented criterion.  Every
test either proves a family adopts *only* its own managed resources, or proves the module
refuses rather than guesses when the contract is not satisfied.
"""

from __future__ import annotations

import json

import pytest

from bosn import cli, legacy
from bosn.engine import EngineResult
from bosn.resources import DiscoveredResource, ResourceScanner, TransferError

OURS = "our-registry-uuid"


class FakeEngine:
    """Records commands and replays canned list/inspect output, like test_resources.py."""

    def __init__(self, listings: dict[str, list[dict]], inspects: dict[str, dict] | None = None):
        self.listings = listings
        self.inspects = inspects or {}
        self.commands: list[list[str]] = []

    def run(
        self, args: list[str], *, check: bool = False, timeout: float | None = None
    ) -> EngineResult:
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


class TransferEngine:
    """Accepts every engine call that ``recreate_volume_with_labels`` issues."""

    def __init__(self) -> None:
        self.commands: list[list[str]] = []

    def run(
        self, args: list[str], *, check: bool = False, timeout: float | None = None
    ) -> EngineResult:
        self.commands.append(list(args))
        return EngineResult(0, "", "")


class AttachedEngine:
    def run(
        self, args: list[str], *, check: bool = False, timeout: float | None = None
    ) -> EngineResult:
        if args[:2] == ["ps", "--all"]:
            return EngineResult(0, "container-id", "")
        return EngineResult(0, "", "")


def clud_volume_labels(**overrides: str) -> dict[str, str]:
    base = {
        "com.clud.docker-build.managed": "true",
        "com.clud.docker-build.stack": "soldr",
        "com.clud.docker-build.project-key": "abc123",
        "com.clud.docker-build.project-root": "/home/dev/project",
        "com.clud.docker-build.cache-role": "target",
    }
    base.update(overrides)
    return base


def soldr_volume_labels(**overrides: str) -> dict[str, str]:
    """Exactly what soldr's producer stamps on a volume -- no `.schema`.

    Verified in soldr/ci/perf_local.py: `docker volume create` passes only `.managed` and
    `.source-root`; `.schema` goes on the runner *container*, which is never adopted here.
    Putting `.schema` in this fixture would model the container and hide the real case.
    """
    base = {
        "io.soldr.perf-local.managed": "true",
        "io.soldr.perf-local.source-root": "/home/dev/checkout",
    }
    base.update(overrides)
    return base


def zccache_volume_labels(**overrides: str) -> dict[str, str]:
    base = {
        "io.zccache.perf-local.managed": "true",
        "io.zccache.perf-local.workspace": "/home/dev/zc-project",
    }
    base.update(overrides)
    return base


# -- family qualification: label-only, never name-based --------------------


def test_clud_family_qualifies_only_by_its_managed_label() -> None:
    assert legacy.CLUD.qualifies(clud_volume_labels())
    assert not legacy.CLUD.qualifies(soldr_volume_labels())
    assert not legacy.CLUD.qualifies({})


def test_a_legacy_looking_name_without_the_managed_label_is_never_adopted() -> None:
    """The anti-name-guessing test: name alone, even a perfect legacy-style name, proves
    nothing.  Only the family's own managed label is ownership proof -- exactly the
    standard labels.py already holds bosn's own contract to."""
    engine = FakeEngine(
        {
            "volume": [
                {"Name": "clud-docker-build-soldr-abc123-target", "Labels": ""},
            ]
        }
    )
    plan = legacy.plan_adoption(
        ResourceScanner(engine),  # type: ignore[arg-type]
        legacy.CLUD,
        registry_id=OURS,
        now=1000.0,
    )
    assert plan.eligible == ()
    assert plan.is_empty()


def test_each_family_adopts_only_its_own_resources() -> None:
    engine = FakeEngine(
        {
            "volume": [
                {"Name": "clud-cache", "Labels": json.dumps(clud_volume_labels())},
                {"Name": "soldr-cache", "Labels": json.dumps(soldr_volume_labels())},
                {"Name": "zc-cache", "Labels": json.dumps(zccache_volume_labels())},
            ]
        }
    )
    scanner = ResourceScanner(engine)  # type: ignore[arg-type]
    clud_plan = legacy.plan_adoption(scanner, legacy.CLUD, registry_id=OURS, now=1000.0)
    soldr_plan = legacy.plan_adoption(scanner, legacy.SOLDR, registry_id=OURS, now=1000.0)
    zccache_plan = legacy.plan_adoption(scanner, legacy.ZCCACHE, registry_id=OURS, now=1000.0)

    assert [e.resource.name for e in clud_plan.eligible] == ["clud-cache"]
    assert [e.resource.name for e in soldr_plan.eligible] == ["soldr-cache"]
    assert [e.resource.name for e in zccache_plan.eligible] == ["zc-cache"]


# -- label mapping -----------------------------------------------------------


def test_clud_maps_stack_and_project_root_onto_bosn_workspace_and_stack() -> None:
    mapped = legacy.CLUD.map_labels(clud_volume_labels(), registry_id=OURS, now=1000.0)
    assert mapped.registry == OURS
    assert mapped.kind == "volume"
    assert mapped.stack == "soldr"
    assert mapped.workspace == "/home/dev/project"
    assert mapped.scope == "stack"
    assert mapped.generation == legacy.LEGACY_GENERATION_SENTINEL


def test_soldr_maps_source_root_onto_workspace_and_uses_a_stack_sentinel() -> None:
    mapped = legacy.SOLDR.map_labels(soldr_volume_labels(), registry_id=OURS, now=1000.0)
    assert mapped.workspace == "/home/dev/checkout"
    assert mapped.stack == "soldr-perf-local"


def test_generation_and_created_never_come_from_the_legacy_producer() -> None:
    """No legacy producer stamps a content digest onto a volume; the sentinel/derivation
    is explicit, not a guess (see legacy.py's module docstring)."""
    mapped = legacy.CLUD.map_labels(clud_volume_labels(), registry_id=OURS, now=1_700_000_000.0)
    assert mapped.generation == legacy.LEGACY_GENERATION_SENTINEL
    assert mapped.created == "2023-11-14T22:13:20Z"


# -- unrecognized producer schema: refuse, never migrate blindly ------------


def test_an_unrecognized_soldr_schema_is_refused_not_migrated() -> None:
    with pytest.raises(legacy.LegacyAdoptionError, match="schema"):
        legacy.SOLDR.map_labels(
            soldr_volume_labels(**{"io.soldr.perf-local.schema": "1"}),
            registry_id=OURS,
            now=1000.0,
        )


def test_a_real_soldr_volume_adopts_even_though_it_carries_no_schema_label() -> None:
    """The contract soldr actually produces must adopt, or the family is dead on arrival.

    soldr stamps `.schema` on the runner *container* but not on its cache volumes
    (verified in ci/perf_local.py), and volumes are the only kind this path relabels.
    Requiring `.schema` would therefore refuse 100% of real soldr volumes -- a feature
    that rejects its own documented contract every time. An absent label is "no version
    claim"; `.managed` + `.source-root` still have to be present.
    """
    mapped = legacy.SOLDR.map_labels(soldr_volume_labels(), registry_id=OURS, now=1000.0)

    assert mapped.registry == OURS
    assert mapped.workspace == "/home/dev/checkout"
    assert mapped.kind == "volume"


def test_all_five_real_soldr_volume_names_from_one_root_adopt_to_one_workspace() -> None:
    """Shaped like an actual checkout, not the two-label fixture in isolation.

    `Runner.volumes` in soldr/ci/perf_local.py returns exactly these five names for one
    checkout root (`target`, `cargo-home`, `soldr-home`, `uv-cache`, `venv`), each created
    with only `.managed` and `.source-root` (verified in `ensure_runner`'s `docker volume
    create` call -- no other label is ever passed to a volume). All five must adopt and
    land in the same workspace, or the family is dead on arrival against soldr's real shape.
    """
    root = "/home/dev/soldr"
    names = [
        "soldr-perf-target-soldr-a6c74af0",
        "soldr-perf-cargo-home-soldr-a6c74af0",
        "soldr-perf-soldr-home-soldr-a6c74af0",
        "soldr-perf-uv-cache-soldr-a6c74af0",
        "soldr-perf-venv-soldr-a6c74af0",
    ]
    engine = FakeEngine(
        {
            "volume": [
                {
                    "Name": name,
                    "Labels": json.dumps(
                        soldr_volume_labels(
                            **{
                                "io.soldr.perf-local.source-root": root,
                            }
                        )
                    ),
                }
                for name in names
            ]
        }
    )
    plan = legacy.plan_adoption(
        ResourceScanner(engine),  # type: ignore[arg-type]
        legacy.SOLDR,
        registry_id=OURS,
        now=1000.0,
    )
    assert {entry.resource.name for entry in plan.eligible} == set(names)
    assert {entry.new_labels.workspace for entry in plan.eligible} == {root}
    assert plan.refused == ()
    assert plan.skipped_immutable == ()


def test_two_checkout_roots_map_to_two_distinct_workspaces_not_one() -> None:
    """The per-worktree duplication issue #75 measures: two checkouts' volumes must never
    collide into a single adopted workspace, even though both carry the same producer
    labels and only differ by `.source-root`."""
    root_a = "/home/dev/soldr"
    root_b = "/home/dev/soldr2"
    engine = FakeEngine(
        {
            "volume": [
                {
                    "Name": "soldr-perf-target-soldr-a6c74af0",
                    "Labels": json.dumps(
                        soldr_volume_labels(**{"io.soldr.perf-local.source-root": root_a})
                    ),
                },
                {
                    "Name": "soldr-perf-target-soldr2-b1c2d3e4",
                    "Labels": json.dumps(
                        soldr_volume_labels(**{"io.soldr.perf-local.source-root": root_b})
                    ),
                },
            ]
        }
    )
    plan = legacy.plan_adoption(
        ResourceScanner(engine),  # type: ignore[arg-type]
        legacy.SOLDR,
        registry_id=OURS,
        now=1000.0,
    )
    workspaces_by_name = {
        entry.resource.name: entry.new_labels.workspace for entry in plan.eligible
    }
    assert workspaces_by_name == {
        "soldr-perf-target-soldr-a6c74af0": root_a,
        "soldr-perf-target-soldr2-b1c2d3e4": root_b,
    }
    assert len(set(workspaces_by_name.values())) == 2


def test_a_volume_from_a_different_producer_namespace_is_not_adopted_by_legacy_soldr() -> None:
    """Label-gated, never name-gated: a volume shaped like soldr's real target volume but
    stamped under a different producer's namespace (e.g. clud, which manages the same kind
    of Rust cache) must not qualify for `--legacy soldr`."""
    engine = FakeEngine(
        {
            "volume": [
                {
                    "Name": "clud-docker-build-soldr-abc123-target",
                    "Labels": json.dumps(
                        clud_volume_labels(**{"com.clud.docker-build.cache-role": "target"})
                    ),
                },
            ]
        }
    )
    plan = legacy.plan_adoption(
        ResourceScanner(engine),  # type: ignore[arg-type]
        legacy.SOLDR,
        registry_id=OURS,
        now=1000.0,
    )
    assert plan.eligible == ()
    assert plan.is_empty()


def test_a_soldr_volume_missing_its_source_root_still_fails_closed() -> None:
    """Relaxing the schema rule must not relax the required fields it sat in front of."""
    without_root = soldr_volume_labels()
    del without_root["io.soldr.perf-local.source-root"]
    with pytest.raises(legacy.LegacyAdoptionError, match="workspace"):
        legacy.SOLDR.map_labels(without_root, registry_id=OURS, now=1000.0)


def test_plan_adoption_puts_unrecognized_schema_volumes_in_refused_not_eligible() -> None:
    engine = FakeEngine(
        {
            "volume": [
                {
                    "Name": "soldr-stale-schema",
                    "Labels": json.dumps(
                        soldr_volume_labels(**{"io.soldr.perf-local.schema": "99"})
                    ),
                },
            ]
        }
    )
    plan = legacy.plan_adoption(
        ResourceScanner(engine),  # type: ignore[arg-type]
        legacy.SOLDR,
        registry_id=OURS,
        now=1000.0,
    )
    assert plan.eligible == ()
    assert len(plan.refused) == 1
    assert plan.refused[0][0].name == "soldr-stale-schema"
    assert "schema" in plan.refused[0][1]


# -- zccache: no observed producer, fails closed without a workspace -------


def test_zccache_without_a_workspace_label_fails_closed() -> None:
    labels_without_workspace = zccache_volume_labels()
    del labels_without_workspace["io.zccache.perf-local.workspace"]
    with pytest.raises(legacy.LegacyAdoptionError, match="workspace"):
        legacy.ZCCACHE.map_labels(labels_without_workspace, registry_id=OURS, now=1000.0)


def test_zccache_with_a_workspace_label_adopts_like_the_other_families() -> None:
    mapped = legacy.ZCCACHE.map_labels(zccache_volume_labels(), registry_id=OURS, now=1000.0)
    assert mapped.workspace == "/home/dev/zc-project"
    assert mapped.stack == "zccache-perf-local"


# -- containers/images: immutable, always skipped, never adopted -----------


def test_containers_and_images_are_skipped_not_adopted() -> None:
    engine = FakeEngine(
        {
            "container": [
                {"Names": "soldr-runner", "Labels": json.dumps(soldr_volume_labels())},
            ],
            "image": [
                {"ID": "sha256:deadbeef", "Labels": json.dumps(soldr_volume_labels())},
            ],
        }
    )
    plan = legacy.plan_adoption(
        ResourceScanner(engine),  # type: ignore[arg-type]
        legacy.SOLDR,
        registry_id=OURS,
        now=1000.0,
    )
    assert plan.eligible == ()
    assert {r.name for r in plan.skipped_immutable} == {"soldr-runner", "sha256:deadbeef"}


# -- unknown family: refuse and list known families -------------------------


def test_resolve_family_lists_known_families_on_refusal() -> None:
    with pytest.raises(legacy.UnknownLegacyFamilyError) as excinfo:
        legacy.resolve_family("acme-corp")
    message = str(excinfo.value)
    assert "clud" in message and "soldr" in message and "zccache" in message


def test_known_families_are_exactly_the_three_documented_ones() -> None:
    assert legacy.known_families() == ["clud", "soldr", "zccache"]


# -- apply_plan: staged relabel, same mechanism as --transfer --------------


def test_apply_plan_recreates_each_eligible_volume_with_the_new_labels() -> None:
    engine = TransferEngine()
    resource = DiscoveredResource("volume", "clud-cache", clud_volume_labels())
    mapped = legacy.CLUD.map_labels(clud_volume_labels(), registry_id=OURS, now=1000.0)
    plan = legacy.LegacyAdoptionPlan(
        family=legacy.CLUD,
        eligible=(legacy.LegacyPlanEntry(resource=resource, new_labels=mapped),),
    )
    adopted = legacy.apply_plan(engine, plan)  # type: ignore[arg-type]
    assert adopted == ["clud-cache"]
    recreated = next(
        cmd
        for cmd in engine.commands
        if cmd[:2] == ["volume", "create"] and cmd[-1] == "clud-cache"
    )
    assert any(f"com.zackees.bosn.registry={OURS}" in arg for arg in recreated)
    assert any("com.zackees.bosn.workspace=/home/dev/project" in arg for arg in recreated)


def test_apply_plan_refuses_an_attached_volume() -> None:
    resource = DiscoveredResource("volume", "clud-cache", clud_volume_labels())
    mapped = legacy.CLUD.map_labels(clud_volume_labels(), registry_id=OURS, now=1000.0)
    plan = legacy.LegacyAdoptionPlan(
        family=legacy.CLUD,
        eligible=(legacy.LegacyPlanEntry(resource=resource, new_labels=mapped),),
    )
    with pytest.raises(TransferError, match="attached"):
        legacy.apply_plan(AttachedEngine(), plan)  # type: ignore[arg-type]


# -- CLI: unknown --legacy value refuses without touching the daemon --------


def test_cli_unknown_legacy_family_refuses_and_lists_known_families(tmp_path, capsys) -> None:
    exit_code = cli.main(
        ["--state-dir", str(tmp_path), "--json", "adopt", "--legacy", "acme", "--yes"]
    )
    assert exit_code == 1
    payload = json.loads(capsys.readouterr().out)
    assert payload["code"] == "adopt.unknown_legacy_family"
    assert "clud" in payload["next"] and "soldr" in payload["next"] and "zccache" in payload["next"]


# -- CLI: missing --yes reports the plan and mutates nothing ---------------


def test_cli_missing_yes_reports_the_plan_without_mutating(tmp_path, capsys, monkeypatch) -> None:
    from bosn import daemon as daemon_mod
    from bosn import engine as engine_mod

    def fake_request(verb, *_args, **_kwargs):
        assert verb == "status"
        return {"ok": True, "registry_id": OURS}

    engine = FakeEngine(
        {"volume": [{"Name": "clud-cache", "Labels": json.dumps(clud_volume_labels())}]}
    )
    monkeypatch.setattr(daemon_mod, "request", fake_request)
    monkeypatch.setattr(engine_mod, "Engine", lambda *_a, **_k: engine)

    exit_code = cli.main(["--state-dir", str(tmp_path), "--json", "adopt", "--legacy", "clud"])
    assert exit_code == 1
    payload = json.loads(capsys.readouterr().out)
    assert payload["applied"] is False
    assert payload["would_adopt"] == ["clud-cache"]
    # Nothing beyond the read-only listing commands was ever issued.
    assert not any(cmd[:2] == ["volume", "create"] for cmd in engine.commands)
    assert not any(cmd[:2] == ["volume", "rm"] for cmd in engine.commands)


# -- CLI: full flow relabels and registers, protected by the quiet period --


def test_cli_legacy_adoption_applies_and_registers_through_compose_adopt(
    tmp_path, capsys, monkeypatch
) -> None:
    from bosn import daemon as daemon_mod
    from bosn import engine as engine_mod

    calls: list[str] = []

    def fake_request(verb, *_args, **_kwargs):
        calls.append(verb)
        if verb == "status":
            return {"ok": True, "registry_id": OURS}
        if verb == "compose-adopt":
            return {"ok": True, "adopted": ["clud-cache"]}
        raise AssertionError(f"unexpected verb {verb}")

    monkeypatch.setattr(daemon_mod, "request", fake_request)

    def scan_only_once(_binary=None):
        # First call builds the scanner's engine; reuse a listing-capable engine that
        # also accepts the staged-copy commands `apply_plan` issues.
        class Combined:
            def __init__(self) -> None:
                self.commands: list[list[str]] = []

            def run(
                self, args: list[str], *, check: bool = False, timeout: float | None = None
            ) -> EngineResult:
                self.commands.append(list(args))
                if "inspect" in args:
                    return EngineResult(0, "{}", "")
                if args[:1] == ["volume"] and args[1] == "ls":
                    return EngineResult(
                        0,
                        json.dumps(
                            {"Name": "clud-cache", "Labels": json.dumps(clud_volume_labels())}
                        ),
                        "",
                    )
                if args[:2] == ["ps", "--all"] and "--filter" in args:
                    return EngineResult(0, "", "")  # not attached
                return EngineResult(0, "", "")

        return Combined()

    monkeypatch.setattr(engine_mod, "Engine", scan_only_once)

    exit_code = cli.main(
        ["--state-dir", str(tmp_path), "--json", "adopt", "--legacy", "clud", "--yes"]
    )
    assert exit_code == 0
    payload = json.loads(capsys.readouterr().out)
    assert payload["adopted"] == ["clud-cache"]
    assert payload["applied"] is True
    assert calls == ["status", "compose-adopt"]


def test_within_quiet_period_still_protects_a_freshly_adopted_legacy_resource() -> None:
    """Registration itself is the daemon's `compose-adopt`, which calls `resources.adopt`
    -- the same function lost-registry recovery uses, so the same quiet-period guarantee
    applies without any extra plumbing here."""
    from bosn.resources import within_quiet_period

    adopted_at = 1_700_000_000.0
    assert within_quiet_period(adopted_at, adopted_at + 23 * 3600)
    assert not within_quiet_period(adopted_at, adopted_at + 25 * 3600)
