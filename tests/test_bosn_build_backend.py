import importlib.util
import io
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
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    monkeypatch.setitem(sys.modules, spec.name, module)
    spec.loader.exec_module(module)
    assert callable(module._soldr_toolchain_environment)


@pytest.fixture
def backend(monkeypatch, tmp_path):
    monkeypatch.delenv("CARGO_TARGET_DIR", raising=False)
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
        target = Path(os.environ.get("CARGO_TARGET_DIR", str(tmp_path / "target")))
        assert os.environ["CARGO"].startswith(str(target / "bosn-wheel-toolchain"))
        assert os.environ["MATURIN_NO_INSTALL_RUST"] == "1"
        return "fixture"

    for name in names:
        setattr(fake, name, original_hook)
    monkeypatch.setitem(sys.modules, "maturin", fake)
    spec = importlib.util.spec_from_file_location(
        "backend_fixture", Path(__file__).parents[1] / "bosn_build_backend.py"
    )
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    monkeypatch.setitem(sys.modules, spec.name, module)
    spec.loader.exec_module(module)
    monkeypatch.setattr(module, "_ROOT", tmp_path)
    monkeypatch.setattr(module, "which", lambda name: "/fixture/soldr")
    calls = []

    def linked(command, **kwargs):
        calls.append(command)
        directory = Path(command[command.index("--shim-dir") + 1])
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
    monkeypatch.setattr(module, "_assert_target_magic", lambda *args: None)
    monkeypatch.setattr(module, "rmtree", lambda *args, **kwargs: None)
    monkeypatch.setattr(module, "copy2", lambda *args: None)
    monkeypatch.setattr(module, "chmod", lambda *args: None)
    binary = module._ROOT / "target" / target.triple / "release/bosn-native"
    binary.parent.mkdir(parents=True)
    binary.write_bytes(b"fixture")

    def compiled(argv, **kwargs):
        calls.append(argv)
        return module._cargo_native_executable(io.BytesIO(native_artifact_stream(module, binary)))

    monkeypatch.setattr(module, "_run_native_build", compiled)
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
        "--message-format=json-render-diagnostics",
        "--target",
        target.triple,
    ]
    assert "PYO3_CROSS_PYTHON_VERSION" not in os.environ


def native_artifact_stream(module, binary, *, manifest=None, success=True):
    artifact = {
        "reason": "compiler-artifact",
        "manifest_path": str(manifest or module._ROOT / "crates/bosn-python/Cargo.toml"),
        "target": {"name": "bosn-native", "kind": ["bin"]},
        "executable": str(binary),
    }
    finished = {"reason": "build-finished", "success": success}
    return (json.dumps(artifact) + "\n" + json.dumps(finished) + "\n").encode()


@pytest.mark.parametrize("layout", ["x86_64-pc-windows-msvc", "custom-target-dir"])
def test_native_cli_stages_the_reported_executable(backend, monkeypatch, layout):
    module, _ = backend
    binary = module._ROOT / layout / "release/bosn-native.exe"
    binary.parent.mkdir(parents=True)
    binary.write_bytes(b"reported-binary")
    decoy = module._ROOT / "target/release/bosn-native"
    decoy.parent.mkdir(parents=True)
    decoy.write_bytes(b"stale-decoy")
    monkeypatch.setattr(module, "_WHEEL_DATA", module._ROOT / "wheel-data")
    monkeypatch.setattr(module, "_WHEEL_NATIVE_DIRECTORY", module._ROOT / "stage")
    monkeypatch.setattr(module, "_copy_linux_openssl", lambda *args: None)
    monkeypatch.delenv("BOSN_WHEEL_TARGET", raising=False)

    def compiled(_argv, **kwargs):
        return module._cargo_native_executable(io.BytesIO(native_artifact_stream(module, binary)))

    monkeypatch.setattr(module, "_run_native_build", compiled)
    module._build_native_cli()
    assert (module._WHEEL_NATIVE_DIRECTORY / module._command_name()).read_bytes() == (
        b"reported-binary"
    )


@pytest.mark.parametrize("invalid", ["duplicate", "missing", "foreign", "unfinished", "failed"])
def test_unproven_native_executable_refuses(backend, invalid):
    module, _ = backend
    binary = module._ROOT / "bosn-native"
    binary.write_bytes(b"fixture")
    stream = native_artifact_stream(module, binary, success=invalid != "failed")
    if invalid == "duplicate":
        stream = stream.splitlines(keepends=True)[0] + stream
    elif invalid == "missing":
        binary.rename(module._ROOT / "gone")
    elif invalid == "foreign":
        stream = native_artifact_stream(module, binary, manifest=module._ROOT / "other.toml")
    elif invalid == "unfinished":
        stream = stream.splitlines(keepends=True)[0]
    with pytest.raises(RuntimeError):
        module._cargo_native_executable(io.BytesIO(stream))


def test_artifact_stream_overflow_terminates_and_reaps_live_producer(backend, monkeypatch):
    module, _ = backend
    monkeypatch.setattr(module, "_MAX_ARTIFACT_BYTES", 1024)
    processes = []

    def producer(*args, **kwargs):
        process = subprocess.Popen(*args, **kwargs)
        processes.append(process)
        return process

    monkeypatch.setattr(module, "Popen", producer)
    command = [
        sys.executable,
        "-c",
        "import sys,time; sys.stdout.write('x'*2048+'\\n'); sys.stdout.flush(); time.sleep(30)",
    ]
    with pytest.raises(RuntimeError, match="stream exceeds"):
        module._run_native_build(command)
    assert len(processes) == 1 and processes[0].poll() is not None


def test_actual_fake_frontdoor_executable(backend):
    module, _ = backend
    with module._soldr_toolchain_environment():
        # Executable is the fixture shim, never a real Rust tool.
        result = subprocess.run(
            [os.environ["CARGO"], "metadata"], check=True, capture_output=True, text=True
        )
        assert result.stdout == "soldr-routed:metadata"


@pytest.mark.parametrize("redirect_target", [None, "absolute", "relative"])
def test_repeated_wheel_does_not_reuse_repaired_cargo_alias(backend, monkeypatch, redirect_target):
    import shutil

    module, _ = backend
    monkeypatch.setattr(module, "_build_native_cli", lambda: None)
    root = module._ROOT / "target"
    if redirect_target:
        root = module._ROOT / "external-target"
        monkeypatch.setenv(
            "CARGO_TARGET_DIR", str(root) if redirect_target == "absolute" else "external-target"
        )
    deps = root / "release/deps/libbosn_native.so"
    primary = root / "release/libbosn_native.so"
    staged = root / "maturin/libbosn_native.so"
    deps.parent.mkdir(parents=True)
    staged.parent.mkdir(parents=True)
    original = b"ELF needs libssl.so.3 libcrypto.so.3"
    deps.write_bytes(original)

    def repaired(*args):
        assert deps.read_bytes() == original, "second build reused patched dependencies"
        if not primary.exists():
            os.link(deps, primary)
        os.replace(primary, staged)
        shutil.copy2(staged, primary)
        staged.write_bytes(b"ELF needs libssl-HASH.so.3 libcrypto-HASH.so.3")
        return "bosn.whl"

    monkeypatch.setattr(module.maturin, "build_wheel", repaired)
    assert module.build_wheel("wheel") == "bosn.whl"
    assert module.build_wheel("wheel") == "bosn.whl"


def test_maturin_failure_preserved_without_pristine_primary(backend, monkeypatch):
    module, _ = backend
    monkeypatch.setattr(module, "_build_native_cli", lambda: None)
    root = module._ROOT / "target"
    staged = root / "maturin/libbosn_native.so"
    alias = root / "release/deps/libbosn_native.so"
    staged.parent.mkdir(parents=True)
    alias.parent.mkdir(parents=True)
    staged.write_bytes(b"patched")
    os.link(staged, alias)
    monkeypatch.setenv("SOLDR_ZCCACHE_MODE", "reflink")

    def fail(*args):
        assert os.environ["SOLDR_ZCCACHE_MODE"] == "copy"
        raise ValueError("original repair error")

    monkeypatch.setattr(module.maturin, "build_wheel", fail)
    with pytest.raises(ValueError, match="original repair error") as failure:
        module.build_wheel("wheel")
    assert "pristine Cargo extension unavailable" in str(failure.value.__cause__)
    assert os.environ["SOLDR_ZCCACHE_MODE"] == "reflink"
    assert staged.samefile(alias) and alias.read_bytes() == b"patched"


def test_detachment_rejects_missing_pristine_and_preserves_independent_files(backend):
    module, _ = backend
    root = module._ROOT / "target"
    staged = root / "maturin/libbosn_native.so"
    alias = root / "release/deps/libbosn_native.so"
    staged.parent.mkdir(parents=True)
    alias.parent.mkdir(parents=True)
    staged.write_bytes(b"repaired")
    alias.write_bytes(b"independent")
    module._detach_repaired_cargo_aliases(None)
    assert alias.read_bytes() == b"independent"
    second = alias.parent / "libbosn_native-actualalias.so"
    os.link(staged, second)
    with pytest.raises(RuntimeError, match="pristine Cargo extension unavailable"):
        module._detach_repaired_cargo_aliases(None)
    assert second.samefile(staged)


def test_failure_after_copyback_detaches_alias_and_restores_env(backend, monkeypatch):
    import shutil

    module, _ = backend
    monkeypatch.setattr(module, "_build_native_cli", lambda: None)
    root = module._ROOT / "target"
    alias = root / "release/deps/libbosn_native.so"
    primary = root / "release/libbosn_native.so"
    staged = root / "maturin/libbosn_native.so"
    alias.parent.mkdir(parents=True)
    staged.parent.mkdir(parents=True)
    alias.write_bytes(b"clean")
    os.link(alias, primary)

    def fail(*args):
        os.replace(primary, staged)
        shutil.copy2(staged, primary)
        staged.write_bytes(b"repaired")
        raise ValueError("repair failed after copyback")

    monkeypatch.setattr(module.maturin, "build_wheel", fail)
    monkeypatch.delenv("SOLDR_ZCCACHE_MODE", raising=False)
    with pytest.raises(ValueError, match="repair failed after copyback"):
        module.build_wheel("wheel")
    assert alias.read_bytes() == b"clean"
    assert not alias.samefile(staged)
    assert "SOLDR_ZCCACHE_MODE" not in os.environ


def test_detachment_existing_temporary_is_never_overwritten(backend, monkeypatch):
    module, _ = backend
    root = module._ROOT / "target"
    staged = root / "maturin/libbosn_native.so"
    alias = root / "release/deps/libbosn_native.so"
    primary = root / "release/libbosn_native.so"
    staged.parent.mkdir(parents=True)
    alias.parent.mkdir(parents=True)
    staged.write_bytes(b"repaired")
    os.link(staged, alias)
    primary.write_bytes(b"pristine")
    monkeypatch.setattr(module, "uuid4", lambda: "fixed-test-name")
    occupied = alias.with_name(".bosn-wheel-detached-fixed-test-name")
    occupied.write_bytes(b"foreign existing bytes")
    with pytest.raises(FileExistsError):
        module._detach_repaired_cargo_aliases(None)
    assert occupied.read_bytes() == b"foreign existing bytes"
    assert alias.samefile(staged)


def test_copy_failure_preserves_original_and_reports_owned_temporary(backend, monkeypatch):
    module, _ = backend
    root = module._ROOT / "target"
    staged = root / "maturin/libbosn_native.so"
    alias = root / "release/deps/libbosn_native.so"
    primary = root / "release/libbosn_native.so"
    staged.parent.mkdir(parents=True)
    alias.parent.mkdir(parents=True)
    staged.write_bytes(b"repaired")
    os.link(staged, alias)
    primary.write_bytes(b"pristine")

    def fail(*args, **kwargs):
        raise OSError("original copy failure")

    monkeypatch.setattr(module, "copyfileobj", fail)
    with pytest.raises(OSError, match="original copy failure") as failure:
        module._detach_repaired_cargo_aliases(None)
    assert "retained owned temporary" in str(failure.value.__cause__)
    assert alias.samefile(staged)
    assert len(list(alias.parent.glob(".bosn-wheel-detached-*"))) == 1
