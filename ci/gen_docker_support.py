"""Generator for `docs/docker-support.md`. Invoked by `uv run python ci/gen_docker_support.py`.

`src/bosn/frontdoor.py`'s `VERBS` table is the only source of truth for which `docker`
verbs bosn governs, forwards, or refuses -- see that module's docstring. This script's
whole job is to turn `frontdoor.supported()` into a human-readable Markdown reference
without adding a single fact of its own: every verb, category, summary, and remedy in the
output must trace back to a table row, never to a string typed here.

Run with no arguments to (re)write `docs/docker-support.md`. Run with `--check` to verify
the committed file still matches what the table generates, without writing anything --
that is what `tests/test_gen_docker_support.py`'s drift guard calls, and what a CI job
could call directly instead of duplicating the comparison.
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "src"))

from bosn.frontdoor import Category, supported  # noqa: E402

OUTPUT_PATH = ROOT / "docs" / "docker-support.md"
REGENERATE_COMMAND = "uv run python ci/gen_docker_support.py"

_CATEGORY_TITLES: dict[str, str] = {
    Category.GOVERNED.value: "Governed",
    Category.FORWARD.value: "Forwarded",
    Category.REFUSE.value: "Refused",
}

# Category order in the output. Fixed, not sorted, so regeneration is stable regardless of
# dict/enum iteration order and matches the order the module docstring introduces them in.
_CATEGORY_ORDER: tuple[str, ...] = (
    Category.GOVERNED.value,
    Category.FORWARD.value,
    Category.REFUSE.value,
)


def _verbs_by_category(payload: dict, category: str) -> list[dict]:
    return [entry for entry in payload["verbs"] if entry["category"] == category]


def render() -> str:
    """Render the full Markdown document from `frontdoor.supported()`.

    Pure function of the table: called with no arguments, returns the same string every
    time the table is unchanged, and is what both `main()` and the drift test call so the
    file on disk and the test's expectation can never be produced by two different code
    paths.
    """
    payload = supported()
    lines: list[str] = []
    lines.append("<!-- GENERATED FILE -- DO NOT EDIT BY HAND.")
    lines.append(f"     Regenerate with `{REGENERATE_COMMAND}`.")
    lines.append("     Source of truth: src/bosn/frontdoor.py (the VERBS table). -->")
    lines.append("")
    lines.append("# bosn-docker supported verbs")
    lines.append("")
    lines.append(
        "This is the full `bosn-docker` verb catalog, generated from the same "
        "`frontdoor.supported()` payload that `bosn-docker --supported --json` prints. "
        "Every verb bosn recognizes falls into exactly one of three categories."
    )
    lines.append("")
    lines.append(
        f"Schema version: `{payload['schema_version']}` "
        "(the shape of the `--supported --json` payload this doc reflects)."
    )
    lines.append("")

    for category in _CATEGORY_ORDER:
        entries = _verbs_by_category(payload, category)
        lines.append(f"## {_CATEGORY_TITLES[category]}")
        lines.append("")
        if category == Category.REFUSE.value:
            lines.append("| Verb | Summary | Remedy |")
            lines.append("| --- | --- | --- |")
            for entry in entries:
                lines.append(f"| `{entry['verb']}` | {entry['summary']} | {entry['remedy']} |")
        else:
            lines.append("| Verb | Summary |")
            lines.append("| --- | --- |")
            for entry in entries:
                lines.append(f"| `{entry['verb']}` | {entry['summary']} |")
        lines.append("")

    lines += _render_compose_flags(payload["compose"])

    # `_render_compose_flags()` ends with an empty separator line. Normalize that internal
    # layout detail to one POSIX terminal newline instead of emitting a trailing blank
    # Markdown paragraph, so the committed generated document has a stable ending.
    return "\n".join(lines).rstrip("\n") + "\n"


def _render_flag_table(flags: list[dict]) -> list[str]:
    """One flag table, accepted and refused rows together -- `Status`/`Remedy` columns
    make the refused rows self-explanatory without a second table per command.
    """
    lines = ["| Flag | Aliases | Takes value | Status | Summary | Remedy |"]
    lines.append("| --- | --- | --- | --- | --- | --- |")
    for entry in flags:
        aliases = ", ".join(f"`{alias}`" for alias in entry["aliases"]) or "--"
        takes_value = "yes" if entry["takes_value"] else "no"
        remedy = entry["remedy"] or "--"
        lines.append(
            f"| `{entry['flag']}` | {aliases} | {takes_value} | {entry['status']} | "
            f"{entry['summary']} | {remedy} |"
        )
    return lines


def _render_compose_flags(compose: dict) -> list[str]:
    """Render the `compose` verb's flag surface (#47): its sub-verb list, the flags shared
    across every sub-verb, and each sub-verb's own accepted/refused flags.

    A pure function of `frontdoor.supported()`'s `"compose"` key, same discipline
    `render()` holds itself to for the verb table above -- nothing typed here that isn't
    already in the payload.
    """
    lines: list[str] = []
    lines.append("## Compose flags")
    lines.append("")
    lines.append(
        "The `compose` verb's own sub-verb and flag surface (#47). A flag not listed as "
        "`accepted` for a sub-verb -- including one this table has never heard of -- is "
        "refused, named, with a remedy; see `bosn.frontdoor.resolve_compose_flag`."
    )
    lines.append("")
    lines.append("Sub-verbs: " + ", ".join(f"`{command}`" for command in compose["commands"]))
    lines.append("")
    lines.append("### Global flags")
    lines.append("")
    lines.append(
        "Apply to every sub-verb identically; parsed ahead of the sub-verb "
        "(`bosn-docker compose -f compose.yaml up`)."
    )
    lines.append("")
    lines += _render_flag_table(compose["global_flags"])
    lines.append("")
    for command in compose["commands"]:
        lines.append(f"### `compose {command}`")
        lines.append("")
        flags = compose["flags"][command]
        if flags:
            lines += _render_flag_table(flags)
        else:
            lines.append(
                "No sub-verb-specific flags are declared; only the global flags above apply."
            )
        lines.append("")
    return lines


def _write(text: str) -> None:
    OUTPUT_PATH.parent.mkdir(parents=True, exist_ok=True)
    with OUTPUT_PATH.open("w", encoding="utf-8", newline="\n") as f:
        f.write(text)


def check() -> bool:
    """Return True if the committed file matches a fresh render, False otherwise."""
    if not OUTPUT_PATH.exists():
        return False
    committed = OUTPUT_PATH.read_bytes()
    return committed == render().encode("utf-8")


def main(argv: list[str] | None = None) -> int:
    argv = sys.argv[1:] if argv is None else list(argv)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--check",
        action="store_true",
        help="verify docs/docker-support.md matches the table without writing it",
    )
    args = parser.parse_args(argv)

    if args.check:
        if check():
            print(f"OK: {OUTPUT_PATH} matches src/bosn/frontdoor.py")
            return 0
        print(
            f"DRIFT: {OUTPUT_PATH} does not match src/bosn/frontdoor.py. "
            f"Regenerate it with `{REGENERATE_COMMAND}` and commit the result.",
            file=sys.stderr,
        )
        return 1

    _write(render())
    print(f"wrote {OUTPUT_PATH}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
