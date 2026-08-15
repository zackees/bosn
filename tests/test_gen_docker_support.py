"""Anti-drift guard for the generated `docs/docker-support.md`.

The doc is generated from `frontdoor.VERBS`; nothing about its content is hand-maintained.
These tests exist to make a table change without regenerating fail CI -- see
`ci/gen_docker_support.py` for the generator itself.
"""

from __future__ import annotations

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "ci"))

import pytest

import gen_docker_support  # noqa: E402
from bosn.frontdoor import VERBS, Category, VerbSpec, supported  # noqa: E402


def test_committed_doc_matches_a_fresh_render() -> None:
    """The load-bearing test: if frontdoor.VERBS changes without regenerating the doc,
    this fails with the exact command to fix it.
    """
    assert gen_docker_support.OUTPUT_PATH.exists(), (
        f"{gen_docker_support.OUTPUT_PATH} does not exist. "
        f"Generate it with `{gen_docker_support.REGENERATE_COMMAND}` and commit the result."
    )
    committed = gen_docker_support.OUTPUT_PATH.read_bytes()
    fresh = gen_docker_support.render().encode("utf-8")
    assert committed == fresh, (
        f"{gen_docker_support.OUTPUT_PATH} is out of date with src/bosn/frontdoor.py. "
        f"Regenerate it with `{gen_docker_support.REGENERATE_COMMAND}` and commit the result."
    )


def test_check_mode_agrees_with_the_byte_comparison() -> None:
    assert gen_docker_support.check() is True


def test_render_is_deterministic() -> None:
    assert gen_docker_support.render() == gen_docker_support.render()


@pytest.mark.parametrize("spec", VERBS, ids=lambda spec: spec.verb)
def test_every_verb_appears_in_the_generated_doc(spec: VerbSpec) -> None:
    text = gen_docker_support.render()
    assert f"`{spec.verb}`" in text


def test_every_refuse_remedy_appears_in_the_generated_doc() -> None:
    text = gen_docker_support.render()
    for spec in VERBS:
        if spec.category is Category.REFUSE:
            assert spec.remedy in text, f"remedy for {spec.verb!r} missing from generated doc"


def test_schema_version_appears_in_the_generated_doc() -> None:
    payload = supported()
    text = gen_docker_support.render()
    assert f"`{payload['schema_version']}`" in text


@pytest.mark.parametrize("spec", VERBS, ids=lambda spec: spec.verb)
def test_table_fields_contain_no_pipe_or_newline(spec: VerbSpec) -> None:
    """The renderer puts every field verbatim into a Markdown pipe table with no escaping.

    A `|` or newline in a future verb/summary/remedy would silently corrupt the table
    while every other check here still passes (the render stays deterministic and the
    substring is still present in the raw text). Catch that at the source instead.
    """
    for field in (spec.verb, spec.summary, spec.remedy):
        if field is None:
            continue
        assert "|" not in field, (
            f"{spec.verb!r} field contains '|', which breaks the table: {field!r}"
        )
        assert "\n" not in field, f"{spec.verb!r} field contains a newline: {field!r}"
