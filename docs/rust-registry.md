# Rust registry foundation

`bosn-registry` is the initial durable SQLite boundary for the Rust migration.
It uses only `kernal-api`'s public SQLite and filesystem-lock facades: SQLite
owns WAL, foreign keys and bounded query materialization; Bosn owns schema and
the typed rows. A writer holds an advisory lock on the state database inode for
its complete lifetime. Read-only diagnostics do not lock, initialize, migrate,
or write.

The schema is v5 and contains the eight Python-v4 tables: `meta`, `resources`,
`resource_uses`, `leases`, `execution_sessions`, `volume_creation_intents`,
`generations`, and `events`. The unique `(kind, name)` resource identity and
all Python foreign keys/indexes are retained. List methods require a caller
chosen limit, clamp it to `1..=1,000`, and never silently truncate: a next
offset is returned.

Execution-session lease IDs are read and written as `Vec<String>` and volume
intent labels as `BTreeMap<String, String>`; malformed persisted JSON is a
typed registry error, never silently accepted.

This is deliberately unreleasable today. `kernal-api` 0.1.0 is not published,
so normal development is pinned to upstream revision
`ed197c5c606d6fd984190721e309c30d7d0383ef`. The development-only kernel
checkout is expected at `_vender/kernal-api` for bootstrap/review; Bosn links the
exact upstream Git revision rather than that working tree. `python ci/verify_release_dependencies.py
crates/bosn-registry/Cargo.toml` intentionally fails until that dependency is
replaced with exact published `kernal-api = "=0.1.0"`.

For source inspection/bootstrap only (never as a Cargo path dependency), use:

```text
git clone https://github.com/zackees/kernal-api _vender/kernal-api
git -C _vender/kernal-api checkout ed197c5c606d6fd984190721e309c30d7d0383ef
```

## Python-v4 bridge guard

`acquire_legacy_migration_guard(state_dir)` takes the exclusive Rust half of the
cooperative Python-v4 cutover lock at `state_dir/registry.migration.lock`. It uses
only kernal-api's public filesystem lock facade. It proves all upgraded Python
writers that hold the matching shared guard have closed; it does not claim to fence
a pre-bridge Python release. The later importer must validate the durable cutover
marker and old-process activation proof before treating this guard as quiescence.

## V4 import boundary

`import_python_v4(state_dir, state_dir/registry.sqlite3, destination)` holds the
bridge guard while it securely reads the private protocol-v1 marker, snapshots the
source through SQLite backup, validates typed rows and native ownership liveness,
and writes a complete v5 database in private staging. The completed staged database
is integrity-checked then published non-overwriting to an owner-private destination
directory. Imported registries retain a reserved reconciliation-required meta gate;
ordinary writers refuse them until a future engine-reconciliation milestone.

The marker only fences bridge-capable Python releases. Upgrade activation and its
old-process proof remain an explicit prerequisite; this API does not infer that an
uncooperative old daemon is absent.

Fresh creation uses the kernel filesystem facade's atomic private create-new
primitive and takes the advisory lock on the database inode itself. `create_writer`
requires a UUID-shaped caller-supplied registry ID; its secure generation stays
at the async caller boundary. Direct v4 writer opens remain refused with
`LegacyImportRequired`; the explicit guarded importer is the only supported
transition into a reconciliation-gated v5 target. Old-daemon activation proof,
engine/daemon cutover coordination, reconciliation, and native Windows/macOS
validation remain follow-up work.

Local development checks:

```text
soldr cargo test --workspace --locked
soldr cargo fmt --check
soldr cargo clippy --workspace --all-targets -- -D warnings
uv run pytest tests/test_release_dependencies.py -q
```

## Rust daemon foundation

`bosn-service` supplies the intentionally small first native daemon surface,
and `bosn-rs` is its development foreground binary. Python launchers are not
changed. `serve STATE_DIR` hardens the state directory through kernal-api,
creates a fresh private v5 registry with kernel OS entropy when absent, or
opens its existing single writer before it attempts owner-only IPC binding.
An occupied endpoint is refused conservatively; this slice does not retire an
existing filesystem object.

The client and daemon use kernal-api's authenticated async local IPC and its
frozen daemon-frame-v1 envelope. Bosn's private Prost payload has protocol
version 1 and a correlated frame request ID; its only operations are typed
`ping`, `status`, and `shutdown`. Frames are capped at 1 MiB despite the
larger kernel framing limit. Each frame read and write has a three-second
deadline, so a partial client cannot retain admission indefinitely. The
daemon checks the kernel peer user identity on both client and server sides.

`status` is deliberately diagnostic only: it reports the actual stable
registry UUID, schema version, resource/lease/session counts, and the
reconciliation gate. It does not claim engine reconciliation, job execution,
or Docker mutation. A bounded (16-command) actor owns the writer. Each status
query moves the writer through kernal-api's blocking lane and returns it to
the actor before the next command, so SQLite never occupies an async worker.
Shutdown stops admission, drains client tasks, then asks that actor to drop
the writer before the foreground service returns.

The daemon foundation's native test matrix covers malformed and oversized
frames, unsupported protocol versions, response correlation/protocol/kind/
encoding rejection, same-user peer authorization, slow-drip absolute read
deadlines, bounded shutdown, independent state directories, alias clients,
and refusal of legacy-v4 or reconciliation-gated registries before IPC binds
or registry bytes are changed. These checks do not claim engine or job
coverage.

Focused evidence includes all-eight-table typed transaction round trips, drop
rollback, stable-ID reopen, concurrent writer exclusion with independent
read-only diagnostics, missing read-only open followed by safe create, and a
Linux cross-language v4-fixture import matrix. The latter creates a mode-0600
synthetic marker in a mode-0700 temporary state directory, substitutes a PID
from an actual exited child process, checks source bytes remain unchanged, and
covers malformed JSON/FK/schema/marker/sequence failures, live ownership,
held guard, no-overwrite destination, reconciliation writer refusal, and 1,001
additional event rows. Native Windows coverage is intentionally deferred until
a kernel-backed private-ACL fixture setup is available.

## Native MCP stdio surface

`bosn mcp` is the first Hermes-facing native surface. It uses newline-delimited
JSON-RPC 2.0 on stdout and MCP's `2025-06-18` initialization/tools lifecycle;
stdout has no launcher diagnostics. The Python console command invokes the
same Rust function through the shipped PyO3 extension, while native development
can use `cargo run -p bosn-service --bin bosn -- mcp`.

Configure Hermes with the normal installed command and explicitly forward a
state root when it differs from Bosn's default:

```yaml
mcp_servers:
  bosn:
    command: bosn
    args: [mcp]
    env:
      BOSN_STATE_DIR: /absolute/path/to/bosn-state
```

This initial surface offers `bosn_status`, `bosn_job_status`,
`bosn_job_logs`, `bosn_job_cancel`, the read-only `bosn_setup_plan`, and
`bosn_setup_prepare`. The latter accepts only `workspace`, a local-or-HTTPS
`config`, `policy` (`refresh` or `offline`), a 1..=300000 ms deadline, and a
1..=8388608-byte output limit. It promptly returns a durable job ID; use the
existing status/log/cancel tools to observe it. The server never starts a
daemon and MCP cannot supply Docker arguments, mounts, output paths, state
roots, task commands, or arbitrary configuration fields. Job logs are
cursor-paginated and limited to 64 records per MCP page. MCP client disconnect
does not cancel jobs. Container lifecycle, setup-task execution, and converge
or run remain intentionally absent.

The official Rust MCP SDK currently owns a Tokio runtime and stdio transport.
Bosn instead uses kernal-api's owned runtime, so the deliberately small adapter
implements the required external JSON-RPC boundary without adding a second
runtime. A future kernel-compatible MCP adapter may replace it.
