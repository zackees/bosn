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
bosn gc owned [--state-dir DIR] [--container-ttl-secs N] [--volume-ttl-secs N] \
              [--image-ttl-secs N] [--max-bytes N] [--apply --yes] [--json]
```

Preview is the default. `--apply` and `--yes` must both be present, matching `gc --unmanaged`, so
a bare invocation from shell history cannot delete. Applying requires `--state-dir` and all three
gates — a defaulted gate would silently reclaim under a TTL the operator never chose. A preview
removes nothing, so it defaults to the daemon's state directory and policy: the bare
`bosn gc owned` that the maintenance log prints runs as shown (#551). `--max-bytes` must be
positive.

The daemon re-derives the plan from its own fresh read and re-verifies each object immediately
before removing it. A pass that takes minutes can otherwise remove a volume a new run just
mounted.

## Unattended

**On by default (#545).** A machine has to stay clean by itself, so the daemon's maintenance pass
applies the plan without any configuration. Opt out with `retention.toml` in the state directory:

```toml
auto_retention = false
```

An absent file, or a file without the key, means **yes**. `true`, `yes` or `1` mean yes; any other
explicit value (including a typo or a quoted string) means **no**, as does a file that exists but
cannot be read, because the daemon cannot then prove the operator did not opt out. Opting out
does not silence the pass: it still reads the engine and reports what it *would* remove.

Every removal keeps the same gates whether or not it is unattended: complete ownership labels
naming this registry, liveness (running / mounted / referenced), an explicit pin, and the
per-kind age gate, each re-checked immediately before the object is removed.

### Why an object was kept

Each pass counts the objects it kept by hold reason (`in-use`, `pinned`, `within-ttl`,
`age-unknown`, `foreign-registry`, `incomplete-labels`, ...). `bosn gc owned --json` returns them
as `held`, and the maintenance log prints a `kept owned object(s): ...` line whenever an
actionable reason (anything other than young or alive) is present, so "nothing to reclaim" and
"held for foreign ownership" no longer look the same.

## Setup containers and manifest volumes (#545)

Setup containers (`bosn-setup-v2-*`) and manifest volumes (`bosn-v-*`) are created with the
setup label set (`setup-managed`, `setup-content-sha256`, `setup-container`), not the canonical
ownership set, and an existing Docker object can never gain a label. The pass therefore also
discovers objects by `com.zackees.bosn.setup-managed`, and proves each one against **this**
registry: exactly one `setup-container:` / `manifest-container:` / `manifest-volume:` record must
match its kind, exact engine name and content digest. The record supplies the stack, generation,
scope and workspace. Without such a record the object is held as `incomplete-labels`; a name is
never evidence. `manifest-guest:` containers and setup images are not reclaimed this way.

The proof can only make an object harder to remove: its age is the shorter of Docker's creation
age and the record's idle time (`last_used`), and a lease, execution session or pending volume
creation intent naming it counts as in use. Both are re-read immediately before each removal.

A manifest volume's declared `retention` is honoured, so a `pinned` volume stays. A container
record's `Pinned` is not: every setup and manifest ensure writes it unconditionally and no command
sets or clears it, so it is bookkeeping rather than a human promise. A container is protected by
liveness, leases and sessions instead, and an explicit `pinned` label on the object still wins.

## Abandoned state directories (#545)

Objects created under a temporary or deleted `--state-dir` carry complete labels naming a registry
nothing will open again. Every other registry correctly treats them as foreign, so they used to
leak forever. Every daemon now enrolls `{schema, registry_id, state_dir}` in a machine catalog,
`<native state dir>/registries/<registry-id>.json` (the platform default, ignoring
`BOSN_STATE_DIR`), whatever state directory it was started with.

Only the **machine daemon** (whose state directory is that native default) acts on the catalog,
and only for a registry whose cataloged state directory definitely no longer contains a
`registry.sqlite3`. Objects naming such an abandoned registry are judged as if this registry
owned them, under every ordinary gate (running/mounted, pin, age), re-checked before removal.
Once no container, volume or image names it any more, its catalog entry is deleted.

Live peers stay protected: a registry whose database still exists is foreign, and its objects
are never touched. An object naming a registry the catalog never saw (created before the catalog
existed) stays `foreign-registry` too; a label alone does not prove its registry is gone. A state
directory on a filesystem that is not mounted looks deleted; the ordinary age gates are the
margin for that case.

## Idle keepalive containers are stopped (#545, #536)

A setup or manifest app with no declared command runs the fixed keepalive
(`trap 'exit 0' TERM INT; while :; do sleep 3600 & wait $!; done`) so later tasks can `exec`
into it. Docker reports it running forever, so the pass above would hold it as `in-use` and it
would pin its volumes indefinitely. Before each unattended pass the daemon therefore **stops**
(never removes) a keepalive container when all of these hold, re-checking the container, its
process table and the registry immediately before the stop:

- running, setup-labelled and proven by a record in this registry (above);
- its `Config.Cmd` is exactly the keepalive launcher, not a declared command;
- no lease, execution session or creation intent names it, and it has no `pinned` label;
- both its age and the registry's idle time are past the container gate (6 h);
- `docker top` shows exactly the keepalive shell and its `sleep`.

At most 16 are stopped per pass, with a 10 s grace for the keepalive's `TERM` trap. The same
pass then reclaims the stopped container and, in later passes, the volumes it no longer pins,
under the ordinary gates. A later `setup ensure` or manifest run starts or recreates it. Declared
app containers (a real service) and `manifest-guest` VMs are never stopped by this rule.

## Stopped setup containers are reported by default (#518)

`bosn-setup-v2-*` containers are created with `docker container create` and **no `--rm`**, and
cannot gain one: `bosn-setup`'s `validate_observed` actively enforces `AutoRemove == false`, so
the container is designed to persist and reclamation has to come from this side. A persisted
container keeps every volume it ever mounted alive, so it is a disk problem rather than a
container-count problem.

`maintenance_pass` reports the pile on every maintenance interval, **including after an
opt-out**:

```
bosn retention: 12 stopped owned setup container(s), oldest 59263.4h old, pinning 34 volume(s),
8 past the 6h container gate; the daemon reclaims them once past the gate; see them: bosn gc owned
```

The line leads with the volume count because that is the actual cost, and it names how many are
already past the 6 h container gate — the same `bosn-core` gate an apply pass acts on, not a
constant invented here. Pinned volumes are read from the container's own mount table; a shared
volume is counted once, and a bind mount or an anonymous volume is not counted at all, since
neither is a named blob the operator can act on.

Two properties are deliberate:

- **Reporting is unconditional; deletion is not.** The two must not be confused. With the default
  opt-out config the pass removes nothing and still prints the line; that line is the only bound a
  default install has.
- **The advice changes, the facts do not.** Opting in swaps the trailing hint for a note that
  the daemon reclaims them once past the gate; the counts are the same either way. An applied
  pass reports what it removed and what failed, never the planned count as removed (#551).

## Relation to the existing paths

This does **not** widen any existing destructive path:

- `gc preview` / `gc apply` remain token-bound and one-candidate-at-a-time.
- `manifest volume-release apply` remains the explicit release for durable data. This policy
  reaches `stack`/`machine` volumes **only** through the label-and-age path, and only when they
  are not pinned.
- `gc --unmanaged` is untouched and still refuses to act on a partial census.

## Known limits

- **The registry reset.** Every GC path requires a registry row. A registry that is reset or
  restored from a different `registry_id` orphans everything on disk, and *no* policy can reclaim
  an object whose row is gone. Reclaiming those needs the label-based path above, which is why it
  exists — but it is worth knowing that the registry is the authority for everything else.
- Images report no size from `image ls`; `docker image inspect` is the read used instead.
- Build cache is out of scope entirely, matching `is_removable_by_id`.