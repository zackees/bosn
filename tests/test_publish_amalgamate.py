from __future__ import annotations

import importlib.util
import re
import shutil
import sys
from pathlib import Path

import pytest

SPEC = importlib.util.spec_from_file_location(
    "publish_amalgamate", Path("ci/publish_amalgamate.py")
)
assert SPEC is not None and SPEC.loader is not None
amalgamate = importlib.util.module_from_spec(SPEC)
# @dataclass resolves its module through sys.modules while the class is built.
sys.modules[SPEC.name] = amalgamate
SPEC.loader.exec_module(amalgamate)

MODULE_MAP = {module.crate: module.module for module in amalgamate.INTERNAL_MODULES}


# --- source rewriting ---------------------------------------------------------


def test_crate_paths_become_module_paths() -> None:
    text = "use crate::labels::Label;\npub(crate) fn f() -> crate::Error { todo!() }\n"
    out = amalgamate.rewrite_rust_source_for_amalgamation(
        text, module="core", module_map=MODULE_MAP
    )
    assert out == (
        "use crate::core::labels::Label;\npub(crate) fn f() -> crate::core::Error { todo!() }\n"
    )


def test_internal_crate_paths_become_sibling_modules() -> None:
    text = "use bosn_core::Plan;\nuse bosn_engine::{Engine, Probe};\nuse bosn_setup;\n"
    out = amalgamate.rewrite_rust_source_for_amalgamation(
        text, module="service", module_map=MODULE_MAP
    )
    assert out == (
        "use crate::core::Plan;\nuse crate::engine::{Engine, Probe};\nuse crate::setup;\n"
    )


def test_identifiers_that_merely_start_with_a_crate_name_are_untouched() -> None:
    text = "let bosn_core_version = 1;\nlet bosn_engine2 = 2;\n"
    assert (
        amalgamate.rewrite_rust_source_for_amalgamation(
            text, module="service", module_map=MODULE_MAP
        )
        == text
    )


def test_the_cli_reaches_modules_through_the_published_crate_name() -> None:
    text = (
        "use bosn_service::{Client, Service};\n"
        "let dir = bosn_service::mcp::default_state_dir();\n"
        "mod local { pub fn f() {} }\nuse crate::local::f;\n"
    )
    out = amalgamate.rewrite_binary_source(text, module_map=MODULE_MAP)
    # A binary is its own crate: `crate::` stays pointed at the binary.
    assert out == (
        "use bosn::service::{Client, Service};\n"
        "let dir = bosn::service::mcp::default_state_dir();\n"
        "mod local { pub fn f() {} }\nuse crate::local::f;\n"
    )


# --- manifest rewriting -------------------------------------------------------

MANIFEST = """[package]
name = "bosn"

[features]
default = []
native-test-helper = ["dep:libc", "bosn-engine/native-test-helper"]
migration-test-helper = ["bosn-registry/migration-test-helper"]

[dependencies]
# internal facade crates
bosn-core = { path = "../bosn-core" }
bosn-engine = { path = "../bosn-engine" }
# external
kernal-api = { version = "=0.1.24", default-features = false }
libc = { version = "=0.2.189", optional = true }

[dev-dependencies]
tempfile = "=3.27.0"
"""


def test_manifest_drops_internal_dependencies_and_feature_items(tmp_path: Path) -> None:
    manifest = tmp_path / "Cargo.toml"
    manifest.write_text(MANIFEST, encoding="utf-8")
    amalgamate.rewrite_bosn_manifest(manifest, MODULE_MAP)
    text = manifest.read_text(encoding="utf-8")
    assert "bosn-core =" not in text
    assert "bosn-engine =" not in text
    assert "# internal facade crates" not in text
    assert 'native-test-helper = ["dep:libc"]' in text
    assert "migration-test-helper = []" in text
    assert 'kernal-api = { version = "=0.1.24"' in text
    assert 'tempfile = "=3.27.0"' in text


# --- the published crate must stand alone -------------------------------------


def test_a_leftover_internal_reference_is_refused(tmp_path: Path) -> None:
    facade = tmp_path / "crates" / "bosn"
    (facade / "src").mkdir(parents=True)
    (facade / "Cargo.toml").write_text('[dependencies]\nserde = "1"\n', encoding="utf-8")
    (facade / "src" / "lib.rs").write_text("use bosn_core::Plan;\n", encoding="utf-8")
    with pytest.raises(RuntimeError, match="source references: bosn_core"):
        amalgamate.assert_publish_crate_is_self_contained(facade, MODULE_MAP)


def test_a_leftover_internal_dependency_is_refused(tmp_path: Path) -> None:
    facade = tmp_path / "crates" / "bosn"
    (facade / "src").mkdir(parents=True)
    (facade / "Cargo.toml").write_text(
        '[dependencies]\nbosn-core = { path = "../bosn-core" }\n', encoding="utf-8"
    )
    (facade / "src" / "lib.rs").write_text("pub mod core;\n", encoding="utf-8")
    with pytest.raises(RuntimeError, match="manifest dependencies: bosn-core"):
        amalgamate.assert_publish_crate_is_self_contained(facade, MODULE_MAP)


def test_an_include_outside_the_crate_is_refused(tmp_path: Path) -> None:
    crate = tmp_path / "crates" / "bosn-core"
    (crate / "src").mkdir(parents=True)
    (tmp_path / "shared.json").write_text("{}", encoding="utf-8")
    source = crate / "src" / "lib.rs"
    source.write_text('const J: &str = include_str!("../../../shared.json");\n', encoding="utf-8")
    target = tmp_path / "crates" / "bosn" / "src" / "core" / "mod.rs"
    target.parent.mkdir(parents=True)
    target.write_text(source.read_text(encoding="utf-8"), encoding="utf-8")
    with pytest.raises(RuntimeError, match="outside its crate"):
        amalgamate.relocate_includes(
            target, original=source, crate_dir=crate, facade=tmp_path / "crates" / "bosn"
        )


def test_a_dependency_feature_the_facade_lacks_is_refused(tmp_path: Path) -> None:
    crates = tmp_path / "crates"
    (crates / "bosn").mkdir(parents=True)
    (crates / "bosn" / "Cargo.toml").write_text(
        '[dependencies]\nserde_json = "=1.0.149"\n', encoding="utf-8"
    )
    (crates / "bosn-core").mkdir()
    (crates / "bosn-core" / "Cargo.toml").write_text(
        '[dependencies]\nserde_json = { version = "=1.0.149", features = ["float_roundtrip"] }\n',
        encoding="utf-8",
    )
    modules = (amalgamate.AmalgamatedModule("bosn-core", "core"),)
    assert amalgamate.facade_dependency_gaps(tmp_path, modules) == [
        "bosn-core needs serde_json feature float_roundtrip"
    ]


# --- the real tree --------------------------------------------------------------


@pytest.fixture
def amalgamated(tmp_path: Path) -> Path:
    """A copy of the real repository, amalgamated."""
    shutil.copytree(Path("crates"), tmp_path / "crates", ignore=shutil.ignore_patterns("target"))
    shutil.copy(Path("Cargo.toml"), tmp_path / "Cargo.toml")
    amalgamate.prepare_bosn_crate_for_publish(tmp_path)
    return tmp_path / "crates" / "bosn"


def test_the_real_tree_amalgamates_into_one_crate(amalgamated: Path) -> None:
    src = amalgamated / "src"
    lib = (src / "lib.rs").read_text(encoding="utf-8")
    for module in MODULE_MAP.values():
        assert f"pub mod {module};" in lib
        assert (src / module / "mod.rs").is_file()
        assert not (src / module / "lib.rs").exists()
        # Internal helper binaries stay behind; only the CLI ships.
        assert not (src / module / "bin").exists()
    assert (src / "bin" / "bosn.rs").is_file()
    manifest = (amalgamated / "Cargo.toml").read_text(encoding="utf-8")
    for crate in MODULE_MAP:
        assert not re.search(rf"^{re.escape(crate)}\s*=", manifest, re.MULTILINE)


def test_the_facade_covers_every_internal_dependency_and_feature() -> None:
    # In the workspace Cargo unifies features across crates, so a gap only
    # shows up in the published crate (#323: float_roundtrip on serde_json).
    assert amalgamate.facade_dependency_gaps(Path("."), amalgamate.INTERNAL_MODULES) == []


def test_every_cli_submodule_ships_with_the_cli(amalgamated: Path) -> None:
    """`bosn.rs` declares `#[path = "bosn/x.rs"] mod x;`; each must resolve (#326)."""
    cli = amalgamated / "src" / "bin" / "bosn.rs"
    declared = re.findall(r'#\[path = "([^"]+)"\]', cli.read_text(encoding="utf-8"))
    assert declared, "the CLI has path-declared submodules"
    for relative in declared:
        module = cli.parent / relative
        assert module.is_file(), module
        assert "bosn_service" not in module.read_text(encoding="utf-8"), module


def test_every_include_in_the_real_tree_still_resolves(amalgamated: Path) -> None:
    includes = 0
    for source in (amalgamated / "src").rglob("*.rs"):
        for relative in amalgamate.INCLUDE.findall(source.read_text(encoding="utf-8")):
            includes += 1
            assert (source.parent / relative).is_file(), (source, relative)
    # bosn-core's unit tests read a fixture from outside its src/ tree.
    assert includes >= 1


# A `pub fn` behind `#[cfg(...)]` (other attributes may sit between them).
GATED_FN = re.compile(r"#\[cfg\((?P<cfg>[^\]]*)\)\]\s*(?:#\[[^\]]*\]\s*)*pub fn (?P<name>\w+)")


def test_seams_the_unit_tests_call_compile_under_cfg_test(amalgamated: Path) -> None:
    """A test-only seam must reach every unit test that calls it (#380).

    In the workspace a test-helper feature (`native-test-helper`) is enabled by
    the calling crate's dev-dependency. The amalgamated crate has no such
    edge: its unit tests compile under `cfg(test)` alone, so a seam gated only
    on the feature is missing there unless the caller is gated on it too.
    """
    sources = {
        path: path.read_text(encoding="utf-8") for path in (amalgamated / "src").rglob("*.rs")
    }
    helpers = set(amalgamate.INTERNAL_FEATURE_REMOVALS)
    seams = []
    for path, text in sources.items():
        for match in GATED_FN.finditer(text):
            features = set(re.findall(r'feature\s*=\s*"([^"]+)"', match["cfg"]))
            # The bare `test` predicate, not a feature name that contains it.
            predicates = re.sub(r'"[^"]*"', '""', match["cfg"])
            if features & helpers and not re.search(r"\btest\b", predicates):
                seams.append((path, match["name"], features & helpers))
    stranded = [
        (caller.relative_to(amalgamated), name)
        for _, name, features in seams
        for caller, text in sources.items()
        if re.search(rf"\b{name}\s*\(", text)
        and not any(f'feature = "{feature}"' in text for feature in features)
    ]
    assert stranded == [], f"feature-only seams called from ungated code: {stranded}"


def test_amalgamating_twice_is_idempotent(tmp_path: Path) -> None:
    shutil.copytree(Path("crates"), tmp_path / "crates", ignore=shutil.ignore_patterns("target"))
    amalgamate.prepare_bosn_crate_for_publish(tmp_path)
    first = {
        path.relative_to(tmp_path): path.read_bytes()
        for path in (tmp_path / "crates" / "bosn").rglob("*")
        if path.is_file()
    }
    amalgamate.prepare_bosn_crate_for_publish(tmp_path)
    second = {
        path.relative_to(tmp_path): path.read_bytes()
        for path in (tmp_path / "crates" / "bosn").rglob("*")
        if path.is_file()
    }
    assert first == second
