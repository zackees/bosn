# Rust migration status

Bosn is being migrated under [issue #153](https://github.com/zackees/bosn/issues/153).
The application domain, daemon protocol, SQLite state registry, Docker engine
seam, native CLI, PyO3 binding, MCP server, and URL setup path are Rust code.
`kernal-api` is the OS boundary and is pinned by reviewed revision until its
required publishing workflow is available.

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
bosn/__main__.py    native CLI entry point
bosn/native_cli.py  package-local executable launcher
bosn/_native.*      Rust PyO3 extension (wheel artifact)
```

`native_cli.py` may only execute the version-matched packaged Rust binary. The
PEP 517 backend may run Cargo while building a wheel. Neither is a Bosn
lifecycle implementation. The package has no runtime Python dependencies.

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

GitHub Actions runs that proof on Python 3.11 for Linux, macOS, and Windows.
Those native-wheel lanes do not need Docker; the separate Linux lane remains
the Docker integration test.

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
