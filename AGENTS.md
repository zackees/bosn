# Agent notes for Bosn

Settled decisions, recorded so they are not re-litigated. Change them only with an
explicit new decision, not by inference.

## Python packaging

- **The extension is `abi3-py310`. Always. Only.** `crates/bosn-python/Cargo.toml`
  declares `pyo3/abi3-py310`; every platform wheel is tagged `cp310-abi3-*` and ships
  `bosn/_native.abi3.so` (`.pyd` on Windows). `requires-python = ">=3.10"`.
  - Do **not** build interpreter-specific wheels (`cp311-cp311`, etc.). One abi3 wheel
    per platform covers CPython 3.10+. This is the fleet-wide policy (soldr, fbuild,
    zccache, running-process all use `abi3-py310`); it is also what lets the macOS
    wheels cross-compile on Linux with no target-side CPython.
  - `ci/verify_installed_wheel.py` and `ci/verify_cross_wheel.py` assert the
    `cp310-abi3` tag and the `_native.abi3.so` / `.pyd` extension name. Keep them in
    lockstep with any change here.

- **Build backend: `bosn_build_backend.py` stays; the bin cannot ride on maturin.**
  Bosn's wheel needs **both** a PyO3 abi3 extension (`bosn._native`, used by
  `import bosn`) **and** the `bosn-native` CLI. maturin builds one artifact kind per
  wheel — a pyo3 extension **or** a `bindings="bin"` binary — and does **not** ship a
  bin alongside a pyo3 extension (verified by building it: bin un-disabled → wheel
  `scripts: []`). soldr is **not** a counterexample: soldr's wheel is `py3-none-*`,
  binary-only, with no extension, so it can be pure `build-backend = "soldr"`. Bosn
  cannot. So a custom PEP 517 backend that builds the CLI and stages it, then lets
  maturin build the extension, is the correct shape — do not "simplify" it away.
  - **The `bosn` command IS the native binary** (#262, done): the backend stages the
    CLI into the wheel's `.data/scripts/` (installed on PATH as `bosn`), with an
    `$ORIGIN` rpath so it finds its co-located OpenSSL sidecars — there is no
    `native_cli.py` launcher and no `[project.scripts]`. The Cargo bin stays
    `bosn-native` (bosn-service already owns a `bosn` bin; a second would collide on
    `target/<profile>/bosn`); the backend renames it to `bosn` while staging.
  - Remaining cruft is tracked in **#265**: OpenSSL is bundled twice (`bosn.libs/` by
    auditwheel for the extension, plus the hand-copied `.data/scripts/` sidecars for the
    CLI); collapse via static/rustls or an rpath into `bosn.libs/`. Dropping
    `bosn_build_backend.py` entirely still waits on **soldr#3239** (native aux-bin
    staging).

## Releasing

- **Push a tag `vX.Y.Z` on main**; `.github/workflows/auto-release.yml` does the rest. Before
  tagging, bump the version in all four places `ci/verify_release.py` checks
  (`pyproject.toml`, `crates/bosn-python/Cargo.toml`, `src/bosn/__init__.py`, and the
  `native_version()` literal in `crates/bosn-python/src/lib.rs`); `tests/test_version_sync.py`
  catches a missed one before the tag does.
- **Rehearse first**: `gh workflow run auto-release.yml -f tag=vX.Y.Z` is a dry run by default,
  and works before the tag exists (it rehearses main). It builds and verifies all four
  wheels and publishes nothing. Pass `-f dry_run=false` to publish an existing tag, e.g. to
  recover a failed run; every publish step is idempotent.
- **A release is exactly four `cp310-abi3` wheels**: Linux x86_64 (`manylinux_2_39`, so glibc
  ≥ 2.39), Windows x86_64, and both Darwin targets. No sdist: building Bosn from source
  needs the Rust toolchain and the staging backend, so an unsupported platform should get
  "no matching distribution" rather than a failed compile.
- **PyPI uses trusted publishing** (OIDC), like kernal-api's crates.io release: no PyPI
  token is stored. It needs `vars.PUBLISH_PYPI == 'true'` and the pypi.org trusted
  publisher for `bosn`: repository `zackees/bosn`, workflow `auto-release.yml`,
  environment `pypi`. PyPI matches both names exactly: renaming the workflow file or the
  job's environment breaks publishing. Without the variable a tag still produces the
  GitHub release, and the run summary says PyPI was skipped.
- The build steps are **copies** of `ci.yml`'s wheel lanes, not a shared `workflow_call`:
  branch protection requires those job names verbatim. Change both together.

### crates.io: one amalgamated `bosn` crate

- **Only `bosn` is published.** `crates/bosn` is a facade that, in the workspace,
  re-exports the internal crates (`bosn::core`, `engine`, `generation`, `registry`,
  `setup`, `service`). `ci/publish_amalgamate.py` (zackees/zccache's approach) rewrites it
  into one self-contained crate: each internal crate's `src/` becomes a module, paths are
  rewritten, the `bosn` CLI binary is carried across, test fixtures reached by
  `include_str!` are relocated, and internal dependencies are stripped. Every internal crate
  and `bosn-python` (the PyO3 extension, PyPI only) is `publish = false`.
- **The script rewrites in place, so never run it on the working copy.** Publish by hand
  from a disposable worktree:

  ```bash
  git worktree add ../bosn-extern/bosn-publish vX.Y.Z
  cd ../bosn-extern/bosn-publish
  python ci/publish_amalgamate.py --root .
  cargo package -p bosn --allow-dirty          # builds the .crate in isolation
  cargo publish -p bosn --allow-dirty          # needs `cargo login`
  ```

- **Keep the facade's `[dependencies]` in step** with the internal crates' external
  dependencies: the amalgamated crate builds from crates.io alone, and the script refuses
  a copy that still names an internal crate.

## macOS (issue #252)

- **No hosted macOS runners.** `ci/lint_no_macos_runners.py` fails CI on any `macos-*`
  `runs-on`/matrix label. Both `x86_64-apple-darwin` and `aarch64-apple-darwin` wheels
  build on `ubuntu-latest` through `zackees/setup-soldr` (pinned SHA) +
  `soldr prepare --target …`, using Soldr's LLVM 21.1.5 and managed Apple SDK 14.5 —
  never Xcode, zig, or osxcross.
- **Linux verifies macOS wheels statically; it never executes them.**
  `ci/verify_cross_wheel.py` checks the Mach-O extension and bundled CLI (arch,
  filetype, min-OS ≤ tag floor, allowed system dylibs, no ELF/OpenSSL contamination),
  tag, and version alignment. The floors are 10.12 (x86_64) and 11.0 (arm64).
- **Executing the macOS wheel is the advisory `macos-x64-execute.yml` lane** — a macOS
  Recovery guest (`zackees/docker-mac-x64`, OSX-KVM on `ubuntu-latest`, no macOS
  runner). It is nightly + manual, never a required check or release gate. arm64 has no
  execution path anywhere in the fleet; it stays compile+static-verify only. See
  `docs/macos-guest.md` "Executing the wheel".

## Toolchain note (host hazard)

`clud`, on launch, re-pins its blessed `soldr==0.7.11` as the `uv` tool
(`~/.local/bin/soldr`), which predates the `prepare` subcommand and breaks local cross
builds. CI is unaffected (it uses `zackees/setup-soldr`, pinned). If a local `soldr`
build fails with "tool not found: prepare", run `uv tool install 'soldr==0.9.15'`.
