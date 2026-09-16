# Unmanaged Docker artifacts: census, warning, and cleanup

This is the native re-base of the ruleset in [#148](https://github.com/zackees/bosn/issues/148),
under the goal in [#149](https://github.com/zackees/bosn/issues/149) and the findings in
[#147](https://github.com/zackees/bosn/issues/147). It is slice S1 of
[#268](https://github.com/zackees/bosn/issues/268): **the contract**. S2 lands the census,
S3 the warning and the cleanup command, S4 unattended operation, S5 pressure attribution.

The ruleset is unchanged in intent from #148. What changed is the implementation it
describes: #148 was written against a Python implementation that no longer exists, and every
`file:line` citation in it and in #149 (`gc.py:209`, `resources.py:154-158`,
`accounting.py:120-134`, `retention.py:252-255`, `cli.py:263-276`, `autostart.py:45-55`)
points at removed code. This document replaces those citations with live ones and closes the
open questions #148 left behind, so the remaining slices can be implemented without
re-litigating them.

## Why this exists

The product's original title-level promise, at the revision #153 was filed against
(`b033779`), was:

> `# bosn — let your agents use Docker all day without filling the disk`

The native README opens with `# Bosn` and describes safe management of Docker development
applications. Nothing in `docs/`, `AGENTS.md`, or `README.md` referenced #147/#148/#149 at
all before this document, and `docs/migration-rust.md`'s release work did not list the
workstream. The chain was dropped during the Rust migration, not deferred. #147's failure was
never "GC is broken" — GC is fine — it was that **nothing was ever routed through bosn, and
bosn had no way to say so.**

## Current native state

This is the honest baseline. It is the reason S2–S5 are not a port but a build.

| Surface | Native state | Evidence |
|---|---|---|
| `bosn gc` | Retired **Bosn-managed setup containers** in one workspace | `crates/bosn-service/src/bin/bosn.rs:506` (`preview`), `:537` (`apply --candidate TOKEN`) |
| `bosn manifest volume-gc` | Retired warm **spec-scoped manifest volumes**, one workspace | `crates/bosn-service/src/bin/bosn.rs:2389` |
| `excluded_unmanaged` | A count of **registry rows in one workspace** that do not match the `bosn-setup-*` shape — not a host census | `crates/bosn-registry/src/lib.rs:2428`, `:2441`; volumes `:2539`, `:2559` |
| Engine census | **present** since S2 (`bosn scan`, #270): one bounded `docker system df -v --format json` plus a dangling filter, one label query per required key, and a bounded `docker volume inspect` | `crates/bosn-core/src/unmanaged.rs`, `crates/bosn-service/src/unmanaged.rs` |
| Byte accounting | **present** for the unowned bucket since S2 (#270); approximate by construction, and never zero for an unmeasurable class | `bosn scan --json` |
| `scan` | **present** since S2 (#270), read-only | `crates/bosn-service/src/bin/bosn.rs` `run_scan` |
| `--ack` / `foreign_ttl` / warning threshold | **present** since S3 (#271): `bosn scan --ack`, `--ttl-seconds`, `--warn-bytes`, `--warn-objects` | `crates/bosn-core/src/unmanaged.rs` |
| `gc --unmanaged` | **present** since S3 (#271) and S3b (#276): the preview is read-only and local, the removal is a daemon operation | `crates/bosn-service/src/bin/bosn.rs` `run_gc_unmanaged`; daemon op 35 in `crates/bosn-service/src/lib.rs` |
| Autostart | **present** since S4 (#272): `bosn daemon autostart enable\|disable\|status` writes the platform entry **and registers it** (`systemctl --user enable --now`, `launchctl load -w`) | `crates/bosn-service/src/autostart.rs` |
| Maintenance pass | **present** since S4 (#272): the daemon runs the census at start and every hour, logging the warning | `crates/bosn-service/src/lib.rs` `serve` |
| Retention / pressure / verdict model | **Implemented and tested, and unwired.** `Pressure::assess`, `evaluate`, `collectable_ordered`, `container_should_stop`, `lease_expired`, `retention_signals`, `PolicyDefaults`, `RetentionConfig` have **zero production consumers** | `crates/bosn-core/src/lib.rs:362-570`; `crates/bosn-core/src/config.rs:53-74`; exercised only by `crates/bosn-core/tests/domain.rs` |

Two consequences worth stating plainly, because both change what the remaining slices are:

1. **`excluded_unmanaged` is not a census and cannot become one.** Its predicate is scoped to
   `r.kind='container' AND r.workspace=?`, so it can only count things Bosn already has a
   registry row for. #147's 31 dangling images were built by plain `docker compose build` —
   no row, no label, therefore invisible by construction. Extending this counter would
   reproduce the blind spot it is being asked to fix.
2. **There is no live retention engine.** The pure `bosn-core` model above is correct and
   tested, but the daemon does not call it: today's GC is two token-bound, registry-scoped,
   per-candidate SQL previews. The machine-wide pressure path that G7/S5 describes therefore
   cannot fire today. S5 is not a bug fix for live behaviour; it is the guarantee that the
   pressure path is foreign-aware **when it is wired**, plus the pure-domain change that
   makes that provable now.

## The census (S2)

### Scope

Host-wide and label-based. Not registry-scoped, not workspace-scoped, and not per-kind. The
subject of the census is exactly what Bosn has no row for.

### Classification by ownership

Every observed resource is placed in one of four ownership classes, using the existing label
contract — `crates/bosn-core/src/lib.rs:31-48` (`NAMESPACE`, the `LABEL_*` keys,
`REQUIRED_LABELS`), `:225` (`is_owned_by`), `:243` (`ownership_from_labels`).

| Class | Definition | Treatment |
|---|---|---|
| **Ours** | Complete 7-label set and `registry` equals this registry's UUID | Excluded from the census entirely. Owned lifecycle already has its own GC. |
| **Foreign registry** | Complete 7-label set, `registry` is a different UUID | **Tier 2, protected.** Reported as `foreign-registry`. |
| **Incomplete labels** | Some Bosn labels present, but not the complete set | **Tier 2, protected.** Reported as `incomplete-labels`. This is the class #267's host hit 924 times. |
| **Unlabeled** | No Bosn labels at all | Eligible for the Tier 1 / Tier 2 classification below. |

Incomplete and foreign labels are **never** reclaimable, in either tier, by any flag. This
is the founding invariant: names never prove ownership
(`docs/rust-domain.md`), and a partially-labelled resource cannot be proven to be either
safe or ours.

### Tier 1 — reclaimable

Safe to remove with a documented command. Age is measured from engine timestamps.

| Class | What | Age gate |
|---|---|---|
| A | Dangling images, per Docker's own `dangling=true` verdict | > TTL |
| B | Exited or created containers | > TTL |
| D | Anonymous (64-hex-named, or Docker-marked) volumes, attached to nothing | > TTL |

| Class | What | Why it is reported but never swept |
|---|---|---|
| F | Build cache | `buildx` exposes no per-record removal — only an age-filtered prune, which is not the same as deleting a proven, listed object. It is reported so the warning is truthful about the pile, and **excluded from the reclaimable headline**, because a byte count no command can free is a promise the tool cannot keep. |

### Tier 2 — never swept

Enters a plan **only** when named explicitly with `--include <id>`. There is deliberately no
flag that selects all of Tier 2 at once.

- **Tagged, unreferenced images.** See "what the engine cannot prove" below.
- Named volumes.
- Attached, running, or otherwise in-use resources.
- All foreign-registry and incomplete-label resources (above).

### What the engine cannot prove

Two classes in the original #148 ruleset are **not implementable** against a current Docker
engine. Both were verified against Docker 29.7.2 with the containerd image store, and both
resolve toward keeping, never toward reclaiming:

- **#148 class C (tagged re-pullable images) does not exist as specified.** The rule was
  "non-empty `RepoDigests` ⇒ still in a registry ⇒ re-pullable ⇒ safe to delete". On a
  containerd-store engine *every* image reports a `RepoDigests` entry, including images built
  locally and never pushed: 79 of 79 images on the reference host, with
  `twp-e2e-kumquat-php:local` carrying `twp-e2e-kumquat-php@sha256:<its own local id>`.
  `docker system df -v` reports an empty `Digest` for all of them, and
  `docker image ls --format '{{.Digest}}'` returns the image *ID*, not a repository digest.
  Proving remote existence would require a network call to the registry, which a read-only
  census does not do. So a tagged, unreferenced image is reported in full and left for a
  human: removing it could destroy the only copy, which invariant 2 forbids.
- **#148 class E (orphan networks) has no data source.** `docker system df -v` returns no
  network section, so a network has no byte total and no age. Networks contribute nothing to
  a byte-thresholded warning, and inventing one from separate reads is not worth the extra
  engine surface. A network observed by the census is protected as unclassified.
- **#148 class F (build cache) has no per-object removal.** See the table above: it is
  counted and reported, and never swept.

Consequently class A (Docker's dangling verdict) is the only image class the census can
sweep. That is the correct outcome, not a shortfall: dangling images are the large reclaimable
set in both incidents (12 GB in #147), and the rest genuinely require a judgment the engine's
accounting data cannot make.

### Byte accounting

The census reports, per class: object count, bytes, and oldest age. Build cache is included
(F — `docs/migration-rust.md`'s predecessor accounting never read it).

It is assembled from bounded, read-only engine reads:

| Read | Supplies |
|---|---|
| `docker system df -v --format json` | one document containing images, containers, volumes, and build cache, with sizes and creation times |
| `docker image ls -aq --filter dangling=true` | Docker's authoritative dangling verdict |
| `docker image ls -aq --filter label=<key>` | which images carry a Bosn label — `image ls` exposes no labels, so this is one bounded query per required key |
| `docker volume inspect <names…>` | volume creation time, which the accounting document does not report at all |

Every read carries an explicit deadline and output cap. A volume the engine does not answer
for stays unaged, and an unaged artifact is protected.

**Bytes are approximate; identity is exact.** Docker's own accounting surfaces
(`docker system df -v --format json`, `docker buildx du`) report human-unit strings with
about four significant digits. The census parses those and marks its output
`bytes_approximate: true`, because the error is under 0.1% and every use of the number is a
threshold decision. Nothing is ever *deleted* by size — deletion is by immutable ID. A size
that cannot be parsed is a refusal for that class, never a guess.

### Fail closed

A class whose listing or sizing fails is reported as `partial` (per-class `partial: true`
plus a top-level flag), **never as zero**. A partial census must never look like a clean
machine. No plan is built from a partial census, and a partial census never authorises
eviction.

## The warning (S3)

Printed by `bosn doctor` and by `bosn scan` when the footprint is over threshold, and by the
tail of any command that ran a maintenance pass (S4). At most once per invocation. There is no
`bosn status` verb in the native CLI; `doctor` is the always-on surface — the one a user runs
when something is wrong, and the one that would have caught #147.

1. **Yellow, ALL CAPS headline.** Detail lines stay mixed-case — an all-caps table is
   unreadable and the shouting must mean something.
2. **stderr**, matching the existing precedent for problem output.
3. **Degrades cleanly.** Honour `NO_COLOR`, `TERM=dumb`, and non-TTY stdout by emitting plain
   text. Caps survive; escape codes do not. No colour machinery exists in the native CLI
   today, so this is a new output primitive, not a configuration of an existing one.
4. **JSON never contains ANSI or caps.** `--json` carries a structured `foreign_reclaimable`
   block: per-class counts, bytes, oldest age, and `partial: true` when a scan was incomplete.
5. **Bosn never runs the suggested command itself.** It prints it, and it is always a `bosn`
   command — never `docker system prune`, whose semantics are the failure mode, not the fix.
6. **It only prints commands this build implements.** Advertising a removal path that does
   not exist would send the user to a command that fails, which is worse than saying plainly
   that it is not here yet.

### Alarm fatigue

A warning printed on every invocation stops being read, which would make the feature
worthless. So:

- **Threshold to warn at all**: 5 GiB or 25 reclaimable objects, whichever is reached first,
  so a healthy machine is silent. Configurable through the existing policy-key mechanism
  (`crates/bosn-core/src/config.rs:7-17`).
- **`bosn scan --ack`** suppresses the current footprint. The warning returns when the
  footprint grows materially past the acknowledged mark (+25%) or after 30 days.
- Tier 2 never appears in the headline. It is a line item pointing at the preview.

## `bosn gc --unmanaged` (S3)

```
bosn scan                            # the census, read-only
bosn scan --ack                      # silence the current footprint
bosn gc --unmanaged                  # preview: plan only, deletes nothing
bosn gc --unmanaged --apply          # execute
bosn gc --unmanaged --include <id>   # opt one Tier 2 item into the plan
```

Preview is the default and deletion is explicit, matching the existing `gc` convention.
Bare `bosn gc` keeps today's behaviour unchanged: owned resources only.

### Removal is daemon-owned, and re-derives its own plan

The preview is read-only and runs locally, like the census. The **removal** mutates Docker,
and only the daemon mutates Docker, so it is a daemon operation.

The plan a client sends is **never trusted**. The daemon takes a fresh census and rebuilds the
plan immediately before removing anything, because a client's preview was taken against state
that may already have changed. That is also why the whole pass refuses outright on an
incomplete census: nothing is deleted from partial information.

This was originally scoped as a job-backed operation, on the assumption that an inline handler
would block the daemon's actor. That assumption was wrong: the daemon spawns a task per client
connection, so a bounded pass blocks only its own caller. The pass is therefore a synchronous
operation with per-removal deadlines, and a cap on how many removals one pass will attempt.
A job would still buy progress reporting and cancellation, and remains available if the
operation grows long enough to need them.

### Why this is a new planning mode, not a widened `gc apply`

Today's `gc apply` is token-bound and per-candidate
(`crates/bosn-service/src/bin/bosn.rs:537`): a preview mints a token for one resource, and
apply re-checks that exact candidate's ownership and attachment before removing it. That
protocol proves a **positive** — this resource is ours and safe — and it must not be
loosened, because it is what makes every owned deletion auditable.

An unmanaged plan proves a **negative**: no complete Bosn ownership for any registry, no
attachment, past the age gate. That is a different proof over a different population, so it
gets its own plan type, its own preview, and its own apply. It does not extend, relax, or
reuse the owned-candidate token path.

### Execution rules

- **Order**: containers → images → volumes → networks → builder cache. Containers first,
  because removing them is what releases the image references blocking deletion.
- **Delete by immutable ID**, never by tag or name.
- **Per-resource failure is logged and the pass continues**, matching the owned GC. Docker
  refusing to remove a parent image is already fail-closed.
- **Every deletion emits a reason** to the existing event log. A foreign deletion must be as
  auditable as an owned one.
- **Re-probe free space afterwards** and report bytes actually reclaimed, not bytes
  predicted. This is where the stale-probe fix (B3) lands.
- **Report what was measured, not what was predicted.** The outcome carries the bytes the
  pass actually accounted for, and the count the daemon re-derived — not the client's.
- **A volume is removed by its generated name.** Docker exposes no separate volume id, so the
  name is the identity; nothing is ever removed by tag.

## Invariants

These bind every slice and are not negotiable per-PR. They are #149's invariants, restated
against the native implementation.

1. Never delete what Bosn does not own without an explicit human command.
2. Never delete what cannot be recreated. Re-pullable and rebuildable are reclaimable;
   local-only images and named volumes are not, regardless of who asked.
3. Fail closed on incomplete information. A partial scan, an unreachable engine, or an
   unmeasurable size is a reason to keep, never to delete.
4. Every deletion carries a logged reason.
5. The user can always pin. An explicit `pinned` retention label overrides every policy.
6. Never prune indiscriminately. Default-deny.

## Decisions

Recorded here to close #268's open questions. Each is a judgment call, not a discovery.

1. **`scan --ack` state lives in a dedicated bounded state file, not the registry.** The ack
   is a user preference about a *notice*, not a fact about a resource. Keeping it out of the
   registry avoids a schema version bump for a preference and, more importantly, keeps
   `doctor` and `scan` — which must be safe to run against an uninitialised or read-only
   state directory — from needing a registry write. It is a bounded file in the state
   directory, and decision 4 below records exactly how it is written and why.
2. **The default warning threshold is 5 GiB / 25 objects.** Carried over from #148
   unchanged, and checked against the reference measurement below: a healthy machine is far
   under it, and #147's incident was ~16 GB across ~34 objects.
3. **There is no `--review` flag.** The preview lists Tier 2 as its own labelled section with
   a per-item reason, so the information `--review` would print is already printed by the
   command the user was told to run. A second flag whose only job is to re-list what the
   preview must compute anyway is a second code path over the same data — and #148's own
   concern was that extra surfaces go unread. `--include <id>` remains the only way a Tier 2
   item enters a plan.
4. **The acknowledgement lives in a bounded `unmanaged-ack.json` in the state directory, and
   is written with `std::fs`.** The kernel facade exposes no plain write primitive, and
   `read_private_regular_file_bounded` requires owner-only permissions the state directory
   does not impose, so both sides use `std::fs` with an explicit size cap and agree on the
   shape. A torn or oversized file reads as *no* acknowledgement — failing toward warning,
   never toward silence.
5. **Autostart (S4) stays in this workstream.** It is what makes the unattended half of the
   warning fire, and the reference machine below is the argument for it. But it is **not** a
   prerequisite for S3: the interactive warning on `status` and `doctor` works without a
   daemon that survives being idle, so S3 may land first.

## Unattended operation

Two halves, because a warning nobody is running cannot fire.

**Autostart registers, it does not merely write.** The Python implementation wrote the
LaunchAgent plist and returned without ever calling `launchctl`, so a macOS user who opted in
got no daemon until their next login. Writing the file is not the operation. `enable` writes
the entry, reloads the manager, and registers; `disable` unregisters, removes the entry, and
reloads — symmetric, because an unloaded file or an unlinked registration is the same defect
from the other side. Every mutating step goes through a `CommandRunner`, so the exact argv is
asserted in tests without requiring `systemctl` or `launchctl` in CI. Real registration was
**not** executed on the development host; only `status`, which reads the filesystem.

**The daemon maintains itself.** It runs the census at start and then on a one-hour interval,
logging the warning to its own stderr — the systemd journal or the launchd log — so a machine
that opted in learns about its unowned footprint without anyone typing anything. The pass runs
on the blocking pool, so the bounded child-process calls cannot stall the accept loop, and the
wait between passes is cancellable, so shutdown is not delayed by up to an hour.

The B5 defect in #147 — "maintenance can be skipped in favour of shutdown" — **cannot occur
here**, because the native daemon has no idle watchdog and no retirement: it runs until it is
stopped. That is a difference in mechanism, not a fix.

## Reference measurement

Point-in-time, read-only, on the development host that produced #267's e2e report
(2026-09-16, Docker 29.7.2). It is recorded here as the acceptance fixture for S2 and S3 —
this is the machine class the feature exists for, and it is a larger pileup than #147's.

```
TYPE            TOTAL     ACTIVE    SIZE      RECLAIMABLE
Images          79        9         46.16GB   22.12GB (47%)
Containers      27        2         15.74GB   15.73GB (99%)
Local Volumes   73        8         23.89GB   17.84GB (74%)
Build Cache     792       0         44.3GB    25.82GB
```

≈ 81 GB reclaimable in total. 65 of 73 volumes are dangling; 2 images carry a Bosn registry
label. The `twp-e2e-*` families named in #267 are present and ungoverned.

Note the contrast with #147: there, `bosn status` reported `registered: 0` and
`~/.local/state/bosn` did not exist. Here Bosn has run, owns 2 images, and still cannot see
any of the ≈81 GB — which is precisely the point of decision 1 in "Current native state":
a registry-scoped counter cannot report what has no row.

## Non-goals

- Automatic reclamation. Deletion of unowned artifacts is human-triggered.
- Replacing `bosn gc` for owned resources.
- A general Docker inventory. `builder` remains declared-but-unsupported as a resource kind
  (`crates/bosn-core/src/lib.rs:51-78`); class F covers build cache as an artifact class, not
  as an owned resource kind.
- Any `docker system prune` semantics, under any flag.
