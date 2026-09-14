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

- **Build backend: target state is `build-backend = "soldr"`; current state is the
  custom `bosn_build_backend`.** soldr dogfoods `build-backend = "soldr"` for its own
  wheel (no delegate, no `maturin` in `build-system.requires` — soldr provisions its
  own pinned maturin and reads `[tool.maturin]`). Bosn cannot use the pure soldr path
  **yet**: Bosn ships a *second* artifact in the wheel — the `bosn-native` CLI
  (`crates/bosn-service/src/bin/bosn.rs`, staged into `.data/platlib/bosn/_bin/`) — and
  soldr's native build has no hook to build+stage an auxiliary Cargo bin. So
  `bosn_build_backend.py` builds+stages the CLI, then delegates the extension to
  maturin.
  - Do not "simplify" `bosn_build_backend.py` down to plain maturin: that drops the CLI
    from the wheel and breaks the `bosn` entry point.
  - Migration is tracked: **zackees/soldr#3239** (native auxiliary-bin staging) →
    **zackees/bosn#262** (adopt the soldr backend once it lands). When adopting the
    interim delegate form, `zackees/setup-soldr` must be added to **every** build lane
    (the Linux/Windows `native-wheel` jobs currently have none; a soldr backend without
    it fails with "cannot resolve the broker daemon route").

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
