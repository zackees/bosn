# Managed retention (`bosn gc owned`)

Issue #456. Companion to [`rust-unmanaged.md`](rust-unmanaged.md), which this does not replace.

## Why this exists

The unmanaged census protects everything this registry owns:

```rust
OwnershipClass::Ours => return protect(ProtectedReason::OwnedByThisRegistry),
```

That is correct — an owned resource has its own lifecycle. But three kinds of owned resource had
**no lifecycle at all**:

| Resource | How it was created | What removed it |
|---|---|---|
| `bosn-setup-v2-*` container | `docker container create`, no `--rm` | a token-bound `gc apply`, one at a time |
| `bosn-v-stack-*`, `bosn-v-machine-*` | `docker volume create` | explicit release, one at a time |
| `bosn-setup:<sha256>` image | `docker build` | nothing |

None of the predicates in `gc_query.rs` reference age, TTL, or a timestamp; they are purely
structural. The hourly daemon loop (`service.rs`) ran a census and **printed a warning** — it
never deleted.

The consequence compounds. A stopped setup container keeps every volume it ever mounted alive, so
the container leak is upstream of the volume leak: on a long-lived machine 115 exited containers
held 182 volumes, reported as 515 GB. Reclaiming the volumes alone would have freed nothing.

## The policy

Normal `bosn run` and `bosn ci` startup attempts to register the user maintenance
service on Linux and macOS. The service uses the stable machine state directory,
independent of `BOSN_STATE_DIR`, so temporary workspace state does not become its
login-service path. Registration failures are printed; the workspace daemon
still starts. CI and isolated tests do not register host services. Windows
persistent registration is not yet supported.

Machine ownership authority and retention admission locks live beside the
default state directory, in `bosn-retention` (for example,
`~/.local/state/bosn-retention` on Linux without an XDG override). They are not
inside `bosn` and do not follow `BOSN_STATE_DIR`. Losing the entire default
`bosn` directory therefore preserves the catalog, authoritative registry,
and last verified explicit opt-out needed to restore its locator. Removing
both directories loses that recovery proof and retention refuses uncertain
ownership.

`bosn daemon autostart disable` persists an explicit opt-out, including before
the first registration. Later automatic startup preserves that choice;
`bosn daemon autostart enable` clears it after successful registration. Automatic
retention itself is enabled unless `retention.toml` explicitly sets
`auto_retention = false`.

`crates/bosn-core/src/retention.rs` is pure and decides one question per object: **may this exact
object be removed right now?** Callers own the clock, the engine, and the registry.

Gates, per kind, because the cost of rebuilding differs by orders of magnitude:

| Kind | Default | Reasoning |
|---|---|---|
| Container | 6 h | recreated on demand; no state worth keeping once stopped |
| Volume | 14 d | cached toolchain; a cold rebuild is expensive |
| Image | 30 d | content-addressed; each rebuild is a full build |

Removal order is **containers → volumes → images**, because a container pins its volumes. Within
a kind, oldest first, so a capped pass spends its budget on the bytes idle longest.

### The safety contract

The order of the checks in `classify_managed` *is* the contract:

1. **Ownership is proven, never assumed.** A `bosn-act-*` name is not evidence; only the complete
   label set naming *this* registry is. Incomplete labels → `HoldReason::IncompleteLabels`.
2. **Liveness before age.** A running container, or a volume any container mounts, is never
   reclaimable at any age. This is the rule that would have prevented the original incident.
3. **`Retention::Pinned` outranks every age gate.** It is an explicit human promise. A *missing*
   retention label is not treated as a pin.
4. **An unmeasured age fails closed.** `HoldReason::AgeUnknown` — "we could not measure it" is
   not "it is safe".

### Budgets

`MAX_MANAGED_REMOVALS` (1024) bounds one pass; `--max-bytes` bounds the bytes. Exceeding either
**defers** rather than fails, and the plan reports `deferred`. An object whose size the engine
would not report is deferred under a byte ceiling, not counted as zero: an unmeasured size cannot
be proven to fit a budget.

## Running it

```
bosn gc owned --state-dir DIR --container-ttl-secs N --volume-ttl-secs N \
              --image-ttl-secs N [--max-bytes N] [--apply --yes] [--json]
```

Preview is the default. `--apply` and `--yes` must both be present, matching `gc --unmanaged`, so
a bare invocation from shell history cannot delete. All three gates are **required** — a defaulted
gate would silently reclaim under a TTL the operator never chose.

The daemon re-derives the plan from its own fresh read and re-verifies each object immediately
before removing it. A pass that takes minutes can otherwise remove a volume a new run just
mounted.

## Unattended

Automatic retention runs on every daemon maintenance interval by default. To opt out,
create `retention.toml` in the state directory:

```toml
auto_retention = false
```

Absent, unreadable, empty, or malformed configuration uses the enabled default.
Only an explicit `auto_retention = false` disables automatic reclamation.
The daemon must be running. The ownership, liveness, pinning, and age gates apply
to every automatic removal. Stopped setup containers and their pinned volumes
are reported even when automatic retention is disabled.

## Relation to the existing paths

This does **not** widen any existing destructive path:

- `gc preview` / `gc apply` remain token-bound and one-candidate-at-a-time.
- `manifest volume-release apply` remains the explicit release for durable data. This policy
  reaches `stack`/`machine` volumes **only** through the label-and-age path, and only when they
  are not pinned.
- `gc --unmanaged` is untouched and still refuses to act on a partial census.

## Known limits

- **Lost ownership.** Cataloged registries retain machine-level ownership snapshots.
  A missing original directory can recover from a clean snapshot with the exact
  registry UUID, recorded retention setting, and machine writer exclusion.
  Dirty snapshots, changed identities, and uncataloged historical registries
  still require reconciliation; names alone never authorize removal.
- Images report no size from `image ls`; `docker image inspect` is the read used instead.
- Build cache is out of scope entirely, matching `is_removable_by_id`.

## Pass time budget

One synchronous pass has a ten-minute work budget shared by all resource stages
and peer sweeps. Each Docker call uses the smaller of its own deadline and the
remaining pass time. Registry pagination and idle retirement check expiry before
continuing. Expiry reports an incomplete pass and defers remaining candidates;
removals already completed remain in the result. An in-flight bounded registry
read may finish after expiry. The client allows thirty minutes for the reply.

### Registry diagnostic history

Normal event appends retain the newest 4096 event IDs. Each new event accepts
at most 256 UTF-8 bytes for its kind and 64 KiB for its detail, and requires a
finite timestamp. Oversized writes return a diagnostic-bounds error before
insertion. Event history is separate from resource, pin, lease, session and
creation-intent ownership proof; trimming events never changes those tables.
Legacy import preserves its source event records and IDs; normal appends then
age older imported events out of the recent-history window. SQLite reuses freed
pages, so an existing large database need not shrink on disk to stop growing.
