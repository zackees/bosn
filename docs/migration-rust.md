# Rust migration status

Bosn is being migrated under [issue #153](https://github.com/zackees/bosn/issues/153).
The application domain, daemon protocol, SQLite state registry, Docker engine
seam, native CLI, PyO3 binding, MCP server, and URL setup path are Rust code.
`kernal-api` is the OS boundary and is pinned by reviewed revision until its
required publishing workflow is available.

The authoritative implementation/fixture/consumer inventory is the
[issue #153 coverage matrix](issue153-coverage-matrix.md). It explicitly
separates implemented surfaces from external release blockers and opt-in live
acceptance gaps.

## Python lifecycle retirement

The legacy Python implementation was removed in the Phase 7 consumer-migration
slice. The following former production modules are no longer present in the
wheel or source tree:

```text
accounting autostart cli clock compose config converge daemon docker_cli engine
frontdoor gc gitstate guest ipc jobs labels legacy manifest migration_lock
options paths recovery registry resources retention shims
```

They formerly implemented Python-owned Docker process execution, lifecycle
decisions, daemon state, and `sqlite3` registry access. Keeping them importable
would leave two writers and two Docker control planes, so no compatibility shim
is provided. The old Python-only test suite, compose/front-door documentation,
and historical Soldr manifest have been removed with those APIs.

The retained `bosn` package is deliberately small:

```text
bosn/__init__.py    native API re-exports
bosn/_native.*      Rust PyO3 extension (wheel artifact)
```

The `bosn` command is the native Rust binary itself, staged into the wheel's
`.data/scripts` tree by the PEP 517 backend and installed on PATH — there is no
Python launcher or console-script entry point. The backend may run Cargo while
building a wheel. Neither the extension nor the CLI is a Bosn lifecycle
implementation in Python, and the package has no runtime Python dependencies.

## Native operation boundary

Only the Rust daemon creates, starts, executes in, retires, or garbage-collects
Docker resources and only its registry actor writes SQLite. CLI, Python, and
MCP calls are authenticated typed requests to that daemon. Local parsing/planning
is inert and is revalidated by the daemon before a mutation.

The supported native surface includes bounded status/doctor and job observation,
setup plan/prepare/ensure/task/app-task, manifest ensure/converge/app-task,
managed named volumes, workspace bind mounts/workdirs, tmpfs, Dockerfile and
pinned-image sources, constrained macOS guest lifecycle, URL configuration, and
Hermes-compatible stdio MCP. See [rust-manifest-runtime.md](rust-manifest-runtime.md)
and [rust-registry.md](rust-registry.md) for exact support/refusal matrices.

## Deliberate remaining release work

Issue #153 is not complete merely because the Python source is gone. Remaining
work includes final feature-parity audit for all legacy lifecycle cases,
documented offline import/cutover of a real Python-v4 registry, verified
downstream consumer migrations (Soldr, clud, and kernal-api), exact published
`kernal-api` and Python artifacts, and supported-platform wheel/runtime/GitHub
Actions evidence. Existing Docker state must remain protected during that
cutover; removing compatibility code does not authorize deleting data.

macOS artifact builds no longer consume hosted Mac capacity: the two Darwin
wheel targets are cross-built on Linux with Soldr's pinned LLVM 21.1.5 and
Apple SDK 14.5, then checked as Mach-O artifacts on Linux.  Release is gated
on those jobs, `ci/verify_release_dependencies.py` proving exact published
`kernal-api 0.1.0`, and closure of the downstream-consumer items above.  A
real x86_64 macOS guest execution lane is advisory follow-up work; arm64 has
no Linux-hosted execution mechanism.  The package native module is
`abi3-py310`: all platform wheels carry `cp310-abi3` tags and package metadata
permits CPython 3.10+, rather than releasing cp311-only Darwin wheels.

## Verification

The retirement slice is checked by:

```bash
uv run pytest tests/test_python_native_boundary.py tests/test_native.py -q
uv run ruff check src tests ci
uv run pyright
PYTHON_BIN="$PWD/.venv/bin/python"
PYTHON_LIB="$("$PYTHON_BIN" -c 'import sysconfig; print(sysconfig.get_config_var("LIBDIR"))')"
PYO3_PYTHON="$PYTHON_BIN" LD_LIBRARY_PATH="$PYTHON_LIB${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" \
  soldr cargo test -j1 -p bosn --lib --locked --no-default-features --features embedded-python-tests
```

The wheel proof builds a platform wheel, then installs it into a clean virtual
environment outside the checkout. It verifies the host-specific extension and
native executable filenames, imports the extension, and starts/stops the
bundled daemon while exercising `daemon status` and `doctor` with Docker
deliberately unreachable:

```bash
uv run pytest tests/test_native_wheel.py -q
# Or, after `uv build --wheel`:
python ci/verify_installed_wheel.py 'dist/bosn-*.whl'
```

GitHub Actions runs the installed-wheel proof on Python 3.11 for Linux and
Windows.  Both Darwin wheels are instead built on Linux by Soldr and checked
with `ci/verify_cross_wheel.py`: `cp310-abi3` tags/version, Mach-O architecture/filetype,
deployment floor, system dylibs, native CLI version string, and absence of
ELF/OpenSSL contamination.  Those native-wheel lanes do not need Docker; the
separate Linux lane remains the Docker integration test.

The Linux Rust CI lane also validates the reviewed `kernal-api` revision and
the locked Cargo resolution before running every ordinary Rust test. The PyO3
crate is tested with its explicit embedded-Python test feature, since the wheel
feature correctly leaves CPython symbols for the installed interpreter:

```bash
python ci/verify_kernel_boundary.py
cargo metadata --locked --format-version 1 --no-deps
cargo test --workspace --exclude bosn --locked
PYO3_PYTHON="$(command -v python)" \
  cargo test -p bosn --lib --locked --no-default-features --features embedded-python-tests
```

Ignored Docker and Hermes acceptance tests remain opt-in; the Rust CI lane does
not silently enable their engine, image, or pinned-agent prerequisites.

## Performance comparison baseline

The durable Phase-0 historical comparison record is
[`performance-baseline.json`](performance-baseline.json). It points to the exact
pre-migration revision and retains only the values that revision actually
reported. In particular, its `raw_samples_ms` fields are `null`: the historical
commit recorded medians and ranges, not the individual samples. That absence is
intentional rather than reconstructed data.

To collect the equivalent native surface from a locally built binary:

```bash
soldr cargo build -p bosn-service --bin bosn --locked
python ci/native_performance_baseline.py --binary target/debug/bosn --samples 7
```

The checked-in artifact also includes one explicitly labelled
`current_native_local_run`. It preserves raw native samples and the separate
warm-cache build and wheel-package wall-clock observations that were available
on its recorded host. It is useful as a rerunnable starting point, but it must
not be read as a Rust-versus-Python speedup: its host/toolchain/cache state is
not the historical Python run's state, the package timing is a single sample,
and neither run is a release threshold. To replace or extend it, collect a
fresh native binary and write the complete collector JSON, revision, host
summary, cache state, and any package timing into
`docs/performance-baseline.json`; retain unavailable metrics as explicit
`unsupported` values rather than fabricating comparisons.

For a separately timed package observation, build to a new empty output
directory after the native collector has run:

```bash
/usr/bin/time -f 'wheel_elapsed_seconds=%e' \
  uv build --wheel --out-dir /tmp/bosn-wheel-baseline
```

The result is only comparable to another run that records the same dependency,
target, and cache conditions. A clean package/build study should use an
isolated disposable target and dependency cache, and record those locations as
cache state rather than treating it as a continuation of the historical data.

The output is a stable JSON document containing native CLI startup, idle daemon
RSS (or an explicit platform/unsupported record), daemon status latency, and an
explicit setup-ensure-reuse record. It contains no command lines, paths,
environment values, or child output. Measurements are wall-clock samples on a
shared host; caches are not cleared, so compare like-for-like runs rather than
treating them as a release threshold.

The real Docker reuse probe is deliberately opt-in:

```bash
python ci/native_performance_baseline.py --binary target/debug/bosn --docker --samples 7
```

It uses the pinned Alpine image already documented by the native live-Docker
test, creates a unique disposable setup application, measures completed reuse
jobs rather than mere submission, and removes only the exact container after
all of its expected ownership labels match. It never prunes Docker caches or
uses a label selector to clean up. Without `--docker`, the same JSON schema
emits `setup_ensure_reuse_latency` as `unsupported` with
`requires_docker_opt_in`; this makes non-Docker local comparison safe and
unambiguous.
