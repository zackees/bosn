# Pure Rust manifest and policy scope

`bosn-core` now parses supplied TOML text into typed stack/task declarations and resolves
policy from caller-supplied values. It does not open files, inspect paths, read environment,
observe CPUs/clocks, download URLs, or invoke a process.

`ManifestRoots` deliberately keeps three distinct opaque inputs: document source/provenance,
build-asset/materialization root, and workspace bind root. Bind sources and Dockerfile paths
remain unresolved spellings for a future kernel resolver; lexical normalization here only
protects container destinations and is not represented as a symlink-safety guarantee.

The supported manifest subset is the existing `stack`/`task` TOML shape: image or Dockerfile,
family/default selection, volumes/scopes/retention, mounts/read-only, tmpfs, env/workdir, and
the macOS guest declaration including explicit license acknowledgement. Digesting Dockerfiles,
COPY context, remote assets, setup-document inline Dockerfiles/companions, cache/apply, and all
filesystem/process work are intentionally deferred.

Machine policy precedence is explicit: defaults (with a supplied CPU observation) < supplied
machine file TOML < supplied environment values < supplied CLI values. `MachinePolicy` remains
immutable when `resolve_app_policy` derives an `AppPolicy`; the latter exposes the original
machine policy and only tightens `run_max_duration` or `build_ttl_seconds`. App input can never
change machine-wide settings or raise a machine ceiling. Origins remain attached to both views.

## Test evidence

The original implementation did not retain a pre-implementation RED test command; its only
recorded intermediate failure was Rust `E0382` (a moved `kind` value), which is not evidence of
missing behavior. The review corrections were RED first: the newly added separate manifest and
policy tests made `soldr cargo test --workspace --locked` fail because the old API had only two
manifest roots and lacked `resolve_machine_policy`/`resolve_app_policy`. After implementing the
three-root and split-policy APIs, the same command is GREEN. The final gate is also
`soldr cargo fmt --all -- --check`, `soldr cargo clippy --workspace --all-targets -- -D warnings`,
and `git diff --check`.

Final local result: 19 tests passed (13 domain, 4 manifest, 2 config), with no failures.
Formatting, warnings-denied Clippy, and the diff check also passed. The Python reference
manifest/config tests passed 64 cases, with one Windows-only drive-path test skipped on Linux.
