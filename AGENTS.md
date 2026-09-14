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

- **Build backend: target state is `build-backend = "soldr"` (soldr's own pattern).**
  `bosn_build_backend.py` is a custom PEP 517 wrapper built on a **false premise**
  its docstring states outright — that maturin "does not package a Cargo binary from
  the same mixed project." maturin **does**: soldr's own wheel (`soldr-cli` = `[lib]`
  → `soldr._native` + `[[bin]] name="soldr"`) ships a `bin/soldr` next to the
  extension via pure `build-backend = "soldr"`, no delegate. `bosn-python` has the
  same shape but disabled the bin (`[tool.maturin] targets = [cdylib]`) and
  hand-stages it. Bosn can drop the custom backend and mirror soldr; the Linux
  OpenSSL bundling is maturin+patchelf (why soldr adds `patchelf` to build-requires).
  Migration and the one open validation (cross-crate `[[bin]]` source) are tracked in
  **zackees/bosn#262**. Until then `bosn_build_backend.py` stays — do not reduce it to
  plain maturin without also un-disabling the bin target and rewiring the `bosn`
  command.

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
