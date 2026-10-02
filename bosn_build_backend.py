"""PEP 517 wrapper that puts Bosn's Rust CLI in every platform wheel.

Maturin's PyO3 bridge builds the extension target but intentionally does not
also package a Cargo binary from the same mixed project.  Build the shared
native CLI first, then let maturin include that artifact in the wheel.
"""

from __future__ import annotations

import json
import os
import stat
from collections.abc import Iterator, Mapping
from contextlib import contextmanager
from dataclasses import dataclass
from functools import wraps
from os import X_OK, access, chmod, environ, pathsep, replace
from os import name as os_name
from pathlib import Path
from re import compile as compile_regex
from shutil import copy2, copyfileobj, rmtree, which
from struct import unpack_from
from subprocess import PIPE, CalledProcessError, Popen, TimeoutExpired, run
from sys import platform
from typing import Any
from uuid import uuid4

import maturin

_ROOT = Path(__file__).resolve().parent
_WHEEL_DATA = _ROOT / "target" / "bosn-wheel-data"
# The CLI is staged into the wheel's ``.data/scripts`` tree so pip installs it
# as the ``bosn`` command on PATH (like soldr ships its own binary), rather than
# behind a Python launcher.  On Linux its OpenSSL sidecars ride in the same
# directory and an ``$ORIGIN`` rpath finds them, so the binary is self-contained
# with no wrapper configuring ``LD_LIBRARY_PATH``.
_WHEEL_NATIVE_DIRECTORY = _WHEEL_DATA / "scripts"
_LINUX_OPENSSL = compile_regex(r"^(lib(?:ssl|crypto)\.so\.\d+) => (\S+)")
_MACHO_64_MAGIC = 0xFEEDFACF
_MAX_ARTIFACT_BYTES = 32 * 1024 * 1024
_MAX_ARTIFACT_RECORD_BYTES = 1024 * 1024


@dataclass(frozen=True)
class _DarwinTarget:
    triple: str
    cputype: int
    deployment_target: str


# A target is an explicit release contract, not an arbitrary Cargo string.  In
# particular, accepting an unknown target would let the backend put a host ELF
# executable into a wheel with a Darwin filename (the false-green #252 exists
# to prevent).  Add a target here together with its static verifier contract.
_DARWIN_TARGETS = {
    "x86_64-apple-darwin": _DarwinTarget("x86_64-apple-darwin", 0x01000007, "10.12"),
    "aarch64-apple-darwin": _DarwinTarget("aarch64-apple-darwin", 0x0100000C, "11.0"),
}


def _wheel_target() -> _DarwinTarget | None:
    requested = environ.get("BOSN_WHEEL_TARGET")
    if not requested:
        return None
    try:
        return _DARWIN_TARGETS[requested]
    except KeyError as error:
        allowed = ", ".join(sorted(_DARWIN_TARGETS))
        raise RuntimeError(
            f"BOSN_WHEEL_TARGET={requested!r} is not a supported Bosn wheel target; "
            f"allowed values: {allowed}"
        ) from error


# The command installed on PATH.  The Cargo bin stays `bosn-native` (the
# workspace already has a distinct `bosn` bin in bosn-service; a second one
# would collide on `target/<profile>/bosn`), so the backend renames it to the
# command name while staging it into the wheel's scripts tree.
def _command_name() -> str:
    return "bosn.exe" if os_name == "nt" else "bosn"


def _assert_target_magic(binary: Path, target: _DarwinTarget | None) -> None:
    """Refuse a wheel whose staged CLI is not the requested native format."""

    if target is None:
        return
    data = binary.read_bytes()
    if len(data) < 16:
        raise RuntimeError(f"cross-built CLI is too short to be Mach-O: {binary}")
    magic, cputype, _cpusubtype, filetype = unpack_from("<IiiI", data)
    if magic != _MACHO_64_MAGIC or cputype != target.cputype or filetype != 2:
        raise RuntimeError(
            f"cross-built CLI does not match {target.triple}: "
            f"magic={magic:#x}, cputype={cputype:#x}, filetype={filetype}; "
            "refusing to stage a host or wrong-architecture executable"
        )


@contextmanager
def _cross_pyo3_environment(target: _DarwinTarget | None) -> Iterator[None]:
    """Make both the pre-built CLI and maturin's cdylib use the same target."""

    if target is None:
        yield
        return
    old_version = environ.get("PYO3_CROSS_PYTHON_VERSION")
    old_deployment = environ.get("MACOSX_DEPLOYMENT_TARGET")
    # Maturin sets this for its own cargo call in some modes, but the CLI is a
    # separate build.  Set it here for both calls and restore the caller's env.
    environ["PYO3_CROSS_PYTHON_VERSION"] = "3.10"
    environ.setdefault("MACOSX_DEPLOYMENT_TARGET", target.deployment_target)
    try:
        yield
    finally:
        if old_version is None:
            environ.pop("PYO3_CROSS_PYTHON_VERSION", None)
        else:
            environ["PYO3_CROSS_PYTHON_VERSION"] = old_version
        if old_deployment is None:
            environ.pop("MACOSX_DEPLOYMENT_TARGET", None)
        else:
            environ["MACOSX_DEPLOYMENT_TARGET"] = old_deployment


@contextmanager
def _linux_rpath_environment(target: _DarwinTarget | None) -> Iterator[None]:
    """Give the Linux CLI an ``$ORIGIN`` rpath so it finds co-located OpenSSL.

    Only the native Linux host build needs this: macOS links system frameworks
    and Windows uses SChannel, neither of which ships a sidecar.  A cross build
    (``target`` set) carries its own soldr-provided linker flags and is left
    alone.  The flag is a final-link argument, so appending it to ``RUSTFLAGS``
    is safe for dependency compiles.
    """

    if target is not None or not platform.startswith("linux"):
        yield
        return
    key = "RUSTFLAGS"
    previous = environ.get(key)
    flag = "-C link-arg=-Wl,-rpath,$ORIGIN"
    environ[key] = f"{previous} {flag}" if previous else flag
    try:
        yield
    finally:
        if previous is None:
            environ.pop(key, None)
        else:
            environ[key] = previous


def _soldr_executable() -> str:
    soldr = which("soldr")
    if soldr is None:
        raise RuntimeError("Bosn source builds require preprovisioned Soldr and Rust toolchain")
    return str(Path(soldr).absolute())


@contextmanager
def _soldr_toolchain_environment() -> Iterator[None]:
    """Use canonical Soldr shims; trusted builder provisions toolchain first."""
    soldr = _soldr_executable()
    shim_dir = _ROOT / "target" / "bosn-wheel-toolchain"
    result = run(
        [soldr, "toolchain", "link", "--shim-dir", str(shim_dir), "--json"],
        cwd=_ROOT,
        check=True,
        capture_output=True,
        text=True,
    )
    document = json.loads(result.stdout)
    tools = document.get("tools") if isinstance(document, dict) else None
    expected = {"cargo", "rustfmt", "clippy-driver", "rustc", "rustdoc"}
    if (
        not isinstance(document, dict)
        or type(document.get("schema_version")) is not int
        or document["schema_version"] != 1
        or document.get("shim_dir") != str(shim_dir)
        or not isinstance(tools, list)
        or len(tools) != len(expected)
    ):
        raise RuntimeError("Soldr toolchain link returned invalid schema")
    seen = set()
    for tool in tools:
        if not isinstance(tool, dict) or not isinstance(tool.get("name"), str):
            raise RuntimeError("Soldr toolchain link returned invalid tool")
        name = tool["name"]
        shim = shim_dir / (name + (".exe" if os_name == "nt" else ""))
        if (
            name not in expected
            or name in seen
            or tool.get("shim_path") != str(shim)
            or type(tool.get("created")) is not bool
            or (tool["created"] is False and tool.get("skip_reason") != "existing-matches")
            or not shim.is_file()
            or not access(shim, X_OK)
        ):
            raise RuntimeError(
                "Soldr toolchain shim missing or differs; refusing untrusted Cargo route"
            )
        seen.add(name)
    updates = {
        "PATH": str(shim_dir) + pathsep + environ.get("PATH", ""),
        "CARGO": str(shim_dir / ("cargo.exe" if os_name == "nt" else "cargo")),
        "MATURIN_NO_INSTALL_RUST": "1",
    }
    previous = {key: environ.get(key) for key in updates}
    environ.update(updates)
    try:
        yield
    finally:
        for key, value in previous.items():
            if value is None:
                environ.pop(key, None)
            else:
                environ[key] = value


def _build_native_cli() -> None:
    target = _wheel_target()
    command = [
        _soldr_executable(),
        "cargo",
        "build",
        "--release",
        "--locked",
        "--package",
        "bosn-python",
        "--bin",
        "bosn-native",
        "--message-format=json-render-diagnostics",
    ]
    if target is not None:
        command.extend(["--target", target.triple])
    with _cross_pyo3_environment(target), _linux_rpath_environment(target):
        # Cargo's machine stream identifies the executable even when Soldr
        # supplies a host target or CARGO_TARGET_DIR redirects build outputs.
        # Compiler diagnostics remain rendered on inherited stderr.
        native_cli = _run_native_build(command)
    _assert_target_magic(native_cli, target)
    rmtree(_WHEEL_DATA, ignore_errors=True)
    destination = _WHEEL_NATIVE_DIRECTORY / _command_name()
    destination.parent.mkdir(parents=True, exist_ok=True)
    copy2(native_cli, destination)
    chmod(destination, native_cli.stat().st_mode)
    _copy_linux_openssl(destination.parent, native_cli, target)


def _run_native_build(command: list[str]) -> Path:
    process = Popen(command, cwd=_ROOT, stdout=PIPE)
    assert process.stdout is not None
    try:
        executable = _cargo_native_executable(process.stdout)
        code = process.wait()
        if code:
            raise CalledProcessError(code, command)
        return executable
    finally:
        process.stdout.close()
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=5)
            except TimeoutExpired:
                process.kill()
                process.wait()


def _cargo_native_executable(artifacts) -> Path:
    manifest = (_ROOT / "crates/bosn-python/Cargo.toml").resolve()
    matches = []
    finished = False
    total_bytes = 0
    for line in iter(lambda: artifacts.readline(_MAX_ARTIFACT_RECORD_BYTES + 1), b""):
        total_bytes += len(line)
        if total_bytes > _MAX_ARTIFACT_BYTES:
            raise RuntimeError("Cargo artifact stream exceeds its size bound")
        if len(line) > _MAX_ARTIFACT_RECORD_BYTES:
            raise RuntimeError("Cargo artifact record exceeds its size bound")
        if not line.lstrip().startswith(b"{"):
            # Proc macros may write ordinary stdout alongside Cargo records.
            print(line.decode("utf-8", errors="replace"), end="")
            continue
        document = json.loads(line)
        if not isinstance(document, dict):
            raise RuntimeError("Invalid Cargo artifact record")
        if document.get("reason") == "build-finished":
            if finished or document.get("success") is not True:
                raise RuntimeError("Cargo build did not finish successfully")
            finished = True
        if document.get("reason") != "compiler-artifact":
            continue
        cargo_target = document.get("target")
        if (
            not isinstance(cargo_target, dict)
            or cargo_target.get("name") != "bosn-native"
            or cargo_target.get("kind") != ["bin"]
            or not isinstance(document.get("manifest_path"), str)
            or Path(document["manifest_path"]).resolve() != manifest
        ):
            continue
        executable = document.get("executable")
        if not isinstance(executable, str) or not executable:
            raise RuntimeError("Cargo omitted the native executable path")
        binary = Path(executable)
        if not binary.is_absolute() or not binary.is_file():
            raise RuntimeError("Cargo native executable is missing or not absolute")
        matches.append(binary)
    if not finished or len(matches) != 1:
        raise RuntimeError("Cargo must report one completed native executable")
    return matches[0]


def _copy_linux_openssl(destination: Path, native_cli: Path, target: _DarwinTarget | None) -> None:
    """Keep the standalone Linux executable independent of host OpenSSL."""

    # Sidecars belong to the output target, never to the Linux builder.  The
    # Darwin branch must remain empty even though this backend runs on Linux.
    if target is not None or not platform.startswith("linux"):
        return
    output = run(["ldd", native_cli], capture_output=True, check=True, text=True).stdout
    copied = set()
    for line in output.splitlines():
        match = _LINUX_OPENSSL.match(line.strip())
        if match is None:
            continue
        name, source = match.groups()
        copy2(source, destination / name)
        copied.add(name)
    if copied != {"libssl.so.3", "libcrypto.so.3"}:
        raise RuntimeError("native CLI was not linked to the expected OpenSSL 3 runtime libraries")


def _wheel_config(config_settings: Mapping[str, Any] | None) -> dict[str, Any]:
    """Request an audited, platform-tagged wheel from maturin's PEP 517 path."""

    settings = dict(config_settings or {})
    arguments = settings.get("maturin.build-args", settings.get("build-args", []))
    if isinstance(arguments, str):
        arguments = arguments.split()
    else:
        arguments = list(arguments)
    if "--compatibility" not in arguments and "--manylinux" not in arguments:
        arguments.extend(["--compatibility", "pypi"])
    target = _wheel_target()
    if target is not None:
        # Maturin receives an explicit target too; otherwise it may compile the
        # extension for the Linux builder while the backend stages a Mach-O CLI.
        if "--target" not in arguments:
            arguments.extend(["--target", target.triple])
        if "--interpreter" not in arguments and "-i" not in arguments:
            arguments.extend(["--interpreter", "python3.11"])
    settings["maturin.build-args"] = arguments
    return settings


@contextmanager
def _wheel_cache_copy_environment() -> Iterator[None]:
    """Wheel repair rewrites ELF bytes: never let it mutate a cache hardlink."""
    previous = environ.get("SOLDR_ZCCACHE_MODE")
    environ["SOLDR_ZCCACHE_MODE"] = "copy"
    try:
        yield
    finally:
        if previous is None:
            environ.pop("SOLDR_ZCCACHE_MODE", None)
        else:
            environ["SOLDR_ZCCACHE_MODE"] = previous


def _detach_repaired_cargo_aliases(target: _DarwinTarget | None) -> None:
    """Maturin restores a pristine primary, but Cargo deps may alias its stage.

    Replace only proven same-inode deps aliases with independent pristine bytes.
    Never reverse guessed SONAMEs or rewrite the repaired wheel artifact/cache.
    """
    if target is not None or not platform.startswith("linux"):
        return
    base = _ROOT / "target"
    staged = base / "maturin" / "libbosn_native.so"
    if not staged.exists():
        return
    if staged.is_symlink() or not staged.is_file():
        raise RuntimeError("unsafe maturin extension artifact")
    aliases = [
        path
        for path in (base / "release" / "deps").glob("libbosn_native*.so")
        if path.is_file() and path.samefile(staged)
    ]
    if not aliases:
        return
    pristine = base / "release" / staged.name
    if pristine.is_symlink() or not pristine.is_file() or pristine.samefile(staged):
        raise RuntimeError("pristine Cargo extension unavailable; use a fresh owned target tree")
    for alias in aliases:
        if alias.is_symlink() or not alias.samefile(staged):
            raise RuntimeError("Cargo extension alias changed during detachment")
        temporary = alias.with_name(f".bosn-wheel-detached-{uuid4()}")
        created = False
        try:
            source_fd = os.open(pristine, os.O_RDONLY | os.O_NOFOLLOW)
            with os.fdopen(source_fd, "rb") as source:
                before = os.fstat(source.fileno())
                if not stat.S_ISREG(before.st_mode):
                    raise RuntimeError("pristine Cargo extension is not regular")
                destination_fd = os.open(
                    temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600
                )
                created = True
                with os.fdopen(destination_fd, "wb") as destination:
                    copyfileobj(source, destination, length=65536)
                    destination.flush()
                    after = os.fstat(source.fileno())
                    if any(
                        getattr(before, key) != getattr(after, key)
                        for key in ("st_dev", "st_ino", "st_size", "st_mtime_ns", "st_ctime_ns")
                    ):
                        raise RuntimeError("pristine Cargo extension changed during copy")
                    os.fchmod(destination.fileno(), stat.S_IMODE(before.st_mode))
                    os.utime(destination.fileno(), ns=(before.st_atime_ns, before.st_mtime_ns))
                    os.fsync(destination.fileno())
            if not alias.samefile(staged):
                raise RuntimeError("Cargo extension alias changed during detachment")
            replace(temporary, alias)
        except BaseException as original:
            if created:
                raise original from RuntimeError(f"retained owned temporary: {temporary}")
            raise


def build_wheel(
    wheel_directory: str,
    config_settings: Mapping[str, Any] | None = None,
    metadata_directory: str | None = None,
) -> str:
    target = _wheel_target()
    with (
        _soldr_toolchain_environment(),
        _cross_pyo3_environment(target),
        _wheel_cache_copy_environment(),
    ):
        _build_native_cli()
        try:
            result = maturin.build_wheel(
                wheel_directory, _wheel_config(config_settings), metadata_directory
            )
        except BaseException as original:
            try:
                _detach_repaired_cargo_aliases(target)
            except Exception as cleanup:
                raise original from cleanup
            raise
        _detach_repaired_cargo_aliases(target)
        return result


def build_editable(
    wheel_directory: str,
    config_settings: Mapping[str, Any] | None = None,
    metadata_directory: str | None = None,
) -> str:
    target = _wheel_target()
    if target is not None:
        raise RuntimeError("cross-target editable wheels are unsupported; build a wheel instead")
    with _soldr_toolchain_environment():
        _build_native_cli()
        return maturin.build_editable(
            wheel_directory, _wheel_config(config_settings), metadata_directory
        )


def _routed_hook(hook):
    @wraps(hook)
    def routed(*args, **kwargs):
        with _soldr_toolchain_environment():
            return hook(*args, **kwargs)

    return routed


get_requires_for_build_wheel = _routed_hook(maturin.get_requires_for_build_wheel)
get_requires_for_build_editable = _routed_hook(maturin.get_requires_for_build_editable)
get_requires_for_build_sdist = _routed_hook(maturin.get_requires_for_build_sdist)
prepare_metadata_for_build_wheel = _routed_hook(maturin.prepare_metadata_for_build_wheel)
prepare_metadata_for_build_editable = _routed_hook(maturin.prepare_metadata_for_build_editable)
build_sdist = _routed_hook(maturin.build_sdist)
