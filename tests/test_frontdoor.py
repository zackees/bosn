import json

import pytest

from bosn.frontdoor import (
    COMPOSE_COMMANDS,
    COMPOSE_FLAGS,
    COMPOSE_GLOBAL_FLAGS,
    FORWARD_SAFE_ALLOWLIST,
    VERBS,
    Category,
    ComposeFlagSpec,
    FlagStatus,
    VerbSpec,
    compose_flag_spec_for,
    resolve,
    resolve_compose_flag,
    spec_for,
    supported,
)

# Every (command, spec) pair across every declared compose sub-verb's own flags -- does
# not include COMPOSE_GLOBAL_FLAGS, which have no single owning command.
_ALL_COMPOSE_FLAG_ENTRIES: list[tuple[str, ComposeFlagSpec]] = [
    (command, spec) for command, flags in COMPOSE_FLAGS.items() for spec in flags
]

# Verbs that create, start, stop, remove, or otherwise mutate an engine-managed resource
# (container/image/volume/network). If a future edit ever adds one of these to
# FORWARD_SAFE_ALLOWLIST, the assertion below must fail -- that is the whole point of the
# allowlist.
_RESOURCE_MUTATING_VERBS = frozenset(
    {
        "run",
        "create",
        "build",
        "exec",
        "start",
        "stop",
        "restart",
        "kill",
        "rm",
        "rmi",
        "pull",
        "push",
        "tag",
        "cp",
        "network",
        "volume",
        "image",
        "container",
        "system",
        "save",
        "load",
        "export",
        "import",
        "commit",
        "rename",
        "attach",
    }
)


@pytest.mark.parametrize("spec", VERBS, ids=lambda spec: spec.verb)
def test_every_spec_is_internally_consistent(spec: VerbSpec) -> None:
    if spec.category is Category.REFUSE:
        assert spec.remedy, f"{spec.verb!r} is REFUSE but carries no remedy"
    assert isinstance(spec.verb, str) and spec.verb
    assert isinstance(spec.summary, str) and spec.summary


def test_no_duplicate_verbs() -> None:
    verbs = [spec.verb for spec in VERBS]
    assert len(verbs) == len(set(verbs)), "VERBS contains a duplicate verb"


@pytest.mark.parametrize("spec", VERBS, ids=lambda spec: spec.verb)
def test_spec_for_round_trips_every_table_entry(spec: VerbSpec) -> None:
    assert spec_for(spec.verb) is spec


def test_spec_for_unknown_verb_returns_none() -> None:
    """`spec_for` is a raw lookup: a miss is `None`, not an opinion."""
    assert spec_for("docker-compose-plugin-that-does-not-exist") is None


def test_resolve_treats_unknown_verb_as_refused() -> None:
    """This is the safety property: an unrecognized verb must never be forwarded.

    A verb absent from VERBS -- e.g. a not-yet-cataloged docker subcommand, or a typo --
    must resolve to REFUSE with a usable remedy, not silently pass through to the real
    engine and create an unlabeled, unregistered resource.
    """
    spec = resolve("some-verb-nobody-registered")
    assert spec.category is Category.REFUSE
    assert spec.remedy


@pytest.mark.parametrize("spec", VERBS, ids=lambda spec: spec.verb)
def test_resolve_agrees_with_the_table_for_known_verbs(spec: VerbSpec) -> None:
    assert resolve(spec.verb) is spec


def test_forward_set_contains_only_the_declared_safe_allowlist() -> None:
    """Table-driven guard against scope creep in FORWARD.

    Every FORWARD verb must be named in FORWARD_SAFE_ALLOWLIST, and nothing else may be
    in that allowlist. Adding a new FORWARD verb without also justifying it in the
    allowlist (or vice versa) fails this test.
    """
    forward_verbs = {spec.verb for spec in VERBS if spec.category is Category.FORWARD}
    assert forward_verbs == FORWARD_SAFE_ALLOWLIST


def test_forward_set_never_contains_a_resource_mutating_verb() -> None:
    """The load-bearing allowlist assertion: if a future edit ever forwards `run`, `rm`,
    `build`, or any other verb that can create/mutate a resource, this must fail.
    """
    forward_verbs = {spec.verb for spec in VERBS if spec.category is Category.FORWARD}
    assert forward_verbs.isdisjoint(_RESOURCE_MUTATING_VERBS)


@pytest.mark.parametrize("spec", VERBS, ids=lambda spec: spec.verb)
def test_refuse_entries_are_not_in_the_resource_mutating_allowlist_by_accident(
    spec: VerbSpec,
) -> None:
    """Sanity check the other direction: every verb this module already knows is
    resource-mutating must actually be categorized REFUSE (or GOVERNED, where bosn takes
    responsibility for the resource itself), never FORWARD.
    """
    if spec.verb in _RESOURCE_MUTATING_VERBS:
        assert spec.category is not Category.FORWARD


def test_supported_round_trips_through_json() -> None:
    payload = supported()
    text = json.dumps(payload)
    parsed = json.loads(text)
    assert parsed == payload


def test_supported_has_schema_version() -> None:
    payload = supported()
    assert isinstance(payload["schema_version"], int)
    assert payload["schema_version"] >= 1


def test_supported_lists_every_verb_with_category_and_summary() -> None:
    payload = supported()
    by_verb = {entry["verb"]: entry for entry in payload["verbs"]}
    assert set(by_verb) == {spec.verb for spec in VERBS}
    for spec in VERBS:
        entry = by_verb[spec.verb]
        assert entry["category"] == spec.category.value
        assert entry["summary"] == spec.summary
        assert entry["remedy"] == spec.remedy


def test_supported_refusals_carry_their_remedy() -> None:
    payload = supported()
    for entry in payload["verbs"]:
        if entry["category"] == Category.REFUSE.value:
            assert entry["remedy"]


def test_verbspec_refuse_without_remedy_is_rejected() -> None:
    with pytest.raises(ValueError, match="remedy"):
        VerbSpec(verb="bogus", category=Category.REFUSE, summary="no remedy given")


def test_governed_and_forward_categories_are_both_represented() -> None:
    categories = {spec.category for spec in VERBS}
    assert Category.GOVERNED in categories
    assert Category.FORWARD in categories
    assert Category.REFUSE in categories


# ---------------------------------------------------------------------------------------
# Compose flag surface (#47)
# ---------------------------------------------------------------------------------------


def test_compose_commands_matches_compose_flags_keys() -> None:
    """COMPOSE_COMMANDS is derived from COMPOSE_FLAGS, not a second hand-typed list."""
    assert COMPOSE_COMMANDS == tuple(COMPOSE_FLAGS.keys())


def test_compose_flags_declared_only_for_known_compose_commands() -> None:
    expected = {"up", "down", "logs", "ps", "build", "run", "exec", "config"}
    assert set(COMPOSE_COMMANDS) == expected


def test_compose_is_the_only_verb_with_flag_data() -> None:
    """Flag data belongs to the GOVERNED `compose` sub-surface only; nothing else in VERBS
    has a flag table, so there is nowhere else flag data could attach by accident.
    """
    compose_spec = spec_for("compose")
    assert compose_spec is not None
    assert compose_spec.category is Category.GOVERNED


@pytest.mark.parametrize("spec", COMPOSE_GLOBAL_FLAGS, ids=lambda spec: spec.flag)
def test_every_global_flag_declares_its_arity_and_is_accepted(spec: ComposeFlagSpec) -> None:
    assert spec.status is FlagStatus.ACCEPTED
    assert isinstance(spec.takes_value, bool)
    assert spec.remedy is None


@pytest.mark.parametrize(
    "entry", _ALL_COMPOSE_FLAG_ENTRIES, ids=lambda entry: f"{entry[0]}:{entry[1].flag}"
)
def test_every_compose_flag_declares_value_taking_arity(entry: tuple[str, ComposeFlagSpec]) -> None:
    _, spec = entry
    assert isinstance(spec.takes_value, bool)
    assert isinstance(spec.flag, str) and spec.flag
    assert isinstance(spec.summary, str) and spec.summary


@pytest.mark.parametrize(
    "entry", _ALL_COMPOSE_FLAG_ENTRIES, ids=lambda entry: f"{entry[0]}:{entry[1].flag}"
)
def test_refused_compose_flags_carry_a_remedy(entry: tuple[str, ComposeFlagSpec]) -> None:
    _, spec = entry
    if spec.status is FlagStatus.REFUSED:
        assert spec.remedy
    else:
        assert spec.remedy is None


def test_compose_flag_spec_rejects_refused_without_remedy() -> None:
    with pytest.raises(ValueError, match="remedy"):
        ComposeFlagSpec(flag="--bogus", status=FlagStatus.REFUSED, summary="no remedy given")


def test_compose_flag_spec_rejects_accepted_with_remedy() -> None:
    with pytest.raises(ValueError, match="remedy"):
        ComposeFlagSpec(
            flag="--bogus",
            status=FlagStatus.ACCEPTED,
            summary="has a remedy but is accepted",
            remedy="should not be here",
        )


@pytest.mark.parametrize("command", COMPOSE_COMMANDS)
def test_no_duplicate_flag_spellings_within_a_command(command: str) -> None:
    """No flag (including any alias) may be declared twice within one sub-verb's own
    table -- a duplicate there is a table bug, unlike the deliberate global-flag override
    `logs`' `-f` performs (covered by the collision test below).
    """
    seen: list[str] = []
    for spec in COMPOSE_FLAGS[command]:
        seen.extend(spec.names())
    assert len(seen) == len(set(seen)), f"duplicate flag spelling within `compose {command}`"


def test_no_duplicate_flag_spellings_among_global_flags() -> None:
    seen: list[str] = []
    for spec in COMPOSE_GLOBAL_FLAGS:
        seen.extend(spec.names())
    assert len(seen) == len(set(seen))


@pytest.mark.parametrize(
    "entry", _ALL_COMPOSE_FLAG_ENTRIES, ids=lambda entry: f"{entry[0]}:{entry[1].flag}"
)
def test_declared_flags_round_trip_through_compose_flag_spec_for(
    entry: tuple[str, ComposeFlagSpec],
) -> None:
    command, spec = entry
    for name in spec.names():
        assert compose_flag_spec_for(command, name) is spec


@pytest.mark.parametrize("spec", COMPOSE_GLOBAL_FLAGS, ids=lambda spec: spec.flag)
def test_global_flags_are_visible_from_every_command_unless_overridden(
    spec: ComposeFlagSpec,
) -> None:
    for command in COMPOSE_COMMANDS:
        for name in spec.names():
            command_own_names = {n for s in COMPOSE_FLAGS[command] for n in s.names()}
            if name in command_own_names:
                continue  # the sub-verb deliberately overrides this spelling
            assert compose_flag_spec_for(command, name) is spec


def test_logs_overrides_the_global_file_flag_spelling() -> None:
    """The documented collision: `-f` means `--follow` on `compose logs`, not bosn's
    global `--file`. The sub-verb's own row must win the lookup, and it must be refused
    with a remedy that names the collision rather than silently behaving like `--file`.
    """
    spec = compose_flag_spec_for("logs", "-f")
    assert spec is not None
    assert spec.status is FlagStatus.REFUSED
    assert spec.aliases == ("--follow",)
    assert spec.remedy


def test_resolve_compose_flag_never_returns_none_for_unknown_flag() -> None:
    spec = resolve_compose_flag("up", "--nonexistent-flag-nobody-typed")
    assert spec.status is FlagStatus.REFUSED
    assert spec.remedy


@pytest.mark.parametrize("command", COMPOSE_COMMANDS)
def test_resolve_compose_flag_agrees_with_declared_flags(command: str) -> None:
    for spec in COMPOSE_FLAGS[command]:
        for name in spec.names():
            assert resolve_compose_flag(command, name) is spec
    for spec in COMPOSE_GLOBAL_FLAGS:
        for name in spec.names():
            own_names = {n for s in COMPOSE_FLAGS[command] for n in s.names()}
            if name in own_names:
                continue
            assert resolve_compose_flag(command, name) is spec


def test_compose_flag_spec_for_rejects_unknown_command() -> None:
    with pytest.raises(ValueError):
        compose_flag_spec_for("not-a-compose-command", "-d")


def test_supported_includes_compose_flag_data() -> None:
    payload = supported()
    assert set(payload["compose"]["commands"]) == set(COMPOSE_COMMANDS)
    assert payload["compose"]["global_flags"]
    for command in COMPOSE_COMMANDS:
        assert command in payload["compose"]["flags"]


def test_supported_compose_flag_entries_match_the_table() -> None:
    payload = supported()
    for command, entries in payload["compose"]["flags"].items():
        specs = COMPOSE_FLAGS[command]
        assert len(entries) == len(specs)
        for entry, spec in zip(entries, specs, strict=True):
            assert entry["flag"] == spec.flag
            assert entry["aliases"] == list(spec.aliases)
            assert entry["status"] == spec.status.value
            assert entry["takes_value"] == spec.takes_value
            assert entry["summary"] == spec.summary
            assert entry["remedy"] == spec.remedy


def test_supported_refused_compose_flags_carry_their_remedy() -> None:
    payload = supported()
    for entries in payload["compose"]["flags"].values():
        for entry in entries:
            if entry["status"] == FlagStatus.REFUSED.value:
                assert entry["remedy"]


def test_supported_with_compose_flags_still_round_trips_through_json() -> None:
    """The pre-existing round-trip test already covers this, but is repeated here scoped
    to the new `compose` key specifically: aliases are tuples in the table and must be
    converted to lists before this payload is built, or `json.loads` would hand back a
    list where the in-memory payload held a tuple and the equality check would fail.
    """
    payload = supported()
    text = json.dumps(payload)
    parsed = json.loads(text)
    assert parsed["compose"] == payload["compose"]


@pytest.mark.parametrize(
    "entry", _ALL_COMPOSE_FLAG_ENTRIES, ids=lambda entry: f"{entry[0]}:{entry[1].flag}"
)
def test_compose_flag_fields_contain_no_pipe_or_newline(
    entry: tuple[str, ComposeFlagSpec],
) -> None:
    """Mirrors `test_gen_docker_support.test_table_fields_contain_no_pipe_or_newline`:
    the generator puts these fields verbatim into a Markdown pipe table with no escaping.
    """
    _, spec = entry
    for field in (spec.flag, spec.summary, spec.remedy, *spec.aliases):
        if field is None:
            continue
        assert "|" not in field, f"{spec.flag!r} field contains '|': {field!r}"
        assert "\n" not in field, f"{spec.flag!r} field contains a newline: {field!r}"
