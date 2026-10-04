# CI cache and Docker footprint: living spec

This document records the verified behavior of Bosn's act2 engine and the
remaining work to make repeated CI runs fast without unbounded disk growth.
Update the survey, implementation table and verification evidence in the same
change as each implementation step. The desired behavior below is a contract,
not a claim that it is already implemented. Related issue: #456.

## Survey and evidence (2026-10-03)

The read-only host audit in #456 measured ~859 GiB under `/var/lib/docker`.
`bosn-ci-cache-v1` was ~49 GiB, of which `actcache/` was ~41 GiB. Seventy-six
unattached `bosn-v-stack-*` or `bosn-v-machine-*` volumes were present; their
attachment state alone says nothing about safe removal. An ~18 GiB anonymous
volume sampled during the audit was attached to an active CI engine. The audit
did not establish an anonymous-volume leak.

The implementation survey was refreshed against `main` at `146515cf` (act2 `0.2.89-act2.3`). The session checkout was on the older `fix/359-workspace-identity` branch, so implementation work is in `../bosn-extern/bosn-456` on `fix/456-ci-cache`. The current code and tests give the following narrower evidence:

| Resource | Reuse between runs now | Accounting now | Expiration now |
|---|---|---|---|
| act release | Verified tar in shared `bosn-ci-cache-v1/tools/` | Class bytes and allocated blocks in `runners cache` | Whole-cache clear only |
| Runner image | Saved tar in shared `images/`, loaded into each fresh engine | Class bytes/blocks; each active engine also holds loaded layers outside this total | Engine removal deletes nested copy; shared tar lasts until whole-cache clear |
| Action checkouts | Shared `actions/` path passed to act | Class bytes and allocated blocks | Whole-cache clear only |
| `actions/cache` archives | Shared `actcache/<repository hash>/` path passed to act | Class bytes/blocks and per-repository namespace breakdown | act2 has fixed age GC while its server runs; no configurable size ceiling |
| `/opt/hostedtoolcache` | Seeded from shared `toolcache/`; completed installs copied back before teardown | Class bytes and allocated blocks | Whole-cache clear only |
| Job container/image layers and job build cache | Private nested engine per run, optionally prepared as a spare; runner tar is loaded, other layers rebuilt or pulled | Nested bytes are not attributed per run in CI cache usage | Owned engine retirement removes the engine and disk-backed anonymous storage; tmpfs storage disappears with the engine |
| CI run records/source snapshots | Retained in daemon state, separate from Docker cache | `runners prune-cache --max-bytes` measures these files | Count, age and size pruning |
| Host Docker build cache and non-CI Bosn volumes | Outside the CI cache volume | Unmanaged census reports only unowned artifacts; owned resources are protected there | Separate conservative lifecycle commands; no aggregate owned-volume quota |

Evidence: `crates/bosn-service/src/ci/engine.rs` mounts the named volume,
constructs act cache paths, seeds and saves tool installs, saves the runner
tar, and delegates exact engine retirement to `crates/bosn-service/src/act_engine/`. The current engine is verified before retirement and uses disk or tmpfs storage according to its frozen creation profile. `crates/bosn-service/src/ci/runtime/runners.rs`
measures the whole volume and prunes run records separately.
`crates/bosn-service/tests/ci_live.rs` contains an opt-in second-run
`actions/cache` restore proof; its module also describes leak checks.
`crates/bosn-service/src/ci/lifecycle.rs` has fault-path and restart
reconciliation tests, but those are not proof of a host-wide storage ceiling.
`docs/rust-unmanaged.md` documents the existing machine-wide *unmanaged*
census and its deliberate exclusion of resources owned by this registry.

### act2 already performs age GC

The pinned act2 cache server inherits a metadata-aware age GC: unused entries
expire after seven days, entries older than thirty days expire even if used,
incomplete uploads expire after five minutes, and superseded entries have a
five-minute grace period. GC runs when the server starts and at most hourly
while it handles requests. It has no byte ceiling. Therefore an idle namespace
receives no maintenance and heavy weekly use can still grow very large.
Evidence: [the pinned act2 handler](https://github.com/zackees/act2/blob/v0.2.89-act2.3/pkg/artifactcache/handler.go).
Bosn must coordinate with this metadata rather than delete archive files by
creation time and call that LRU. A live archive transfer also needs explicit
protection when multiple servers share a namespace.

### Current live-host survey

A fresh host probe found `/` at 93% full with 134 GiB free, and the shared CI
volume retains its exact Bosn machine/pinned labels. Docker container and
volume inspections respond, but `docker system df` stalled in the previous
survey. Component accounting should use bounded direct volume measurements
rather than depend on that whole-engine operation.

## Accounting implementation in this branch

`bosn ci runners cache` now returns `bytes` (apparent file lengths),
`allocated_bytes` (filesystem blocks), `components`, `partial`, and `errors`.
Components contain class totals and valid repository namespace details. The CLI
shows the twenty largest entries; JSON carries up to 256 repository namespaces
plus the five class totals. Larger stores retain their largest or unknown
namespace details and explicitly mark the report partial. Errors are bounded,
so an arbitrarily large machine census cannot overflow the 64 KiB tool reply.
The widget/dashboard summary uses allocated bytes and identifies archive bytes.
Older JSON readers retain the existing `volume` and `bytes` fields.

Measurements are bounded, read-only, and non-atomic: active writes can change
sizes between samples. Class totals contain their namespace entries; do not
sum both, or sum component samples into the independent volume total. Unknown
files are included in the volume total. No layer sizes are added to it. A
Docker outage, permission error, malformed sample, or failed measurement is
reported as partial/unknown. Only Docker's explicit `no such volume` verdict
reports absence. The temporary reader uses Docker auto-removal and explicit
cleanup by its immutable ID after every read result, including a measurement command deadline.
Cleanup errors identify the container for recovery. Discovery after a create
command that returns no ID still needs partial-create recovery verification.
Its read-only cache mount and masked engine data volume create no build cache.

Verification: the two initial focused tests failed before the implementation
(missing allocated bytes/components, missing partial flag), then passed.
Real sparse-file measurement, malformed/duplicate records, Docker outage,
public runner error reporting and failed-helper cleanup are covered by tests.
The generated CI schema was refreshed from the Rust types in an isolated
container with a writable docs mount.

A third RED repro created 1,000 namespaces and malformed samples: the JSON
report exceeded the tool limit. Bounded namespace/error details turn it GREEN
without discarding the independently measured volume total.

A read-only invocation of the actual BusyBox measurement script took 4.38 s
on the shared volume and reported 53,272,137,728 allocated bytes (~49.6 GiB):

| Component | Allocated bytes | Approximate GiB |
|---|---:|---:|
| act archives | 44,340,707,328 | 41.3 |
| tool installs | 5,726,400,512 | 5.3 |
| runner image tar | 2,226,663,424 | 2.1 |
| action repositories | 945,635,328 | 0.9 |
| act release archives | 32,727,040 | 0.03 |

There were 58 repository namespace measurements. The largest three occupied
13.95, 10.53 and 7.16 GiB. This verifies the need for a byte policy even with
act2's existing age GC. The sample is evidence of usage, not permission to
delete those stores. The reader container was removed after the sample.

Cache quotas, eviction coordination, warm-run benchmarks and aggregate owned
engine/image/build-cache attribution remain open. This slice is local branch
implementation, not evidence of a released runtime change.

## act2 retention implementation in progress

The sister checkout `../bosn-extern/act2-cache`, branch `feat/cache-budget`,
starts from act2 `master` at `5af1c44`; the tested retention slice is committed
locally as `f702202`. Its focused byte-limit repro failed
against existing GC: two 80-byte completed archives remained with a 100-byte
budget. The candidate metadata-aware LRU pass turns that repro green and
keeps the newer archive warm. It measures actual archive lengths rather than
trusting the declared upload size, refuses unmeasurable data, and preserves
metadata when removal fails so future accounting can still find the data.

The next focused repro confirmed a cross-server race: GC could unlink an
archive while another server was downloading it. The candidate now uses a
separate bbolt file for an OS shared lock over each complete HTTP request and
an exclusive lock for GC. A two-process test proves protection during a
transfer and expiration after it finishes. Lookup refreshes use metadata and
absolute-age expiry honors the five-minute lookup-to-download grace.

Act2 exposes typed byte, maximum-age, unused-age and maintenance-interval
settings. A periodic timer maintains an idle **running server**, preserving
warm LRU entries. The artifactcache suite passes with the Go race detector;
focused tests cover policy parsing, invalid settings before store creation,
actual archive lengths and failed-removal metadata preservation. The full CLI
suite passes against a separate private Docker 29.7.2 test daemon with
RAM-backed storage (49.66 s); no host Docker socket or host cache was used.
The changed act2 packages also pass pinned golangci-lint v2.11.4. The CLI suite
caught direct test-input fixtures missing the new policy defaults, and those
fixtures were corrected to match the actual CLI boundary.

This candidate is not enabled in Bosn. Offline maintenance of namespaces with
no server, a machine aggregate budget, bounded repository audit/export and
end-to-end concurrent restore tests remain open. A shared namespace must use
the same coordination protocol in every server; legacy peers cannot be
assumed to hold the new transfer lock. The Bosn pin continues to name the
existing released act2 binary. The dependent branch has its own detailed
contract in `act2-cache/docs/cache-retention.md`.

## Docker-backed verification

The complete opt-in `ci_live` suite passed: six scenarios, zero failures,
473.76 s, using a private Docker 29.7.2 daemon with 20 GiB RAM-backed storage.
The host socket and host CI cache were not mounted into this daemon. The
outer storage must be executable (`tmpfs ...:rw,exec,size=20g`); the initial
harness omitted `exec` and act installation failed with permission denied.
Correcting the harness mount, without changing Bosn's engine profile or
verification rules, allowed the same restore test to pass.

The proof covers:

- A later job restores the actual file saved by an earlier job.
- A second run restores actual file contents in a different engine.
- Different local repositories do not restore each other's archives, even
  when using the same key.
- A run claims a prepared spare, another spare is prepared, and daemon stop
  removes its owned engines/storage.
- Host-resource snapshots return to baseline after success, failure, timeout,
  client interruption and daemon interruption/restart.
- Recorded matrix/needs/failure execution produces the expected tree and
  bounded failure report.

These live scenarios use Bosn's **existing released act2 pin**, not the new
quota candidate. Act2 transfer/quota behavior is separately proven by its
package/process/race tests. The RAM-backed harness does not reproduce the
real host's disk-pressure cleanup failures in #445 or the 0.1.10 partial
create receipt in #452. It is also not a sustained footprint benchmark under
an integrated machine budget.

The isolated test-container IDs and socket-volume identity are recorded in
`/tmp/bosn-456-docker-harness.json` for exact cleanup/reuse. These are temporary
verification resources; none is a new production runner or cache contract.

## Required behavior

1. **Warm jobs.** Jobs in the same repository can reuse safe `actions/cache`
   entries, action checkouts and completed tool installs across runs and
   across fresh nested engines. Different repositories cannot read each
   other's `actions/cache` entries. Concurrent jobs cannot observe partially
   written cache entries. A newly created engine may be cold for its private
   image layers, but its reusable inputs must be hydrated from the shared
   store. Keep the private Docker socket boundary.
2. **Bounded disposable cache.** A configurable policy bounds `actcache/`
   by size and age. Eviction protects entries in use by any active engine,
   including engines of another Bosn daemon using the named volume. A pass
   removes exact selected entries and reports skipped protected bytes. It
   never clears the entire shared volume as a quota mechanism. Tools, runner
   tar, action checkouts and tool installs need explicit retention decisions
   and accounting so they cannot become an invisible remainder.
3. **Owned accounting.** Report measured bytes and counts for each shared
   cache component and repository namespace, active nested-engine storage,
   detached owned volumes, and host-side Bosn images and build cache where
   ownership can be established. Show largest contributors, retention reason,
   exact safe action, and whether measurement is partial. Never add shared
   layers and volume sizes into a false physical total. Continue reporting
   unowned artifacts through the existing `scan` path.
4. **Lifecycle and expiry.** Completed, failed, cancelled and interrupted runs
   eventually remove their exact nested engine and private storage. Cleanup
   failure remains visible and retryable. Old Bosn-owned images and
   containers are expired only after exact ownership, reachability and use
   checks. Build cache needs a separate policy: Docker's build-cache records
   lack safe per-record deletion in the current census, so any prune must be
   scoped and independently verified before it is automated. Retained
   stack/machine and pinned volumes keep the explicit-release contract.
5. **Pressure response.** A threshold warning identifies the owned class
   causing pressure and distinguishes reclaimable from intentionally retained
   bytes. Unknown sizes remain unknown; an incomplete read cannot authorize
   deletion. The warning points to a supported Bosn command, not a broad
   Docker prune.

## Implementation sequence and verification

| Step | Work | Evidence required | State |
|---|---|---|---|
| 1 | Inventory current caches, accounting and cleanup; record a host sample | Code paths, current tests, read-only audit | Complete (survey above) |
| 2 | Expose a typed breakdown for the shared cache and owned engine volumes | Focused RED to GREEN tests with accurate partial/unknown behavior | Shared cache implemented and tested in this branch; engine/retained-volume attribution open |
| 3 | Add age/size policy for disposable act cache data and active-use coordination | Concurrent live runs retain hits; over-limit idle data shrinks; no cross-repo reads | act2 byte limit and cross-process transfer RED to GREEN; policy settings and idle-server maintenance tested; offline and aggregate completed-archive maintenance implemented locally; Bosn integration and automatic warm cutover open |
| 4 | Account for and expire eligible old CI engines, host images and build cache | Fault/restart live Docker tests, exact ownership checks, repeated-run footprint trend | Existing lifecycle passes live end-state/restart tests; online retry implemented/tested; image/build-cache attribution/expiry and live failure replay open |
| 5 | Wire pressure diagnostics and verify sustained warm workloads | Repeated cold/warm benchmark plus disk growth under the configured ceiling | Open |

### Decisions to preserve

- Keep one named shared volume for cache data that must survive fresh engines;
  the host does not have a portable bind path into Docker Desktop's VM.
- Keep a private nested engine per run until a measured alternative preserves
  the socket/isolation and cleanup contract. Reusing a whole engine without
  a robust ownership boundary would let jobs inherit arbitrary state.
- Treat Docker's `reclaimable` estimate and `du` as different measurements.
  Report their provenance and avoid a misleading combined physical total.
- The existing `runners prune-cache` controls run records. Its name must not
  be presented as a shared cache eviction command.

### Still unverified

- A strengthened second-run test now verifies restored file contents in a
  different engine and passed in the isolated Docker harness (81.45 s for the
  cold/warm pair). The sequential-job restore test also passes. Concurrent
  cross-server eviction is covered by the act2 process test; end-to-end
  concurrent Bosn runs under a configured ceiling remain unverified.
- The isolated live suite proves normal lifecycle cleanup and interruption
  recovery on Docker 29.7.2. Delayed/failed control recovery and partial-create
  replay on the disk-backed host remain unverified; the active sample in #456
  is not a leak verdict.
- Which detached stack/machine volumes remain useful and which registry owns
  them; do not infer from names or attachment alone.

### Offline act2 maintenance progress (local candidate)

The act2 checkout now offers typed paginated `cache audit` and explicit
`cache prune --apply` for a namespace with no running server. A RED repro left
160 bytes above a 100-byte ceiling; GREEN leaves the newest 80-byte archive.
The audit creates nothing for absent stores and reports missing, busy and partial
states separately. It refuses unknown/untracked inventories before deletion.
Pages are capped at 12 entries and expose metadata/file-size fingerprints;
these are consistency checks, not archive-content hashes. Metadata, temporary
files and allocated-block accounting remain separate from completed-archive quota.

Artifactcache race tests and focused offline CLI tests pass. Updated artifactcache/CLI packages also pass pinned golangci-lint v2.11.4
with zero issues; the full CLI Docker result predates this extension. The candidate also reports bounded successful eviction receipts with typed
reasons, reclaimed completed-archive lengths, remaining/protected budget bytes
and an explicit budget-met flag. A protected over-budget namespace is not
reported as meeting its ceiling. Read-only mount proof passes on a disposable Docker volume: EROFS write probe,
accurate 160-byte inventory and byte-identical metadata. Sustained aggregate-budget
tests remain open. Legacy
servers do not honor the new transfer lock: Bosn still pins the released act2,
and this candidate must not evict mixed-version shared stores.

### Aggregate archive retention progress (local candidate)

Act2 now provides a cohort root lease: enrolled new namespaces share a root lock
for each server lifetime; aggregate maintenance holds it exclusively before
namespace locks/metadata. Live servers defer the aggregate pass, and new namespace
creation cannot cross a running pass. Existing legacy namespaces cannot silently
enroll. A new directory cohort is needed so released act2 peers never write it;
completed-archive import is now implemented locally, while automatic Bosn
quiescence/cutover remain open.

The aggregate pass validates all namespaces before deletion and refuses busy,
legacy, unknown, partial or oversized inventories. Up to 64 namespaces are
supported per pass, with bounded directory reads. Global UsedAt eviction preserves
recent-use protection and reports aggregate-budget receipts and protected overflow.
Two individually compliant 160-byte namespaces demonstrate the old aggregate gap
(320 bytes); the candidate global pass retains only the warmer repository's 160
bytes. Partial inventory in the last namespace preserves the first namespace.
The full artifactcache race suite, focused offline CLI/policy tests and pinned
lint pass for this slice. Namespace report ordering is deterministic.

This is an archive-length budget, not a physical machine footprint ceiling. Other
cache classes, metadata and temporary data remain accounted separately. Scheduled
Bosn integration/retry, automatic warm cutover, host image/build-cache expiry and sustained
concurrent workloads remain open; the production pin and cache path are unchanged.

### Warm migration progress (local candidate)

Act2 provides bounded completed-archive import from a quiescent legacy namespace
into a new cohort. The source metadata is held read-only; files and retention
timestamps remain unchanged. Recent completed archives are selected within an
explicit positive byte bound. The importer checks lengths, source/destination
SHA-256 and a final source inventory fingerprint, then publishes a synced staged
namespace by atomic rename under the exclusive cohort lease. Existing targets
are refused. Failed staging cleans only the exact generated directory or reports
its pending path; unpublished work has no successful import receipts.

A fixture with no transfer-coordination file demonstrates a real HTTP lookup and
download hit from a fresh candidate server after import. Source metadata stays
byte-identical and new reservations do not collide with imported IDs. Byte-bound,
overwrite/cancellation/corruption and changed-source staging cleanup cases are
covered. Import receipts are bounded to twelve details with an omitted count.
The full artifactcache race suite, focused offline CLI/policy tests and final
migration-specific race check pass; pinned lint reports zero issues.

This migration requires legacy servers to be stopped: holding their metadata
lock can fail active requests, and it cannot coordinate arbitrary file writers
outside that protocol. The CLI requires explicit quiescence acknowledgement;
Bosn must verify quiescence rather than treating that flag as evidence. Import
copies rather than removes legacy archives, so retained legacy bytes and temporary
duplication must be included in headroom. Bosn integration, automatic safe cutover,
retry, sustained workload and host image/build-cache expiry remain open.

### Online engine cleanup progress (local candidate)

Survey of current main found `CLEANUP_BUDGET = 180s`, so the older 60-second
failure in #445 is not the current literal budget. However, durable failed
retirements were retried only at startup; a running daemon could retain an engine
until restart. The candidate daemon now runs a separate cancellable retry worker.
Every pass scans at most 512 pending records and attempts at most one eligible
retirement, with a 200-second whole-pass deadline, then waits 60 seconds. A cursor carries across passes and wraps at
the end so early failures/active records cannot starve later cleanup.

Only registry `cleanup_required` records are eligible, and a tracked CI run must
already be done. Active claims are neither interrupted nor cleared. The existing
backend still verifies registry ownership and exact immutable container identity,
authorizes removal, requests `container rm --force --volumes`, and proves container
name/ID absence before recording a terminal receipt. Retrying retirement expires
that engine's private nested image/build-cache store. New v2 profiles require
independent named-storage absence proof, as described below; legacy anonymous
profiles retain their earlier removal contract. The named warm cache remains
protected. Successful retry updates the
run's cleanup field but preserves its original execution verdict and history.

All three isolated synthetic-host tests pass: recovery without daemon restart,
preservation of an active claim, and cursor fairness across failed retirement.
Strict Clippy for all bosn-service targets passes, as do source-length/include
checks. The timed daemon worker and disk-backed fault paths still need live proof. No disk-backed timeout or partial-create replay
has yet proved #445/#452 resolved on current main. Legacy anonymous-volume absence
is not independently recorded; the v2 named-storage slice below adds the required
volume reconciliation proof for new engines. Host base
images and shared cached image/tool artifacts also need separate expiry policy.

### Independent private-storage retirement (local candidate)

Evidence: anonymous disk storage names were observed only inside container
inspect responses. They were neither committed to the intent nor required in
terminal receipts. If `container rm --volumes` removed the container but left its
volume, a later container-absence probe could finalize cleanup without identifying
that storage. This loses the nested image/build-cache retirement evidence.

New disk profiles use `named_disk_storage_run_tmp_noexec_v2`: a local, labelled
`bosn-act-storage-<immutable intent UUID>` volume is created after the durable
intent commits and before container creation. Its scope is spec, retention warm,
with exact registry/run identity. The pinned shared cache remains separate.
Container declarations and observed mounts must bind the exact storage name.
Existing anonymous v1 profiles remain observable for recovery; new creation uses
v2 only. Older executables do not understand v2 profiles, so downgrade into a
registry containing them is unsupported.

Cleanup proves the container absent, verifies any remaining private volume's
exact labels/local driver/empty options, removes only that named volume without
force, and then proves its absence by a successful list. The registry requires
the exact storage identity in v2 terminal receipts, even when partial creation
never produced a container ID. An attached, foreign or pinned volume is refused.
A failed/unavailable probe is not absence; failed or uncertain removal stays
pending. Storage control commands each have a ten-second budget; retirement
reserves an additional forty seconds for them. Startup's pass budget is now
180 seconds so the required 135-second named-storage removal reservation can
fit; the former 120-second startup bound would always defer a present v2 engine.
The online retry retains its 180-second retirement and 200-second whole-pass bounds.

Transport tests cover unreadable inspect, attached volumes, lost acknowledgement,
reported success with surviving volume and later successful absence reconciliation.
The durable registry test rejects container-only/wrong-storage receipts and
verifies the correct identity survives reopen. The full isolated service library
suite passed 397 tests (four ignored), and all 18 registry lifecycle tests passed.
The focused bound-spare test passes, and strict Clippy passes for all service
and registry targets. Online retry now follows a spare's run binding for its active-run guard
and cleanup update, rather than looking up the original spare UUID as a CI run.

This provides a stronger expiry contract for new private nested image/build-cache
storage. Legacy anonymous storage still lacks independent survivor identity, and
host base images/shared cache artifacts need separate policy. Live Docker
fault/restart/partial-create replay is still required: these checks do not prove
late Docker-create side effects resolved or claim #445/#452 closed. No existing
host data was converted or removed; cache cohort integration remains open.

Live v2 verification passed all six scenarios in a fresh, isolated Docker
29.7.2 engine (457.26 seconds): a later job restored `actions/cache` from an earlier
job, a second fresh engine restored prior-run cache contents, repository
namespaces remained isolated, and spare reuse/daemon-stop cleanup succeeded,
all with v2 private storage enabled. Success, failure, timeout, client kill,
daemon kill/restart and the recorded matrix/failure report also passed. A final
volume listing contained only the retained `bosn-ci-cache-v1`, with no private
storage volumes. This proves those tested lifecycle paths, not partial-create
or lost-acknowledgement recovery against real Docker. The live isolation check now
also asserts that `/var/lib/docker` uses the intent-derived named volume; that
additional assertion passed in a focused lifecycle rerun (215.86 seconds).
Host snapshots already compare all owned
volume identities before and after retirement.

Shared-cache and private-storage inspection now deserialize the same typed
Docker volume response before ownership checks. The shared cache still accepts
a valid different registry's machine cache; local driver/scope, empty options
and identity labels remain mandatory. Ten focused accounting tests and the
new malformed-label/foreign-driver/bind-option boundary test pass in isolation.
Strict Clippy for all service targets and source-length/include checks also pass.

### Machine-wide owned volume breakdown (local candidate)

`bosn scan --json` now adds `owned_storage`, derived from the same read-only
Docker census. Valid label sets from every registry are included and grouped
as shared CI cache, private CI storage, or other Bosn volumes, with attached and
detached object counts. Malformed Bosn labels make the summary partial. Names
alone never establish ownership. Missing sizes keep the class byte total null,
rather than reporting a known subtotal as the complete footprint.

These are Docker's rounded approximate sizes, not allocated-block samples. They
must not be added to the shared-cache `du` result, because that would count the
same volume twice. This summary grants no deletion authority. The bounded
correlation slice below adds registry cleanup state. Legacy anonymous volumes remain
outside this labelled breakdown. The initial four aggregation tests pass.
Before the correlation slice, the CLI built and a read-only live scan succeeded
in JSON and readable modes: one detached retained shared-cache volume,
805,100,000 approximate bytes, zero private CI volumes, and no partial reads.
Final strict Clippy verification passes for all service targets, including the
readable-output change. Source-length/include and diff checks pass.

The next local slice adds up to 64 private-volume detail rows (largest/unknown
first, with an explicit omitted count). `scan` opens only its selected registry
read-only and correlates each matching owner/intent-derived volume with the
durable engine state and any spare run binding. Foreign registry, unavailable
registry, absent record, unreadable record and identity mismatch are separate
typed outcomes. Correlation is diagnostic and cannot authorize deletion. This
slice passed six focused isolated accounting tests, including persisted
cleanup-required state, unavailable/foreign/missing history, and bounded details
without losing object totals or hiding unknown sizes. Strict Clippy passes for
all service and registry targets after extracting volume classification into
its own function. All 18 registry lifecycle regression tests pass. The final
CLI built and a live read-only scan of a synthetic labelled private volume in
the isolated engine reported its exact intent/registry identity, detached status,
zero bytes and unavailable lifecycle history (not matched cleanup authority).
The synthetic volume was then removed; the retained cache was preserved.
The registry read-only handle now exposes the same bounded engine-record decoder
as the writer handle (one row, at most 32 KiB), including registry-owner checks.
The persisted cleanup-required correlation test verifies this read-only API.

### Concurrent shared-input staging (local candidate)

The act tarball and runner-image tar previously staged as `<path>.$$`. Shell
PIDs are container-local, so distinct engines can collide on the same shared
staging file. Both paths now use `mktemp` in the destination directory, followed
by validation/save and atomic rename. A normal shell exit cleans the unique
stage. Abrupt engine death can still leave a stage; stale-stage expiry and
coordination to avoid redundant concurrent downloads remain open. The focused
cache-script test passes. A live primitive probe ran two separate network-disabled
containers against one synthetic volume: both had shell PID 1 and the identical
legacy `/stage/input.1` path, while `mktemp` returned distinct shared-volume paths.
The synthetic containers and volume were removed afterward. This proves the
cross-container naming fix, not concurrent full artifact-download behavior or
crash-stage expiry. Strict Clippy for all service targets and source-length/diff
checks pass.

The next shared-input slice adds a per-artifact `flock` on a persistent
shared-volume lock file. The pinned Docker image provides BusyBox `flock`.
New engines take an exclusive lock before checking/downloading the act tarball.
Runner archives load under shared locks, allowing concurrent warm restores;
a missing/failed restore upgrades to exclusive and rechecks before pulling or
publishing. Refresh takes the exclusive lock before
removing an invalid tar. File descriptors release on process/engine death;
lock files must not be unlinked, which could split coordination across inodes.
Legacy executables ignore these locks. A live two-container probe verified that
an exclusive holder excludes another process, and killing the holder releases
the lock without deleting its file. The synthetic resources were removed. Full
artifact-level concurrency/failure verification and final checks remain pending.
An isolated test now executes the production runner-cache shell with a
deterministic Docker transport: two cold callers issue exactly one pull/save;
two warm callers overlap their loads and issue no pull/save. It passes in
0.88 seconds. This verifies shell coordination with mocked Docker operations;
live artifact correctness and failure/stale-stage cleanup remain open.
The concurrency test now uses a warm-reader barrier instead of timing sleeps.
A failed-save injection reproduced a false success: the shell reached image
inspection after publication failed. Runner and act publication chains now
explicitly exit on failure, before consuming or reporting the cached input.
The failure test also checks stage cleanup and subsequent lock reuse; it went
from RED to GREEN and passed in 1.32 seconds after the explicit failure exits.
Final strict Clippy for all service targets and source-length/include/diff checks
pass. A real fresh-engine cache-restore rerun is the next verification gate.
