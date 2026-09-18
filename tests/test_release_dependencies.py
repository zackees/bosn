import importlib.util
from pathlib import Path

_SPEC = importlib.util.spec_from_file_location(
    "verify_release_dependencies", Path("ci/verify_release_dependencies.py")
)
assert _SPEC is not None and _SPEC.loader is not None
_MODULE = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(_MODULE)
verify = _MODULE.verify


def test_every_repo_manifest_uses_the_published_kernel() -> None:
    # kernal-api is on crates.io, so the release gate must pass for the real
    # tree. Before it was published this asserted the opposite.
    manifests = {
        path.parent.name
        for path in Path("crates").glob("*/Cargo.toml")
        if "kernal-api" in path.read_text(encoding="utf-8")
    }
    # Named, not counted, so a new crate that takes kernal-api is a deliberate edit here.
    # `bosn` matters most: it is the one crate published to crates.io.
    assert manifests == {
        "bosn",
        "bosn-engine",
        "bosn-generation",
        "bosn-python",
        "bosn-registry",
        "bosn-service",
        "bosn-setup",
    }
    for crate in sorted(manifests):
        assert verify(Path("crates") / crate / "Cargo.toml") == [], crate


def test_exact_published_kernel_is_accepted(tmp_path: Path) -> None:
    manifest = tmp_path / "Cargo.toml"
    manifest.write_text('[dependencies]\nkernal-api = "=0.1.14"\n', encoding="utf-8")
    assert verify(manifest) == []


def test_valid_inline_table_dependency_is_accepted(tmp_path: Path) -> None:
    manifest = tmp_path / "Cargo.toml"
    manifest.write_text(
        '[dependencies]\nkernal-api = { version = "=0.1.14", default-features = false }\n',
        encoding="utf-8",
    )
    assert verify(manifest) == []


def test_toml_forms_and_source_bypasses_are_not_spoofed_by_comments(tmp_path: Path) -> None:
    manifest = tmp_path / "Cargo.toml"
    manifest.write_text(
        '# kernal-api = "=0.1.14"\n'
        '[dependencies.kernal-api]\nversion = "=0.1.14"\n'
        'git = "https://example.invalid/k"\n[patch.crates-io]\n'
        'kernal-api = { path = "../k" }\n',
        encoding="utf-8",
    )
    errors = verify(manifest)
    assert any("git or path" in error for error in errors)
    assert any("patch or replace" in error for error in errors)


def test_replace_rename_target_and_workspace_bypasses_are_rejected(tmp_path: Path) -> None:
    root = tmp_path / "Cargo.toml"
    root.write_text(
        '[workspace]\n[replace]\n"kernal-api:0.1.14" = { path = "kernel" }\n', encoding="utf-8"
    )
    member = tmp_path / "member" / "Cargo.toml"
    member.parent.mkdir()
    member.write_text(
        "[dependencies]\n"
        'k = { package = "kernal-api", version = "=0.1.14", workspace = true }\n'
        "[target.'cfg(unix)'.dependencies]\n"
        'kernal-api = { version = "=0.1.14", git = "https://example.invalid" }\n',
        encoding="utf-8",
    )
    errors = verify(member)
    assert any("bypass" in error for error in errors)
    assert any("patch or replace" in error for error in errors)


def test_each_kernel_source_selector_is_rejected_individually(tmp_path: Path) -> None:
    variants = {
        "git": 'kernal-api = { version = "=0.1.14", git = "https://example.invalid" }',
        "path": 'kernal-api = { version = "=0.1.14", path = "kernel" }',
        "registry": 'kernal-api = { version = "=0.1.14", registry = "private-mirror" }',
        "rename": (
            'k = { package = "kernal-api", version = "=0.1.14", git = "https://example.invalid" }'
        ),
        "workspace": 'kernal-api = { version = "=0.1.14", workspace = true }',
    }
    for name, dependency in variants.items():
        manifest = tmp_path / f"{name}.toml"
        manifest.write_text(f"[dependencies]\n{dependency}\n", encoding="utf-8")
        assert any("bypass" in error for error in verify(manifest)), name


def test_target_source_selector_is_rejected_individually(tmp_path: Path) -> None:
    manifest = tmp_path / "Cargo.toml"
    manifest.write_text(
        '[dependencies]\nkernal-api = "=0.1.14"\n'
        "[target.'cfg(unix)'.dependencies]\n"
        'kernal-api = { version = "=0.1.14", registry = "private-mirror" }\n',
        encoding="utf-8",
    )
    assert any("bypass" in error for error in verify(manifest))
