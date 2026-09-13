# Rust domain-policy milestone

This is a partial Phase 2 milestone for issue #153, not the whole Rust migration.
It adds a zero-dependency `bosn-core` library containing typed resource/label/ownership
contracts, explicit lease-liveness input, retention snapshots/evaluation, shared-consumer
signal aggregation, pressure assessment, and deterministic collection ordering. It does not
implement a registry, daemon, engine, CLI, Python removal, persistence/import, or final
engine ownership recheck.

The API has no OS, process, filesystem, environment, or clock reads. Callers pass `now`,
configuration, `ObservedLease` records (lease plus per-lease liveness observation), resource consumers, pressure,
and container run-state observations. Missing/unknown/invalid observations protect the
resource. TTL expiry alone never releases a lease: an explicit `ConfirmedDead` observation
is required. PID-reuse observations are represented as `Alive { observed_start }` and protect.

Labels retain the legacy optional retention behavior: an absent label becomes warm without
being emitted again, while an explicitly emitted `warm` label round-trips. Unknown retention,
incomplete labels, foreign registry IDs, and names never prove ownership.

## RED/GREEN evidence

RED (tests written before the API):

```text
soldr cargo test --workspace
FAILED: `bosn_core` exposed none of the requested domain API (67 unresolved-item errors).
```

Formal-review follow-up RED:

```text
changing_legacy_labels_to_pinned_cannot_drop_durable_pin
FAILED: rendered retention was None, expected Some("pinned").
container_idle_stop_uses_only_explicit_time_and_config
FAILED to compile: `container_should_stop` was absent.
```

GREEN:

```text
soldr cargo test --workspace --locked
PASS: 13 integration tests, 0 failures (including finite-input elapsed-time overflow).
soldr cargo fmt --all -- --check
PASS.
soldr cargo clippy --workspace --all-targets -- -D warnings
PASS.
```
