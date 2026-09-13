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
`10e558a9f2eb51c2989c89d05b13cf7636bd374e`. `python ci/verify_release_dependencies.py
crates/bosn-registry/Cargo.toml` intentionally fails until that dependency is
replaced with exact published `kernal-api = "=0.1.0"`.

## Python-v4 bridge guard

`acquire_legacy_migration_guard(state_dir)` takes the exclusive Rust half of the
cooperative Python-v4 cutover lock at `state_dir/registry.migration.lock`. It uses
only kernal-api's public filesystem lock facade. It proves all upgraded Python
writers that hold the matching shared guard have closed; it does not claim to fence
a pre-bridge Python release. The later importer must validate the durable cutover
marker and old-process activation proof before treating this guard as quiescence.

Fresh creation uses the kernel filesystem facade's atomic private create-new
primitive and takes the advisory lock on the database inode itself. `create_writer`
requires a UUID-shaped caller-supplied registry ID; its secure generation stays
at the async caller boundary. Python-v4 files are
explicitly refused with `LegacyImportRequired`; offline backup/quiescence,
import, daemon lifecycle/engine action coordination, and reconciliation are
the next milestone.

Local development checks:

```text
soldr cargo test --workspace --locked
soldr cargo fmt --check
soldr cargo clippy --workspace --all-targets -- -D warnings
uv run pytest tests/test_release_dependencies.py -q
```

Focused evidence currently includes all-eight-table typed transaction round
trips, drop rollback, 1,001-row pagination, stable-ID reopen, concurrent
writer exclusion with independent read-only diagnostics, missing read-only
open followed by safe create, and v4/newer schema refusal.
