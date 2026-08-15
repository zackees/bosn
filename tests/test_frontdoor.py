import json

import pytest

from bosn.frontdoor import (
    FORWARD_SAFE_ALLOWLIST,
    VERBS,
    Category,
    VerbSpec,
    resolve,
    spec_for,
    supported,
)

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
