#!/usr/bin/env python3
"""Build the crates.io `bosn` package as one self-contained crate.

In the workspace, `crates/bosn` is a thin facade that re-exports Bosn's
internal crates. crates.io gets a single `bosn` crate instead: this script
copies each internal crate's `src/` into the facade as a module, rewrites the
paths between them, carries the `bosn` CLI binary across, and strips the
internal dependencies from the manifest. The internal crates stay
`publish = false`.

The same approach as zackees/zccache's `ci/publish_amalgamate.py`. It rewrites
the tree in place, so run it in a disposable checkout, never the working copy:

    git worktree add ../bosn-publish HEAD
    python ci/publish_amalgamate.py --root ../bosn-publish
    cargo publish -p bosn --allow-dirty --manifest-path ../bosn-publish/Cargo.toml
"""

from __future__ import annotations

import argparse
import os
import re
import shutil
from collections.abc import Iterable, Sequence
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


@dataclass(frozen=True)
class AmalgamatedModule:
    crate: str
    module: str


INTERNAL_MODULES: tuple[AmalgamatedModule, ...] = (
    AmalgamatedModule("bosn-core", "core"),
    AmalgamatedModule("bosn-engine", "engine"),
    AmalgamatedModule("bosn-generation", "generation"),
    AmalgamatedModule("bosn-registry", "registry"),
    AmalgamatedModule("bosn-setup", "setup"),
    AmalgamatedModule("bosn-service", "service"),
)
# The facade keeps these feature names, because the copied sources gate on
# them, and drops the items that forward to internal crates.
INTERNAL_FEATURE_REMOVALS: dict[str, set[str]] = {
    "native-test-helper": {"bosn-engine/native-test-helper"},
    "migration-test-helper": {"bosn-registry/migration-test-helper"},
}
# The product CLI. Internal helper binaries under each crate's `src/bin/` are
# test harnesses and are not published.
CLI_CRATE = "bosn-service"
CLI_BINARY = Path("src/bin/bosn.rs")
# Files outside a crate's `src/` that its sources `include_str!`/`include_bytes!`
# are copied here, one directory per module so they cannot collide.
RELOCATED_INCLUDES = Path("amalgamated")
INCLUDE = re.compile(r'include_(?:str|bytes)!\(\s*"([^"]+)"\s*\)')


def prepare_bosn_crate_for_publish(
    root: Path = ROOT,
    modules: Sequence[AmalgamatedModule] = INTERNAL_MODULES,
) -> None:
    """Rewrite `crates/bosn` into the single crate uploaded to crates.io."""

    root = root.resolve()
    facade = root / "crates" / "bosn"
    facade_src = facade / "src"
    module_map = {module.crate: module.module for module in modules}

    relocated = facade / RELOCATED_INCLUDES
    if relocated.exists():
        shutil.rmtree(relocated)

    for module in modules:
        crate_dir = root / "crates" / module.crate
        source_src = crate_dir / "src"
        target_src = facade_src / module.module
        if not source_src.is_dir():
            raise FileNotFoundError(f"missing internal crate source: {source_src}")
        if target_src.exists():
            shutil.rmtree(target_src)
        shutil.copytree(
            source_src,
            target_src,
            ignore=shutil.ignore_patterns("bin", "__pycache__", "*.pyc"),
        )
        (target_src / "lib.rs").rename(target_src / "mod.rs")

        for target in sorted(target_src.rglob("*.rs")):
            relative = target.relative_to(target_src)
            original = source_src / ("lib.rs" if relative == Path("mod.rs") else relative)
            text = rewrite_rust_source_for_amalgamation(
                target.read_text(encoding="utf-8"),
                module=module.module,
                module_map=module_map,
            )
            target.write_text(text, encoding="utf-8")
            relocate_includes(target, original=original, crate_dir=crate_dir, facade=facade)

    cli = facade_src / "bin" / CLI_BINARY.name
    cli.parent.mkdir(parents=True, exist_ok=True)
    cli.write_text(
        rewrite_binary_source(
            (root / "crates" / CLI_CRATE / CLI_BINARY).read_text(encoding="utf-8"),
            module_map=module_map,
        ),
        encoding="utf-8",
    )

    write_publish_lib_rs(facade_src, modules)
    rewrite_bosn_manifest(facade / "Cargo.toml", module_map)
    assert_publish_crate_is_self_contained(facade, module_map)


def rust_crate_ident(crate_name: str) -> str:
    return crate_name.replace("-", "_")


def _internal_idents(module_map: dict[str, str]) -> list[tuple[str, str]]:
    # Longest first, so no crate name is rewritten inside a longer one.
    return sorted(
        ((rust_crate_ident(crate), module) for crate, module in module_map.items()),
        key=lambda item: len(item[0]),
        reverse=True,
    )


def rewrite_rust_source_for_amalgamation(
    text: str,
    *,
    module: str,
    module_map: dict[str, str],
) -> str:
    """Point a library module's paths at its new place in the facade crate."""

    text = re.sub(r"\bcrate::", f"crate::{module}::", text)
    for ident, target in _internal_idents(module_map):
        text = re.sub(rf"\b{re.escape(ident)}::", f"crate::{target}::", text)
        text = re.sub(rf"\b{re.escape(ident)}\b", f"crate::{target}", text)
    return text


def rewrite_binary_source(text: str, *, module_map: dict[str, str]) -> str:
    """A binary is its own crate: it reaches the modules as `bosn::<module>`."""

    for ident, target in _internal_idents(module_map):
        text = re.sub(rf"\b{re.escape(ident)}::", f"bosn::{target}::", text)
        text = re.sub(rf"\b{re.escape(ident)}\b", f"bosn::{target}", text)
    return text


def relocate_includes(target: Path, *, original: Path, crate_dir: Path, facade: Path) -> None:
    """Keep every `include_str!`/`include_bytes!` in `target` resolvable.

    Paths inside the crate's `src/` moved with it and need nothing. A file
    elsewhere in the crate (a test fixture) is copied under `amalgamated/` and
    the literal is rewritten to reach it. Anything outside the crate is refused.
    """

    text = target.read_text(encoding="utf-8")
    src = crate_dir / "src"

    def relocate(match: re.Match[str]) -> str:
        literal = match.group(1)
        resolved = Path(os.path.normpath(original.parent / literal))
        if resolved.is_relative_to(src):
            return match.group(0)
        if not resolved.is_relative_to(crate_dir):
            raise RuntimeError(
                f"{original}: include {literal!r} reaches outside its crate; "
                "the published crate could not carry it"
            )
        if not resolved.is_file():
            raise FileNotFoundError(f"{original}: include {literal!r} does not exist")
        module = target.relative_to(facade / "src").parts[0]
        copied = facade / RELOCATED_INCLUDES / module / resolved.relative_to(crate_dir)
        copied.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(resolved, copied)
        rewritten = Path(os.path.relpath(copied, target.parent)).as_posix()
        return match.group(0).replace(f'"{literal}"', f'"{rewritten}"')

    rewritten = INCLUDE.sub(relocate, text)
    if rewritten != text:
        target.write_text(rewritten, encoding="utf-8")


def write_publish_lib_rs(facade_src: Path, modules: Sequence[AmalgamatedModule]) -> None:
    declarations = "\n".join(f"pub mod {module.module};" for module in modules)
    text = f"""//! Bosn: a daemon that owns, bounds, and garbage-collects Docker development
//! resources, so agents can use Docker all day without filling the disk.
//!
//! This crate is amalgamated at release from Bosn's internal workspace crates;
//! each is one module below, with the same paths the workspace facade exports.

{declarations}
"""
    (facade_src / "lib.rs").write_text(text, encoding="utf-8")


def rewrite_bosn_manifest(manifest_path: Path, module_map: dict[str, str]) -> None:
    text = manifest_path.read_text(encoding="utf-8")
    text = strip_internal_dependency_lines(text, module_map.keys())
    for feature, removals in INTERNAL_FEATURE_REMOVALS.items():
        text = rewrite_feature_items(text, feature, remove=removals)
    manifest_path.write_text(text, encoding="utf-8")


def strip_internal_dependency_lines(text: str, internal_crates: Iterable[str]) -> str:
    internal = set(internal_crates)
    output: list[str] = []
    section = ""
    for line in text.splitlines(keepends=True):
        stripped = line.strip()
        header = re.match(r"^\[([^\]]+)\]$", stripped)
        if header:
            section = header.group(1)
        if section == "dependencies" and stripped == "# internal facade crates":
            continue
        if section in {"dependencies", "dev-dependencies"} and _dependency_name(line) in internal:
            continue
        output.append(line)
    return "".join(output)


def _dependency_name(line: str) -> str | None:
    match = re.match(r"^([A-Za-z0-9_-]+)\s*=", line)
    return match.group(1) if match else None


def rewrite_feature_items(text: str, feature: str, *, remove: set[str]) -> str:
    pattern = re.compile(rf"(?ms)^{re.escape(feature)}\s*=\s*\[(.*?)\]")
    match = pattern.search(text)
    if not match:
        return text
    items = [item for item in re.findall(r'"([^"]+)"', match.group(1)) if item not in remove]
    rendered = ", ".join(f'"{item}"' for item in items)
    return text[: match.start()] + f"{feature} = [{rendered}]" + text[match.end() :]


def assert_publish_crate_is_self_contained(facade: Path, module_map: dict[str, str]) -> None:
    manifest = (facade / "Cargo.toml").read_text(encoding="utf-8")
    source_text = "\n".join(
        path.read_text(encoding="utf-8") for path in sorted((facade / "src").rglob("*.rs"))
    )
    manifest_errors = [
        crate
        for crate in sorted(module_map)
        if re.search(rf"^{re.escape(crate)}\s*=", manifest, re.MULTILINE)
    ]
    source_errors = [
        rust_crate_ident(crate)
        for crate in sorted(module_map)
        if re.search(rf"\b{re.escape(rust_crate_ident(crate))}\b", source_text)
    ]
    if manifest_errors or source_errors:
        details = []
        if manifest_errors:
            details.append("manifest dependencies: " + ", ".join(manifest_errors))
        if source_errors:
            details.append("source references: " + ", ".join(source_errors))
        raise RuntimeError("amalgamated crate is not self-contained: " + "; ".join(details))


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument(
        "--root",
        type=Path,
        required=True,
        help="checkout to rewrite in place; never the working copy",
    )
    args = parser.parse_args(argv)
    prepare_bosn_crate_for_publish(args.root)
    print(f"{args.root / 'crates' / 'bosn'} is a self-contained crates.io package")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
