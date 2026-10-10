# Agent notes for Bosn

Settled decisions, recorded so they are not re-litigated. Change them only with an
explicit new decision, not by inference.

## CI and release gate (staged rollout)

Ordinary PRs and `main` pushes run minimal CI; the literal `ci-test` PR label
adds Linux lint, unit, and Docker tests. The literal `ci-full` PR label runs the
full platform matrix, including hosted Intel and Apple Silicon macOS smoke
checks. Before **any** tag or release, full CI must pass on the exact candidate
commit SHA. See [CI tiers and queue cost](docs/ci-queue-slo.md) and the
[fleet rollout issue](https://github.com/zackees/soldr/issues/3345).

Releases go only through the **pretag release gate** (see "Releasing" below):
an explicit request for an exact candidate SHA on `main`, refused unless full
CI already passed on that SHA. Nothing tags or publishes on a version bump or a
pushed tag; never create a release tag by hand.

## act2 dependency release authorization

Act2 releases are always authorized when necessary to implement or ship Bosn
work. Proceed without asking for additional release approval. Before tagging,
require full act2 CI to pass on the exact candidate commit SHA, use a clean
checkout of act2's default branch, and verify that the tag and release do not
already exist. Publish through act2's existing tag-triggered release workflow.
Verify the resulting binaries and checksums before updating Bosn's pinned
version and artifact digests. Record release evidence and rollout status in the
living implementation spec.

## Code rules (hard gates in `./lint`)

- **Single responsibility and DRY.** Parse inputs eagerly into typed structs
  and enums at the boundary; never probe untyped JSON for fields.
- **Every Rust, Python and shell file stays under 1,000 lines**
  (`ci/lint_file_length.py`). Split by responsibility; never raise the limit.
- **Write a path once.** Within a file, a directory appears in at most one
  `include_str!`/`include_bytes!` path. Several files from one directory go
  through one base macro, e.g.
  `macro_rules! asset { ($n:literal) => { include_str!(concat!("assets/", $n)) }; }`,
  and a file used more than once is bound to one `const`
  (`ci/lint_include_base.py`). A `concat!` base must stay inside the crate's
  `src/`, because `ci/publish_amalgamate.py` only relocates literal includes.

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

## Releasing (the pretag release gate)

- **The version is written once**: `[workspace.package].version` in the root `Cargo.toml`
  (the zccache/soldr pattern). `bosn` and `bosn-python` inherit it, the wheel reads it through
  maturin (`dynamic = ["version"]`), `bosn.__version__` comes from the loaded extension, and
  `uv.lock` records the project as dynamic. `ci/verify_release.py` refuses any second copy.
  The internal crates are never published and keep their own versions.
- **Prepare the version by PR**: `./bump patch` (or `minor`, `major`, `X.Y.Z`) rewrites that
  one line and refreshes `Cargo.lock`'s two workspace entries. Merging it releases nothing.
- **Request the release for the exact merged SHA** (`<SHA>` = full 40-hex commit on `main`):
  1. Full CI on that SHA: `gh workflow run ci.yml -f tier=full -f commit_sha=<SHA>`. ci.yml
     names the run `CI full <SHA>`; wait for it to go green and note its run ID.
  2. Optional release-request issue (the control record): body lines
     `candidate_sha: <SHA>` and `tag: vX.Y.Z`. The release comments and closes it.
  3. Dry run, then publish:
     `gh workflow run auto-release.yml -f candidate_sha=<SHA> -f full_ci_run_id=<ID> [-f issue=<N>]`
     (dry by default: builds and verifies all four wheels, publishes nothing), then the same
     with `-f dry_run=false`.
- **What the gate enforces** (`ci/release_gate.py`, the guard job every other job needs):
  the SHA is full, lowercase, checked out, and reachable from `origin/main`; the CI run is a
  `workflow_dispatch` of `ci.yml` from `main` named `CI full <SHA>`, completed `success`, with
  every full-tier cell (`ci/verify_full_coverage.py`'s `REQUIRED`, hosted macOS smokes
  included) and `Full CI coverage` successful; tag `v<version>` is new or already names that
  SHA; a named issue is open and names the same SHA and tag. The tag is created only by the
  final `GitHub release` job (`gh release create --target <SHA>`), after PyPI.
- **Retry by dispatching the same SHA again**, never by bumping the version: every
  publishing step is idempotent (PyPI and GitHub assets are byte-compared, only missing
  files are uploaded). `auto-release.yml` has no push or tag trigger.
  `tests/test_auto_release_guard.py` holds the gate and that wiring.
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

- **Hosted macOS runners are reserved for `ci-full` and release smoke checks.**
  `ci/lint_no_macos_runners.py` rejects macOS runner labels outside those gated jobs.
  Both `x86_64-apple-darwin` and `aarch64-apple-darwin` wheels
  build on `ubuntu-latest` through `zackees/setup-soldr` (floating `@v0`) +
  `soldr prepare --target …`, using Soldr's LLVM 21.1.5 and managed Apple SDK 14.5 —
  never Xcode, zig, or osxcross.
- **Linux verifies macOS wheels statically; full CI and release execute them on
  matching hosted Macs.**
  `ci/verify_cross_wheel.py` checks the Mach-O extension and bundled CLI (arch,
  filetype, min-OS ≤ tag floor, allowed system dylibs, no ELF/OpenSSL contamination),
  tag, and version alignment. The floors are 10.12 (x86_64) and 11.0 (arm64).
- **The legacy Recovery smoke remains advisory in `macos-x64-execute.yml`** — a macOS
  Recovery guest (`zackees/docker-mac-x64`, OSX-KVM on `ubuntu-latest`, no macOS
  runner). It is nightly + manual, never a required check or release gate. Both
  architectures now have hosted full CI and release smoke checks. See
  `docs/macos-guest.md` "Executing the wheel".

## Toolchain note (host hazard)

`clud`, on launch, re-pins its blessed `soldr==0.7.11` as the `uv` tool
(`~/.local/bin/soldr`), which predates the `prepare` subcommand and breaks local cross
builds. CI is unaffected (it uses `zackees/setup-soldr@v0`). If a local `soldr`
build fails with "tool not found: prepare", run `uv tool install 'soldr==0.9.15'`.

## Daemon version contract (#509, #324)

Decided 2026-10-09.

- **Every daemon-backed CLI command runs the version pre-flight** before it
  sends work (#324), and `bosn mcp` runs it once at startup against a live
  daemon: an incompatible daemon is refused with both releases, both
  protocols and the remedy, never a bare "request failed".
- **Clients accept by protocol (#509 phase 2).** A daemon reporting this
  client's `DAEMON_PROTOCOL` is accepted whatever its release (a one-line
  release-skew note goes to stderr); another protocol is refused. A daemon
  reporting protocol 0 predates the handshake and keeps the exact-release
  check. `daemon_version_mismatch` is the one place this is decided.
- **Compatibility moves from release equality to a wire protocol (#509).**
  Every daemon reports `DAEMON_PROTOCOL` on its ping reply (zero means it
  predates the handshake). Bump it on any change to the request/reply wire or
  the JSON documents it carries; `protocol_surface_is_pinned` fingerprints those
  files and fails until the bump decision and new fingerprint are recorded.
- **Not soldr's per-generation daemons.** soldr's broker can launch one daemon
  generation per client image because its daemons are caches. Bosn's daemon is
  the sole registry writer and Docker owner for a state directory, so it stays
  one daemon per state directory; skew is solved by the daemon serving a
  window of protocols, not by running several daemons (phased plan on #509).
