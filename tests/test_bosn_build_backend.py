import importlib.util
import json
import os
import subprocess
import sys
from pathlib import Path
from types import ModuleType, SimpleNamespace

import pytest


def test_soldr_environment_exists(monkeypatch):
    monkeypatch.setitem(sys.modules, "maturin", ModuleType("maturin"))
    for name in (
        "get_requires_for_build_wheel",
        "get_requires_for_build_editable",
        "get_requires_for_build_sdist",
        "prepare_metadata_for_build_wheel",
        "prepare_metadata_for_build_editable",
        "build_sdist",
    ):
        setattr(sys.modules["maturin"], name, lambda *args: None)
    spec = importlib.util.spec_from_file_location(
        "bosn_build_backend_test", Path(__file__).parents[1] / "bosn_build_backend.py"
    )
    module = importlib.util.module_from_spec(spec)
    monkeypatch.setitem(sys.modules, spec.name, module)
    spec.loader.exec_module(module)
    assert callable(module._soldr_toolchain_environment)


@pytest.fixture
def backend(monkeypatch, tmp_path):
    fake = ModuleType("maturin")
    names = (
        "build_wheel",
        "build_editable",
        "get_requires_for_build_wheel",
        "get_requires_for_build_editable",
        "get_requires_for_build_sdist",
        "prepare_metadata_for_build_wheel",
        "prepare_metadata_for_build_editable",
        "build_sdist",
    )

    def original_hook(*args, **kwargs):
        assert os.environ["CARGO"].startswith(str(tmp_path / "target/bosn-wheel-toolchain"))
        assert os.environ["MATURIN_NO_INSTALL_RUST"] == "1"
        return "fixture"

    for name in names:
        setattr(fake, name, original_hook)
    monkeypatch.setitem(sys.modules, "maturin", fake)
    spec = importlib.util.spec_from_file_location(
        "backend_fixture", Path(__file__).parents[1] / "bosn_build_backend.py"
    )
    module = importlib.util.module_from_spec(spec)
    monkeypatch.setitem(sys.modules, spec.name, module)
    spec.loader.exec_module(module)
    monkeypatch.setattr(module, "_ROOT", tmp_path)
    monkeypatch.setattr(module, "which", lambda name: "/fixture/soldr")
    calls = []

    def linked(command, **kwargs):
        calls.append(command)
        directory = tmp_path / "target/bosn-wheel-toolchain"
        directory.mkdir(parents=True, exist_ok=True)
        tools = []
        for name in ("cargo", "rustfmt", "clippy-driver", "rustc", "rustdoc"):
            path = directory / (name + (".exe" if module.os_name == "nt" else ""))
            path.write_text('#!/bin/sh\nprintf "soldr-routed:%s" "$1"\n')
            path.chmod(0o755)
            tools.append(dict(name=name, shim_path=str(path), created=True))
        return SimpleNamespace(
            stdout=json.dumps(dict(schema_version=1, shim_dir=str(directory), tools=tools))
        )

    monkeypatch.setattr(module, "run", linked)
    return module, calls


def test_context_routes_and_restores_failure(backend, monkeypatch):
    module, calls = backend
    monkeypatch.setenv("PATH", "caller-path")
    monkeypatch.setenv("CARGO", "caller-cargo")
    monkeypatch.setenv("MATURIN_NO_INSTALL_RUST", "caller-install")
    monkeypatch.setenv("CARGO_BUILD_JOBS", "7")
    with pytest.raises(RuntimeError, match="fixture failure"):
        with module._soldr_toolchain_environment():
            assert os.environ["PATH"].split(os.pathsep)[0] == str(
                module._ROOT / "target/bosn-wheel-toolchain"
            )
            assert os.environ["CARGO"].endswith("cargo")
            assert os.environ["MATURIN_NO_INSTALL_RUST"] == "1"
            assert os.environ["CARGO_BUILD_JOBS"] == "7"
            raise RuntimeError("fixture failure")
    assert os.environ["PATH"] == "caller-path"
    assert os.environ["CARGO"] == "caller-cargo"
    assert os.environ["MATURIN_NO_INSTALL_RUST"] == "caller-install"
    assert calls[0][:3] == ["/fixture/soldr", "toolchain", "link"]
    assert "--force" not in calls[0]


@pytest.mark.parametrize("mutation", ["schema", "differs", "duplicate", "missing"])
def test_bad_shim_proof_refuses(backend, monkeypatch, mutation):
    module, _ = backend
    original = module.run

    def invalid(*args, **kwargs):
        result = original(*args, **kwargs)
        doc = json.loads(result.stdout)
        if mutation == "schema":
            doc["schema_version"] = True
        elif mutation == "differs":
            doc["tools"][0].update(created=False, skip_reason="existing-differs")
        elif mutation == "duplicate":
            doc["tools"][1] = doc["tools"][0]
        else:
            doc["tools"].pop()
        return SimpleNamespace(stdout=json.dumps(doc))

    monkeypatch.setattr(module, "run", invalid)
    before = dict(os.environ)
    with pytest.raises(RuntimeError):
        with module._soldr_toolchain_environment():
            raise AssertionError("untrusted route")
    assert dict(os.environ) == before


def test_missing_soldr_explicit(backend, monkeypatch):
    module, _ = backend
    monkeypatch.setattr(module, "which", lambda *args: None)
    with pytest.raises(RuntimeError, match="preprovisioned"):
        with module._soldr_toolchain_environment():
            pass


@pytest.mark.parametrize(
    "hook",
    [
        "build_wheel",
        "build_editable",
        "prepare_metadata_for_build_wheel",
        "prepare_metadata_for_build_editable",
        "build_sdist",
        "get_requires_for_build_wheel",
        "get_requires_for_build_editable",
        "get_requires_for_build_sdist",
    ],
)
def test_maturin_hooks_have_path_frontdoor(backend, monkeypatch, hook):
    module, _ = backend
    monkeypatch.setattr(module, "_build_native_cli", lambda: None)

    # Hook wrappers were bound when module loaded; exercise their environment
    # through a replacement scoped hook for metadata and dependency methods.
    def delegate(*args, **kwargs):
        assert os.environ["CARGO"].startswith(str(module._ROOT / "target/bosn-wheel-toolchain"))
        assert os.environ["MATURIN_NO_INSTALL_RUST"] == "1"
        return "delegated"

    if hook in ("build_wheel", "build_editable"):
        monkeypatch.setattr(module.maturin, hook, delegate)
        assert getattr(module, hook)("output") == "delegated"
    else:
        assert getattr(module, hook)() == "fixture"


def test_native_cli_explicit_soldr_preserves_flags(backend, monkeypatch):
    module, calls = backend
    target = module._DARWIN_TARGETS["aarch64-apple-darwin"]
    monkeypatch.setenv("BOSN_WHEEL_TARGET", target.triple)
    monkeypatch.setattr(module, "run", lambda argv, **kwargs: calls.append(argv))
    monkeypatch.setattr(module, "_assert_target_magic", lambda *args: None)
    monkeypatch.setattr(module, "rmtree", lambda *args, **kwargs: None)
    monkeypatch.setattr(module, "copy2", lambda *args: None)
    monkeypatch.setattr(module, "chmod", lambda *args: None)
    binary = module._native_cli(target)
    binary.parent.mkdir(parents=True)
    binary.write_bytes(b"fixture")
    monkeypatch.setattr(module, "_WHEEL_NATIVE_DIRECTORY", module._ROOT / "stage")
    module._build_native_cli()
    assert calls[-1] == [
        "/fixture/soldr",
        "cargo",
        "build",
        "--release",
        "--locked",
        "--package",
        "bosn-python",
        "--bin",
        "bosn-native",
        "--target",
        target.triple,
    ]
    assert "PYO3_CROSS_PYTHON_VERSION" not in os.environ


def test_actual_fake_frontdoor_executable(backend):
    module, _ = backend
    with module._soldr_toolchain_environment():
        # Executable is the fixture shim, never a real Rust tool.
        result = subprocess.run(
            [os.environ["CARGO"], "metadata"], check=True, capture_output=True, text=True
        )
        assert result.stdout == "soldr-routed:metadata"
