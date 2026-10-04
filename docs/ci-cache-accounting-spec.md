# CI cache and Docker footprint: living spec

This document records the verified behavior of Bosn's act2 engine and the
remaining work to make repeated CI runs fast without unbounded disk growth.
Update the survey, implementation table and verification evidence in the same
change as each implementation step. The desired behavior below is a contract,
not a claim that it is already implemented. Related issue: #456.

## Current implementation status (2026-10-04)

Accounting/lifecycle foundation PR #473 merged as `b23398ae`. It adds
bounded owned-footprint diagnostics, private-storage identity/reconciliation,
and coordinated cleanup deadlines. Actual private-Docker tests prove shared
archive hits across jobs and successive fresh engines; they do not prove
sustained physical-footprint convergence. No new Bosn release is claimed.

The retention candidate is in `../bosn-extern/bosn-retention`. It includes typed
policy/arguments/import evidence and participating legacy-session leases.
The execution path now carries a typed legacy/cohort route; production planning
still explicitly selects legacy, and rejects configured retention.
Production planning still refuses configured retention: warm migration, old-peer
exclusion, durable routing and independent watcher supervision are unfinished.
Tools/images/actions/tool-install expiry and host owned-image/build-cache
pressure control remain open; successful private-engine retirement removes
that engine's nested containers, images and build cache. Historical entries
below record intermediate states and must not be read as current rollout status.

Act2 v0.2.89-act2.7 is released from
`1a6782d5bbabb715f425cad5601b1f706fc23fa8` (merged PR #28). Exact-commit
full CI 37190653388 and release run 37191477610 passed. The Linux x86_64
archive SHA-256 is
`61d640112af87278075c70cd67412ec9c44ceffd1e269e88632d50d4a83ebd27`;
the extracted binary SHA-256 is
`328147f59cc101aa86cc826ecb5e7b2493f74765d520d4809997b2a9c6ba56bd`.
Published checksums match, and the isolated binary reports
`act version 0.2.89-act2.7`. The candidate pin selects these verified bytes.
Four pin consistency tests and the actual published audit/watch command contract
passed; strict service all-target Clippy passed (28.59 seconds).

This release includes the act2.6 empty-cutover fix and historical publication
receipts. Bosn's typed parser checks exact namespace identity, historical and
current byte ceilings, retained-source warmth, fingerprints and bounded archive
receipts. Four isolated boundary tests passed; parsing historical evidence
cannot authorize enrollment. The exclusive import executor preserves partial
publication on nonzero exits but is not yet invoked by production planning.
A local registry journal now persists import intent and recovers historical
publication after restart. It is not machine-wide routing or enrollment state.
The candidate also executes bounded independent maintenance passes in durable
finite-lived helpers and consumes typed retention outcomes. Participating cohort
workflow engines and maintenance helpers now require the same shared policy
record. A callable cancellable periodic supervisor is now implemented; production
startup policy discovery and enrollment remain unfinished.

The tool-cache candidate uses unique save stages and skips hidden unfinished
stages when rehydrating fresh engines. Its actual pinned-engine shell proof
passed, alongside deterministic concurrency and completed-install tests. These
changes are not yet shipped on Bosn main; policy activation and storage ceilings
remain open.

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
Cleanup errors identify the container for recovery. Each create now carries a
unique UUID name and nonce label. A failed or invalid acknowledgement triggers
typed discovery; identity and isolation must match before removing an immutable
ID. Successful removal requires Docker's explicit absence verdict. Discovery
that is absent, unreadable, malformed or mismatched remains pending, with the
helper name in diagnostics. The service now commits a durable helper intent
before create and retries uncertain cleanup through the existing maintenance
worker; the verification scope and remaining crash-replay gap are below.
Its read-only cache mount and masked engine data volume create no build cache.

Verification: the two initial focused tests failed before the implementation
(missing allocated bytes/components, missing partial flag), then passed.
Real sparse-file measurement, malformed/duplicate records, Docker outage,
public runner error reporting and failed-helper cleanup are covered by tests.
Eight helper transport tests pass, including a replay where real Docker creates
the helper but a wrapper discards the acknowledgement. Recovery removes that
verified helper; a subsequent real measurement succeeds with nonzero allocated
bytes and leaves the shared cache intact. Tests also reject successful remove
responses without observed absence and preserve mismatched helper identities.
The replay uses the isolated private Docker engine, not the host Docker data.
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
| 2 | Expose a typed breakdown for the shared cache and owned engine volumes | Focused RED to GREEN tests with accurate partial/unknown behavior | Shared-cache apparent/allocated breakdown and machine-wide labelled volume attribution implemented/tested locally; bounded private-volume lifecycle correlation verified; per-private-volume allocated-block samples and legacy anonymous attribution open |
| 3 | Add age/size policy for disposable act cache data and active-use coordination | Concurrent live runs retain hits; over-limit idle data shrinks; no cross-repo reads | act2 byte limit and cross-process transfer RED to GREEN; policy settings and idle-server maintenance tested; offline and aggregate completed-archive maintenance implemented locally; Bosn integration and automatic warm cutover open |
| 4 | Account for and expire eligible old CI engines, host images and build cache | Fault/restart live Docker tests, exact ownership checks, repeated-run footprint trend | Named private storage requires independent absence proof; six live lifecycle scenarios and focused mount identity pass, expiring nested image/build-cache storage; online retry verified locally; host base/shared artifacts, legacy anonymous survivors, real partial-create/lost-ack replay and sustained growth checks open |
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
pass. The real fresh-engine cache-restore rerun passed in 47.25 seconds with
the new lock/publication shell against the pinned Docker image. It verifies
cache contents restored across distinct engines; concurrent cold live hydration
and interrupted-stage expiry remain open.

### Repeated act2 warm/retention workload (local candidate)

Eight cycles now exercise imported warm data, fresh HTTP servers, cold namespace
growth and aggregate maintenance. Each adds 160 cold bytes; active servers defer
GC, then idle maintenance stays within a 160-byte completed-archive ceiling while
preserving the imported 80-byte warm archive. A final fresh server restores it.
The race-enabled workload/import tests pass (1.176 seconds), and pinned Go lint
reports zero issues. This verifies act2 handler/file behavior, not Bosn's automatic
maintenance loop or a physical machine ceiling. Namespace metadata and retained
legacy-source bytes remain outside this archive cap; automatic cutover remains open.

### act2 review: bounded inventory and interrupted deletion (local candidate)

Review found that `filepath.WalkDir` materialized whole directories before the
advertised entry/time checks. Inventory now reads 256 entries per page, with one
shared 100,000-entry ceiling and a 64-directory depth ceiling. Focused tests cover
oversized directories, cancellation and excessive depth.

Review also found that interruption between archive removal and metadata deletion
could permanently block offline maintenance. A typed deletion intent is now
committed before file removal; cache metadata and intent are deleted in one final
transaction. Offline preflight accepts missing archives only with a matching intent.
Ordinary audit remains partial until recovery; unknown missing files still refuse
maintenance. Cohort recovery starts only after every namespace passes preflight.
Recovery receipts count zero newly reclaimed archive bytes when the file is already
absent, and a second pass emits no duplicate deletion. Lookup preserves missing-file metadata so it cannot orphan the intent before
recovery. Interruption, intervening lookup and cohort deferral tests pass; the full
cache package passes race testing (17.315 seconds),
and Go lint reports zero issues. These changes remain unshipped and do not change
Bosn's released act2 pin or close the automatic maintenance/cutover gaps above.

### Automatic act2 shutdown retention (local candidate)

An explicit `--cache-server-cohort-max-bytes` policy now invokes aggregate
maintenance after the cache server stops and releases its cohort lease. Active
peers/transfers defer collection; the last normal shutdown retries under the
existing five-second operation limit and full namespace preflight. The typed
`RetentionOnClose()` outcome and `cache_retention` log field distinguish partial
or deferred work from completed collection and protected overage. Repeat close
does not repeat maintenance. The CLI closes on function exit, including watch
exit and early errors, and keeps cleanup independent of workflow cancellation.

RED: shutdown left 240 completed archive bytes under a 160-byte ceiling. GREEN:
eight cycles add cold data, restore the warm 80-byte archive through fresh
servers, and each shutdown leaves at most 160 bytes without an explicit prune.
The cache package passes race testing (16.969 seconds); active-peer deferral,
last-peer retry, protected overage, cancellation and lease release are covered.
CLI policy/offline checks and lint cover the new flag and context wiring.

This closes the normal act2 shutdown-to-maintenance handoff, not the host retry
or Bosn integration gap. Killed servers and deferred/protected work require host
maintenance. Bosn still pins the released build without this flag. The policy
remains opt-in; physical machine bytes, metadata and retained legacy inputs are
outside the completed-archive ceiling. Warm cutover and automatic Bosn retries
remain open.


### Durable accounting-helper recovery (candidate, not released)

Accounting requests now commit a typed helper intent through the sole registry
writer before Docker create. The existing event ledger stores Pending, Created
and Removed states; no second database or schema migration is introduced.
Created records retain the immutable container ID before start or removal.
Identity freezes the image digest, cache volume, nonce and ownership labels.
Recovery verifies these plus the read-only mount and container isolation before
removing that exact ID. Successful receipts require observed absence. Missing
name lookup after uncertain create remains Pending because create may arrive
later; known-ID absence can finish a Created record.

An in-process active guard is installed before intent visibility and released
on return or cancellation. The maintenance worker skips active measurements,
pages at most eight batches of 64 records, and attempts one inactive helper per
pass. Its helper cursor is independent of engine retirement and advances past
deferred records. Engine and helper passes each have a 200-second outer budget;
the worker sleeps 60 seconds after both passes, so this is not a fixed
60-second cleanup deadline. Output paging is bounded; internal SQL scan cost
and accumulated completed ledger history are not a physical storage ceiling.
Shared volumes and foreign or mismatched containers are never removed.

Verification in the isolated Rust container: registry library tests passed
(5), service library tests passed (414; 6 ignored), and the focused accounting
and helper suite passed (20, including its real-Docker replay). Registry tests
close and reopen the writer between transitions and verify immutable IDs,
owner checks, nonce reuse refusal and paging past completed history. The real
private-engine replay discards a successful create acknowledgement and makes
recovery inspection unavailable, leaving a persisted Pending intent and an
actual orphan. A fresh backend recovers the verified ID from that ledger,
removes it and records Removed. A subsequent tracked measurement returns
positive allocated bytes with the shared cache intact.

This proves durable state and backend replacement recovery. Actual daemon
SIGKILL during helper creation and a timed background-worker recovery replay
remain unverified. Engine partial-create failures in #452 are separate; helper
recovery does not establish their resolution. Bosn still pins the released
act2 without the candidate aggregate retention policy, so this change does not
yet establish automatic machine-wide byte control.

### Supervised act2 aggregate retry (local candidate)

The candidate now adds `cache prune-cohort --apply --watch 1m`, emitting a JSON
report for each bounded pass and retrying busy or incomplete outcomes after
the interval. This provides a maintenance process independent of cache-server
shutdown, including stores left behind by crashed servers. It uses existing
exclusive cohort leases and refuses incomplete or legacy inventories before
deletion. Single-pass failures retain their error exit; invalid intervals and
policies fail before maintenance. Cancellation stops future passes.

Focused CLI race tests turn the missing --watch repro GREEN and verify repeated
partial reports plus recovery after an actual running handler releases its
root lease. The retry lease test uses an empty store; byte convergence is still
supported by the separate eight-cycle archive workload. Bosn does not yet
launch or supervise this command, and its released act2 pin lacks it. Safe
machine-wide warm migration, old-peer exclusion, headroom and physical storage
admission remain open. This is a tested maintenance mechanism, not evidence of
automatic Bosn byte-budget enforcement.

The watcher now also has a retained-data workload proof: real metadata/storage
fixtures contain 240 completed archive bytes with shutdown retention disabled.
An age-only pass leaves them intact; the watched CLI's 80-byte aggregate budget
removes the eligible cold entries and leaves the recently used warm entry.
A fresh server retrieves that entry and its exact bytes through loopback HTTP.
The focused watcher race tests and pinned lint pass. This closes the earlier
empty-store-only verification limitation for watcher byte collection; it still
does not prove actual SIGKILL handling or Bosn supervision.

### Candidate handoff and graceful cancellation

The act2 implementation is published for review as draft
[act2 PR #22](https://github.com/zackees/act2/pull/22), head `8a2e84f`.
The complete candidate diff and the graceful-interrupt follow-up passed the
single-reviewer local gate. A fresh full artifactcache race run passed in
16.778 seconds; focused CLI policy/offline/watcher race tests and pinned lint
also passed. A candidate CLI was built inside the isolated Go container.
Sending its maintenance watcher the first real SIGINT after its initial JSON
report produced a successful exit. The watcher now combines the CLI's graceful
job context and force context through the existing EarlyCancelContext helper;
the focused repro previously required force cancellation and now passes.

Latest published act2 remains `v0.2.89-act2.3` as checked during this handoff.
The draft introduces no version bump or release. It is not merged, not a
verified release artifact, and not a Bosn pin update. Bosn policy arguments,
independent helper supervision, verified warm migration and physical pressure
control remain required integration work.

### Owned-volume warnings in doctor (local Bosn candidate)

`bosn doctor` now renders owned-volume footprint warnings from its existing
machine census, before unmanaged warning/acknowledgement handling. No second
Docker scan is added. It uses the existing default thresholds (5 GiB or 25
objects) across the nonoverlapping owned volume classes. Partial accounting or
unknown sizes also warn when owned or invalid-labeled volumes are observed.
An unavailable empty census does not claim an owned footprint.

Warnings show each observed class's Docker approximate bytes and attachment
counts, link users to `bosn scan --json`, and direct shared-cache owners to
`bosn ci runners cache` for actual block accounting. They preserve retained
volume release contracts and convey no removal authority. They remain on
stderr, so `doctor --json` stdout keeps its existing report shape. An unmanaged
acknowledgement does not suppress the independent owned warning.

Two focused RED warning signals became GREEN; all eight owned-accounting tests
pass in the isolated Rust container. Strict Clippy for all service targets,
source-length/include gates and slice review pass. This is a visibility
improvement, not a free-space pressure controller or automatic owned GC. No
new live host measurement was required for this warning change.

### CI coverage scope for act2 PR #22

The existing PR workflow's Windows and macOS lanes select only
`TestRunEventHostEnvironment`; their success is host-environment coverage,
not execution evidence for the newly added retention, transfer-lock or import
tests on those operating systems. The snapshot lane builds the release target
matrix without publishing. Linux runs `./...` with coverage and a 20-minute
per-package timeout, then CLI smoke steps. Its test step was confirmed live
on job `111376046334` in run `37181922413`; no replacement run was started.
Lint, spelling, snapshot and both host-environment lanes had completed
successfully at that observation. Linux completion and end-to-end Bosn quota
integration remain unproven until their corresponding evidence exists.

### Verified CI snapshot available for integration experiments

Downloaded Linux amd64 artifact `11295491838` from act2 run `37181922413`.
The extracted binary SHA-256 is
`ae923c1fdba91a95b358218e767b3f8e82085574de6e127704be34aabfd9af45`.
Its build information identifies clean revision
`52a1ce794a4cdbcd0983b1fdf92e1bf0a628f0cc`, Linux amd64, CGO disabled,
and version `0.0.0-SNAPSHOT-52a1ce7`. GitHub's commit record confirms that this
is the PR merge snapshot with parents `60c84bbf` (current master) and `8a2e84f`
(the reviewed candidate). It is not the PR branch SHA itself.

The actual downloaded binary was executed only in the isolated Go container.
It exposes the quota/cohort/watch flags, emits a typed partial report for a
missing maintenance root without creating that root, and exits successfully
on its first real SIGINT. This provides a provenance-checked executable for
local integration experiments while Linux CI continues. The artifact is a
CI snapshot, not an approved release or permanent download pin; Bosn's
released artifact/version checks remain unchanged. Warm workflow execution
with this binary and automatic Bosn policy/supervision are still unverified.

### Warm migration headroom admission and native publication

Act2 PR #22 now includes commit `0ae797d`. Import measures destination
caller-available free bytes before archive copying and refuses unknown or
insufficient space. The additional-space estimate is selected apparent
archive lengths plus 64 KiB per archive and 64 MiB metadata headroom. Reports
expose retained source archive bytes separately: when both stores use one
filesystem, its current available space already accounts for retained data.
This estimate is not a reservation against concurrent writes or an exact
allocated-block budget, and does not implement Bosn's machine pressure floor.

Focused RED refusal/accounting cases now pass. Failure-path tests leave source
metadata and archives unchanged, remove the generated stage, and publish
nothing. Native probes use Linux/macOS filesystem available blocks and Windows
GetDiskFreeSpaceEx; other platforms explicitly refuse unknown space. Linux
import race tests pass, the artifactcache race suite passed in 16.874 seconds,
pinned lint reports zero issues, and Windows amd64/macOS arm64 cross-builds
pass. Local review passed after fixing an exposed Windows publication bug.

Windows now uses same-volume MoveFileEx with WRITE_THROUGH without replacement,
after file/metadata close. Unix retains rename plus parent-directory sync and
truthfully preserves Published after rename even on a later sync failure.
Power-loss durability on arbitrary Windows filesystems is not established.
Existing host CI jobs now include TestImport* to execute these native paths;
no workflow files or runners were added. All CI checks passed for the previous
8a2e84f candidate; the new head's native and full CI remain pending. Bosn warm
cutover, old-peer exclusion and maintenance supervision remain open.

## Native Windows coordination correction (candidate)

CI run 37182945345 at act2 `0ae797d` passed full Linux tests, macOS
import tests, lint and snapshot builds. Windows import tests failed before
publication: bbolt attempted to truncate an intentionally read-only
coordination descriptor while acquiring an exclusive lease.

Windows now validates the existing bounded coordination database read-only,
then takes a native exclusive LockFileEx lease over exactly bbolt's lock byte.
Shared leases retain bbolt's read-only locking protocol. No writable descriptor
or creation is permitted by inspection. Close releases the lock and handle
idempotently; acquisition failure returns a nil lease. Unix retains its
existing bbolt protocol.

Read-only file preservation, shared/exclusive exclusion, separate-process
transfer protection and import tests pass with the race detector in isolated
Linux Docker (2.124 seconds); pinned lint reports zero issues. The previous
full artifactcache race suite passed in 16.844 seconds after correcting a
typed-nil lease regression. Windows package cross-build passed. Existing host
CI now selects the lease and separate-process transfer tests as well as import
tests. Review found no concrete protocol defect; native Windows execution of
this correction remains pending. This does not complete Bosn policy wiring,
maintenance supervision, warm cutover or machine-wide physical budgeting.

## Warm-cutover store accounting (candidate)

The integration destination layout is `actcache/cohort-v1/<repository hash>`,
separate from retained legacy `actcache/<repository hash>`. The class and volume
totals already contain both. Namespace diagnostics now additionally enumerate
the cohort layout and expose optional `store_path`, relative to the shared
volume, so equal repository hashes cannot hide the retained source or import.
The CLI shows that path and remains compatible with older replies lacking it.
Top-256 namespace limits apply across both layouts; these separate non-atomic
samples must not be added to the inclusive class/volume total. This path is
accounting provenance, not deletion authority or proof of cohort enrollment.

An actual shell measurement in isolated Docker was RED (only one namespace
reported for two same-repository stores), then GREEN. The fixture checks both
paths, distinct apparent/allocated measurements, and exclusion of a symlinked
namespace. Malformed/uppercase cohort hashes remain partial. Targeted accounting
tests pass: 17 passed, two live-Docker cases intentionally excluded from this
slice. Strict all-target bosn-service Clippy also passes. Source length/include gates
pass and the single reviewer passed this
slice; full Bosn branch review remains unclaimed.

Act2 `578ce2b` native Windows and macOS jobs in run 37183643367 passed,
including imports, read-only lease exclusion and separate-process transfer
protection; lint also passed. Full Linux and snapshot jobs were still running
when sampled. This supersedes the pending native correction result above.
Bosn still pins the older released act2: no automatic migration, new-root
server routing, retention flags or maintenance supervisor is enabled yet.

## Command-level warm-cutover verification (candidate)

The integration test `TestCacheCutoverCommandsPreserveFreshServerHitsAndRepositoryIsolation`
uses actual loopback HTTP reservation, upload, commit, lookup and download. Two
stopped source servers have the same cache key/version but different 80-byte
payloads, under separate 16-digit repository identities. Cobra import commands
copy each store into `actcache/cohort-v1/<repository hash>` with an 80-byte import
bound; source metadata remains byte-identical. Three successive fresh servers
per repository download the correct distinct payloads after import.

A command-level aggregate pass with an 80-byte ceiling correctly reports
160 remaining bytes, 160 protected bytes and BudgetMet=false: both archives
were recently transferred. This verifies truthful protected overflow, not age
expiry or sustained convergence. Linux Docker race testing and pinned command
lint pass. Servers have immediate failure-path cleanup plus explicit close
before import. The existing native host CI selection includes this test; no
workflow/job/runner was added.

Production integration still requires a verified released binary pin, typed
machine policy, durable routing/cutover state, source quiescence across daemons,
and supervised maintenance independent of run teardown. Proposed command sequence:
`act cache import --apply --source-quiescent --from SOURCE --namespace HASH
--max-bytes IMPORT_BOUND --cache-server-path COHORT`; start servers with
`--cache-server-path COHORT/HASH --cache-server-cohort-root COHORT`, explicit
namespace byte/age policy and optional close-time aggregate policy; supervise
`act cache prune-cohort --apply --watch 1m --max-bytes AGGREGATE_BOUND
--cache-server-path COHORT` with the same namespace byte/age policy.
The source-quiescent flag is caller responsibility, not a detector of old
peers. This test does not authorize automatic source deletion or prove those
Bosn daemon paths are implemented.

## Other owned-volume contributors (candidate)

`bosn scan --json` now exposes `other_volumes` and `other_volumes_omitted`
within its existing owned-storage report. Each contributor identifies the volume,
registry, workspace, stack, generation, scope, retention, attachment state and
approximate Docker bytes. The 64 largest or unknown contributors remain visible;
class counts and size semantics still cover all observed objects. These are
not allocated-block measurements and cannot be added to shared-cache `du` totals.

A typed advisory inspection hint distinguishes registry-history investigation
from the existing read-only manifest durable-release preview for stack/machine
or pinned data. Preview must use the owning registry/workspace and is itself
responsible for establishing any eligible token. Labels, names, detached state
and this contributor list establish no deletion authority; foreign/untracked
and pinned data are preserved. No cleanup command was added or executed.

A RED test showed retained bytes lacked any contributor detail. After the fix,
ten owned-accounting tests pass in genuine isolated Docker. Strict all-target
Clippy and source length/include gates also pass. Tests verify
identity/retention visibility, unknown-first ranking, a 71-object/64-detail bound,
unchanged totals and foreign-registry handling. The single reviewer passed this
slice; full branch review remains unclaimed.

CI run 37183643367 for act2 `578ce2b` ultimately ended cancelled: Linux was
automatically superseded by the later test commit, not proven passing. Its
Windows/macOS/lint/snapshot jobs passed. At current `7902785`, run 37184103814
passed native Windows/macOS (including command cutover tests) and lint; Linux
and snapshot were still running at the latest sample.

## Full candidate review and schema correction

The single reviewer covered the full 45-file Bosn range through `224990a3`
against `146515cf`; no blocking ownership/replay/concurrency defects were found.
Full unit verification then caught a stale published schema for optional
`CacheComponent.store_path`: 417 service cases passed, one schema case failed,
and six cases were ignored. The earlier review assertion of schema parity was
incorrect. An actual generated schema fixes that mismatch; its comparison test
now passes. Registry library tests (five), workspace formatting and strict
all-target service Clippy pass.

A tiny stdout exporter, `soldr cargo run -q -p bosn-service --example
export_ci_schema`, generates the contract inside an isolated container without
writing its read-only source mount. The host can validate/copy its JSON output
into `docs/ci.schema.json`. No weakening of the schema parity gate was made.
These results precede reconciliation with newer main changes and do not prove
the merged candidate; current main is `5a5040fa`. Full verification after that
reconciliation remains required before pushing a candidate.

## Main reconciliation and exact-source gate

Merge `1527ee9` incorporates main `5a5040fa`, including unresolved-create
quarantine and authoritative source/run receipts. Startup recovery first
requires main's container absence proof, then independently removes/verifies
private storage before finalization. Both old and new test modules are retained.
The single reviewer passed the merge follow-up. The merged service library
suite passes: 429 cases, six ignored, no failures.

Main updates the production act2 pin to `0.2.89-act2.4`; that release still
does not contain this retention candidate. Act2 candidate `7902785` now has
all checks successful in run 37184103814: full Linux, native Windows/macOS,
lint and snapshot build. The earlier cancelled run is not substituted for it.

The current source-bound pre-push gate requires released Bosn >= 0.1.12 and
actual Rust/Linux workflow receipts. Host global Bosn is 0.1.10; an isolated
`uvx --from bosn==0.1.12` environment resolves and reports 0.1.12 without
replacing the user's global executable. Running the full gate/stamping the
candidate is the next required step before pushing this Bosn branch.

## Actual gate cleanup failure on released Bosn 0.1.12

The clean-source gate at `7179b6e` used isolated released Bosn 0.1.12
(act2 0.2.89-act2.3) and its own registry/state. Rust run
`e9701619-7ebb-4d51-8a91-c1001ce38b6e` passed with executed-step proof
in 489 seconds and reported cleanup removed. Linux run
`d58306d1-8da5-4af1-bff8-90046524e57a` completed all required workflow
steps successfully (act exit 0): Install, Lint, and Test. Python results
were 257 passed / 10 skipped; live Docker acceptance requires explicit opt-in.
The overall run nevertheless ended error: engine cleanup failed with
`Docker CLI exceeded its deadline`. The gate correctly refused attestation
and exited 1 after 1114 seconds; this branch was not pushed.

A subsequent exact-ID Docker inspection reported the Linux engine absent.
That late observation does not rewrite the failed run or prove independent
private-storage absence. No broad prune or user-daemon restart was performed.
This is current live evidence extending #445 beyond its previously observed
0.1.10 scope. Candidate online retry/named-storage behavior still needs to be
validated against this workload before calling the failure resolved.

Both successive fresh engines restored cached inputs. The Linux run restored
a roughly 693 MB archive; the Rust run saved tools in 0.3 seconds. Linux
native install prepared packages in 3m27s and uv reported a cross-filesystem
hardlink fallback to full copying. This demonstrates reuse plus a remaining
copy cost; it is not sustained convergence or a machine-wide physical cap.
Bounded raw observations and gate receipts remain in task-owned Git metadata
and logs. A draft PR body records scope and gaps, but push awaits a valid gate.

### Cleanup deadline diagnosis

Release tag v0.1.12 resolves to `08c7c8cdb262b980d56fd9fccbe9b6156fc6ec82`.
Its exact engine deletion uses the generic 30-second `docker_control` helper.
The candidate retains that per-command bound despite the 180-second outer
lifecycle budget: increasing the outer budget alone cannot extend deletion.
Candidate named-volume deletion likewise uses a 10-second storage-control
bound. The successful workflow followed by cleanup timeout and later engine
absence therefore exposes a concrete remaining deletion-budget gap, including
possible delayed completion after a caller timeout.

Next correction must distinguish short observation/create control operations
from potentially slower exact authorized deletion, account for all reserved
commands in startup and online budgets, and test a delayed removal that exceeds
the old bound. Timeout still leaves durable pending state; only exact container
and storage absence may finalize retirement. The existing named-storage/online
retry changes do not by themselves prove this deadline failure resolved.

### Coordinated deletion budgets (candidate; verification in progress)

The isolated process/registry regression now includes a 31-second authorized
container deletion. Against the old helper it failed (0 passed / 1 failed),
confirming the per-command bound independently of the outer lifecycle timeout.
The candidate separates exact deletion (90 seconds) from container observation
(30 seconds) and storage observation (10 seconds). Startup reserves 155 seconds
for legacy retirement or 275 seconds for named-storage retirement, including
independent absence probes and persistence. Lifecycle/startup/online retirement
allow 360 seconds; the online pass wrapper allows 380 seconds. Accounting helper
budgets remain independent. All values share one budget module.

Timeout continues to leave pending cleanup; exact absence remains mandatory.
These bounds do not guarantee disk-pressure convergence or resolve an arbitrarily
slow Docker removal. The container regression turned GREEN (1 passed); the private-volume regression
passed with an 11-second removal, exceeding its previous 10-second bound. Strict
service all-target Clippy passed. The existing reviewer found no blocking issues.
The full isolated service suite passed: 429 tests, 6 ignored, 0 failed.
A new exact-source workflow gate remains required before shipping this correction.

### Retention integration boundary (in progress)

Act2 PR #22 merged as `7e6f010fa7cc7171083329b2cf3d18f1566e3468`.
Full checks on that exact merge SHA are running in run 37186796787; no new
tag/release or Bosn pin update has been made. Typed `[cache]` configuration now
requires explicit positive repository/aggregate archive byte ceilings and
positive maximum-age, unused-age, and maintenance intervals in seconds. It
rejects unknown fields, inconsistent ceilings and act2 duration overflow.
These settings govern completed archive lengths, not a physical machine cap.

Production planning currently refuses configured retention with an explicit
rollout error. Activation remains dependent on a verified release pin, safe
warm import into the coordinated root, and maintenance supervision. Existing
legacy shared-cache execution remains the default. This preparatory boundary
does not claim that configured quotas are yet enforced.

### Live lifecycle verification at b0860ff

The candidate CLI built in the genuine isolated Rust harness. Against the
private Docker engine, `the_host_engine_is_unchanged_after_every_way_a_run_can_end`
passed (1 test, 282.13 seconds). After the warm-up baseline, exact owned-container,
network, volume and engine-image inventories returned to baseline after success,
failure, timeout, client SIGKILL and daemon SIGKILL/restart. The timeout scenario
verified intent-derived named storage, volume-only mounts and a nested Docker
engine distinct from its parent. Shared cache inputs persisted across the fresh
engines. This validates candidate lifecycle cleanup under these workloads, not
archive retention, sustained physical convergence or cleanup under full-disk
conditions. The separate exact-source gate still uses released Bosn 0.1.12.

The policy boundary test passed; both config tests passed, and strict service
library Clippy passed in the isolated container. Review caught a spare-planning
bypass; run planning, spare kickoff and spare planning now use the same
`load_engine` guard. The config test proves parsed policy is refused at that
boundary; it does not directly instrument backend creation counts. The existing
reviewer passed the corrected slice. Retention activation remains incomplete.

### Coordinated command mapping (candidate, not activated)

Bosn now constructs act2 server and independent watcher arguments from the
typed policy. Both use `actcache/cohort-v1`; repository namespace parsing only
accepts a 16-character lowercase hexadecimal direct child. Server and watcher
share the same byte/age settings. The watcher passes an explicit aggregate
ceiling and interval, rather than relying on server shutdown for idle cleanup.
These command builders are not yet invoked by production admission. Warm import,
legacy-peer exclusion, receipts, supervision and verified pinning are still
required before removing the planning guard. CLI contract validation is pending.

The two existing live Bosn cache tests passed at b0860ff (2 tests, 100.65
seconds): a later job restores an earlier job's archive, and a subsequent run
with a different engine ID restores and verifies the saved bytes. This proves
existing shared-cache reuse, not yet quota enforcement. Full act2 checks run
37186796787 passed on merge SHA 7e6f010, including Linux, Windows, macOS, lint
and snapshot. Dependency release approval is being prepared; no tag exists yet.
The namespace test and strict service library Clippy passed for the command
builders, and review verified flag mapping. Execution of those builders remains
unverified and production activation stays guarded.

### Dependency release and source-gate checkpoint

The exact-source Bosn gate at b0860ff passed Rust (477 seconds) and Linux
(485 seconds), reusing Python-static and passing guards. It exited successfully
and stamped the candidate as `445265cfa97e` after 962 seconds. This gate covers
the accounting/lifecycle candidate; it does not cover the later retention wiring.

The user approved act2 v0.2.89-act2.5 and established standing authorization
for necessary act2 releases in AGENTS.md. The annotated tag points to
`7e6f010fa7cc7171083329b2cf3d18f1566e3468`, whose full CI passed. Existing
release workflow run 37187501368 is running. Binary/checksum verification and
Bosn pin updates remain pending; no production retention activation is claimed.

### Verified act2.5 dependency pin (candidate)

Release run 37187501368 passed and published v0.2.89-act2.5 from the exact
full-CI-verified merge SHA 7e6f010. The Linux x86_64 archive matches published
checksums (`d116cc9e5ca040f4807763f562476275015e9d9bdf5c3a2ac598c182c0be0a9e`).
The extracted binary digest is
`ea682586fbf32aef140cfd2dc620d883be29f14650daadd6d1cd29ed34292ea1`;
its actual isolated invocation reports `act version 0.2.89-act2.5`. Bosn's
version, archive URL and both digests now pin these verified bytes.

The command contract test passed against the exact-merge snapshot: namespace
audit reports missing with null counts/bytes; missing-root watcher reports
partial/unknown budget outcome and stops successfully on its first SIGINT.
The initial fixture mistakenly expected missing audit to be partial/nonzero;
actual execution and review corrected the test, not the production contract.
Strict service-library Clippy passed. The published-binary contract test passed (1 test, 0.05 seconds). Production policy activation remains guarded pending warm migration
and supervision. Accounting/lifecycle PR #473 merged as
`b23398aef9f9c12eb314e0835ca7a508391a07fd` after required checks passed;
full hosted macOS/release checks were not requested or claimed.

### Warm migration refuses a budget-induced cold cutover (act2 follow-up)

Inspection and a focused RED test exposed an act2.5 import gap: two completed
80-byte source archives with a 79-byte import ceiling published an empty
cohort destination. The retained source survived, but destination existence
then prevented a warmer retry. This conflicts with warm migration.

The act2 candidate now refuses before staging/publication if completed
archives were skipped for budget and none fit. Tests prove destination absence,
unchanged source metadata, a successful 80-byte retry and actual HTTP restore
of the retained archive. Genuinely empty-source initialization remains allowed.
Import race tests passed (1.177 seconds), and pinned lint reported 0 issues.
This follow-up is not yet released: Bosn's act2.5 pin still has the old edge
case, and production enrollment remains guarded. A corrected dependency release
and verified pin are needed before automatic migration can be enabled.

### Typed warm-import boundary (candidate, not activated)

Bosn now builds source-quiescent import arguments using the validated repository
hash, legacy source path, fixed cohort root and repository byte budget. Calling
this builder requires prior legacy-writer exclusion; it does not establish that
exclusion. Production planning remains guarded.

Import JSON is eagerly decoded into typed reports and receipts with a 64 KiB
input bound. Parsing checks schema, exact source/destination identity, policy
ceiling, at most twelve receipts, unique positive archive IDs, lowercase SHA-256
digests, count/omission consistency and checked byte totals. A separate warm
publication check requires complete publication, no pending stage/error, known
adequate headroom, retained-source evidence and a nonempty import when the source
is populated. Published-but-partial output remains visible for reconciliation;
it is not converted into absence or permission to blindly repeat import.

Three isolated boundary tests passed (0.01 seconds), strict service-library
Clippy passed, and the existing reviewer found no blocking issues. These tests
use typed receipt fixtures: they do not prove actual automatic migration,
durable migration receipts, peer exclusion or supervision. Act2 cold-cutover
correction is tracked in PR #27; full CI is still running.

### Participating legacy cache lifetime lease (candidate)

Workflow and act-list invocations now acquire a shared FD 8 lock on
`/bosn/cache/actcache/.legacy-migration.lock` before executing act. The descriptor
survives exec and spans the server lifetime; OS process death releases it.
Independent readers can run concurrently. New cached engine profiles freeze
`SharedLegacyLeaseV1` and advertise the corresponding coordination label.
Historical profiles omit the optional field, preserving their identity digest.

The isolated process test passed (0.11 seconds): two readers coexist, exclusive
migration is refused while either lives, killing the last reader releases the
lease, and literal arguments survive the shell boundary. Engine tests passed
(19), registry lifecycle tests passed (18), and spare tests passed (5). The
existing historical digest assertion passed unchanged. Strict service
all-target Clippy passed.

This coordinates participating writers only. Older or unknown peers have no
proven exclusion; instant inventory absence cannot prevent their future
admission. Automatic migration remains guarded. Real workflow execution with
the new wrapper, durable migration receipt reconciliation and watcher
supervision remain unverified. Act2 PR #27 merged as
`d874c3b6e9518e11c3137da8dd810f1f3f361db1`; exact-merge full CI run
37188971746 is in progress, so the correction is not yet released.

The private-Docker `ci_live` warm-cache tests subsequently passed with the
new lifetime wrapper and frozen profiles (2 tests, 61.07 seconds): a later job
restored an earlier job's archive, and a successive run restored it through a
distinct fresh private engine. This verifies continued warm legacy sharing;
it does not verify cohort migration or retention-policy activation.

### Verified act2.6 pin and historical receipt boundary

The published act2.6 CLI contract test passed (1 test, 0.16 seconds), as did
four pin/proof consistency tests. The dependency is verified; production cache
policy remains guarded. Four typed import/receipt boundary tests passed and
strict library Clippy passed. Historical receipt validation shares the same
checked archive count/digest/byte logic as import reports, binds both source
and destination to the repository namespace, and requires a valid source
fingerprint, original import ceiling, current repository ceiling and known
retained-source warmth. It refuses populated-source empty evidence.

The record is creation-time evidence only: current inventory, durable routing,
publication reconciliation and old-writer exclusion remain caller obligations.
This parser is prepared for act2 PR #28 and is not invoked against act2.6,
which does not include the receipt command. Independent review found no blockers.
Issue #456 now records merged accounting work and exact dependency release
evidence while leaving the full acceptance criteria open.

The actual private-Docker workflow tests also passed with the verified act2.6
pin (2 tests, 140.91 seconds): a later job restored an earlier job's archive,
and a successive run restored saved bytes through a distinct fresh private
engine. These exercise artifact verification and the legacy lease wrapper in
real runs. They establish continued warm reuse, not policy activation or
sustained image/container/build-cache footprint convergence.

### Exclusive import lifetime and truthful command transport (candidate)

Bosn's import executor now uses a nonblocking exclusive FD 8 legacy-session
lease across the actual act process. Participating live sessions refuse import
immediately; an importer excludes participating readers until process death.
The OS releases the lease on termination. Older peers still need independent
exclusion, and the caller must persist intent before invoking this method.
Production planning remains guarded.

Import execution has a 90-second command deadline and a 64 KiB output bound.
It parses stdout even on nonzero exit, preserving published-but-partial evidence
instead of discarding it through a generic checked-command helper. Command exit
and publication report are separate typed facts: warm acceptance requires exit
zero plus complete valid warm publication. Transport/parsing failure leaves
publication unresolved; it is never converted into destination absence or
permission to blindly repeat import.

Two isolated synthetic transport tests passed (0.09 seconds), including a
nonzero exit with published/partial output and contradictory success evidence.
The actual process lease test passed (0.55 seconds): readers coexist, import
refuses contention, importer excludes readers, and termination releases both
lease modes. Strict service all-target Clippy passed. Independent review found
no blockers. These tests do not prove actual automatic migration or supervision.

Act2 receipt PR #28 merged as `1a6782d5bbabb715f425cad5601b1f706fc23fa8`
after full PR CI passed. Exact-merge full CI run 37190653388 is in progress;
the receipt feature remains unreleased and absent from the act2.6 candidate pin.

### Tool-cache save isolation and efficient fresh-engine seeding (candidate)

A focused RED test found that the old whole-root `cp -a` seeding copied hidden
`.saving-*` directories into every fresh engine. A second RED test replayed
equal PIDs from separate private engines with a controlled copy barrier: both
publishers used the same `.saving-$$` path. One could remove or reuse another's
unfinished stage, despite their different install destinations.

Save now creates an exclusive `mktemp -d` stage for each install, with one
shared shell setup function. Cleanup targets that invocation's own stage. Seed
copies visible published top-level entries and skips hidden stages/control
files. Sibling completion markers, inner stamps and existing warm entries
retain their existing behavior. The collision fixture terminates only its
spawned process groups, including blocked copy children on failure.

Four actual shell/copy tests passed (0.52 seconds), including both corrected
RED signals and the two existing completion conventions. Strict service
all-target Clippy passed (71.37 seconds), and independent review found no
blockers. This proves deterministic equal-PID isolation and avoids private-disk
duplication of unfinished saves; it does not introduce tool-install TTL/quotas,
abandoned-stage pruning or improve every pre-existing copy-failure path.
Real engine workflow coverage of this new seed/save slice remains pending.

### Verified act2.7 artifacts and pinned-engine tool-cache shell

The act2.7 release workflow 37191477610 succeeded on exact merge commit
`1a6782d5bbabb715f425cad5601b1f706fc23fa8`, whose full CI run 37190653388
also succeeded. Published archive checksums and extracted binary identity were
verified before changing the candidate pin. Four pin tests passed (0.01 seconds),
and the published audit/watch CLI contract passed (0.12 seconds). This command
test does not yet verify Bosn parsing an actual historical receipt.

The actual pinned engine image's shell executed the generated tool-cache save
and seed scripts successfully (1 test, 0.23 seconds). The helper had a read-only
root, no network, dropped capabilities and only disposable tmpfs cache/work
paths. It restored both sibling and inner completion conventions while excluding
a hidden orphan stage. Its immutable container ID was confirmed absent afterward.
This verifies the BusyBox shell path; actual workflow tool-install activity,
expiry and quotas remain unverified. Strict service all-target Clippy passed
after the shell test and new pin (28.59 seconds). Independent review passed.

### Actual Bosn import transport and published historical receipt

The isolated private-Docker proof passed using published act2.7 through
`DockerActBackend::import_cache_for_quiescent_source` (1 test, 2.83 seconds).
A Go HTTP fixture reserved, uploaded and committed an 80-byte archive, then
closed its legacy server before exporting the source. The test imported that
source through the actual exclusive lease wrapper and required exit zero plus
complete warm-publication evidence. It then called the published historical
receipt command and parsed its output through Bosn's `PublicationReceipt`.
The receipt identified 80 imported bytes; source file names and SHA-256 hashes
matched before and after import. Exact helper-container absence was verified.

The helper mounted only disposable tmpfs paths, had a read-only root, no network
and dropped capabilities. File streaming uses Bosn's existing bounded transport;
failed operation checks return through exact cleanup. Docker auto-removal and
a finite helper lifetime also cover a lost acknowledgement or panic. Initial
fixture errors concerned container absence wording, directory enumeration and
the absolute act binary path; they are not production RED signals. Independent
review passed after fixing unbounded fixture streaming and panic-before-cleanup.

Reproduce with the opt-in test
`published_binary_import_preserves_source_and_has_typed_historical_receipt`,
`BOSN_ACT_RETENTION_TEST_BINARY` pointing at the verified release,
`BOSN_ACT_IMPORT_SOURCE_TAR` containing a closed HTTP-seeded legacy namespace
`0123456789abcdef`, and the isolated Docker transport. This verifies actual
command/schema interoperability and source preservation. It does not establish
automatic enrollment, old-peer exclusion, crash-safe routing, watcher supervision
or restored workflow hits from the new cohort. Those remain activation gates.

Strict service all-target Clippy passed after the final import fixture changes
(60.84 seconds). The proof and review are complete for this slice; the full
source gate is still required before pushing the retention candidate.

### Durable local import intent and historical publication recovery

The registry stores a bounded typed migration record under a namespace-specific
indexed `meta` key, without a schema migration or second database. An immediate
transaction commits the nonce, namespace, original import ceiling and creation
time before import. A second begin is refused even for the same nonce; an
unresolved import must be reconciled rather than repeated blindly. Publication
contains the historical source fingerprint, imported count/bytes and retained
source bytes. Conflicting publication never replaces the recorded evidence;
identical recovery is idempotent. Rollback leaves no intent.

Bosn's recovery method reads the actual act2 historical receipt with a 30-second
deadline and 64 KiB output bound. It requires an existing local intent, exact
legacy/cohort namespace paths, complete typed receipt checks and the original
import ceiling. Missing, malformed or failed reads leave the intent unresolved.
Successful evidence is committed in an immediate transaction. This call does
not rerun import or delete source data.

Two isolated registry tests passed (0.44 seconds), covering rollback, reopen,
duplicate refusal, conflicting nonce/fingerprint, invalid warm byte evidence,
invalid time and idempotent recovery. The actual private-Docker published-binary
proof passed (1 test, 4.28 seconds): missing receipt first remained unresolved,
then a closed HTTP-seeded 80-byte archive was imported, the command acknowledgement
was discarded before journaling publication, and the registry was reopened.
Recovery recorded the actual act2.7 receipt and repeated safely. Source hashes
remained unchanged and exact helper-container absence was confirmed. Independent
review passed.

This is local recovery evidence, not daemon-wide automatic startup reconciliation
or machine-wide routing. The act2 historical receipt does not carry Bosn's nonce,
so this journal does not prove correlation to a unique import invocation. Before
enrollment, callers still need current inventory, publication durability, shared
routing state and exclusion of older writers. The production configuration guard
remains in place. No automatic migration or physical storage ceiling is claimed.

Strict registry/service all-target Clippy passed (50.64 seconds) after correcting
a collapsible conditional flagged by the first lint attempt. The task Docker
engine also reported no remaining import-proof helper containers. The retention
candidate remains local pending the exact-source gate and broader activation work.

### Typed routes reach actual workflow execution

`ActInvocation` now carries a typed `CacheRoute` instead of an arbitrary namespace
string. Legacy routes use the validated direct-child repository path; cohort
routes pass the shared root, repository/aggregate byte ceilings and age/interval
flags through the existing policy builder. There is exactly one cache-server
path in the execution argv. The production planner validates its derived
namespace and selects `Legacy` explicitly; it cannot infer enrollment from a
local journal or historical receipt. Retention configuration remains guarded.

The published act2.7 CLI accepted the full workflow planning argv for both routes
(1 test, 1.12 seconds). Four engine invocation tests, eight parameter tests and
fourteen lifecycle tests passed. These preserve runner pinning, overlay selection,
secret handling, execution claims and cleanup behavior.

An isolated real owned-engine proof then passed across two fresh engines
(1 test, 42.43 seconds). The first run saved an `actions/cache` archive and
a later job restored its file contents. A successive run restored those bytes
through a different private engine using the same explicit cohort route. Both
lifecycle reports proved cleanup, and the task Docker engine had no remaining
run containers. This exercises actual execution through the new route, not only
argument generation. The final three-engine extension also passed (1 test,
73.01 seconds): a different namespace using the same cache key reported no
hit and had no restored file. All three engines had distinct immutable IDs
and successful lifecycle cleanup. Independent review passed.

This fixture creates new coordinated namespaces; it does not migrate a live
legacy repository or authorize production enrollment. Shared machine routing,
old-peer exclusion, independent watcher supervision, other cache classes and
sustained physical-footprint convergence remain required.

Strict service all-target Clippy passed after the final three-engine fixture
(7.71 seconds). This routing slice is verified locally; the exact-source gate
is still required before pushing the retention candidate.

### Bounded idle-cohort maintenance and truthful outcomes

Bosn now builds a single-pass maintenance command from the same typed policy
used by the watch command, with no workflow server required. The executor
captures at most 64 KiB with a 30-second transport deadline and parses stdout
even when the command exits nonzero. A valid partial report remains visible;
transport or invalid-output failure means unknown outcome, not zero reclamation
or proof that no deletion occurred. The caller must separately supervise and
retire the verified helper, including remote execution after transport failure.

The typed report binds the exact cohort root and aggregate ceiling, validates
namespace identities and repository ceilings, and bounds stores/pages/receipts
to 64/12/32. Receipt counts and bytes use checked arithmetic and unique IDs.
Complete reports need known totals and complete namespace evidence. Stable
remaining bytes must equal namespace retention totals. Protection is sampled
separately at root and namespace level, so each value is bounded independently;
these samples must not be summed or asserted equal across time. A complete pass
with `budget_met=false` is distinct from an incomplete/unknown pass. These are
logical completed-archive bytes, not allocated filesystem blocks.

Four isolated boundary tests passed, including a protection-boundary RED/GREEN
regression. The first parser rejected a legitimate complete report whose archive
crossed act2's five-minute protection boundary between root and namespace
samples; validation now accepts those independently bounded measurements. An
actual published-binary run also exposed a mistaken full-path namespace
assumption: act2 emits basenames within its root-bound report. That candidate
parser mismatch was corrected without changing the dependency protocol.

The final idle-expiry/replay proof passed against published act2.7 in genuine
private Docker (1 test, 1.20 seconds): no workflow/cache server was alive, an
old imported archive expired, exactly 80 bytes and one deletion were reported,
current archive bytes became zero, and a second pass reported no new reclamation.
The legacy source hashes stayed unchanged and exact helper absence was verified.
Independent review passed; strict service all-target Clippy passed (17.82 seconds).

This is a callable bounded pass, not an enabled periodic supervisor. Shared
machine policy/routing, durable helper creation/recovery, restart scheduling,
old-peer exclusion and production enrollment remain required. The configuration
guard stays. No expiry for other shared cache classes or sustained physical
footprint ceiling is claimed by this proof.

The actual nonzero transport path was then verified against published act2.7:
a missing cohort returned a nonzero partial report with unknown byte totals,
and Bosn retained that typed outcome. The expanded proof passed (0.96 seconds);
strict all-target Clippy passed again (6.48 seconds). This closes command-exit
handling for the tested missing-root case; it is not crash-supervisor coverage.

### Durable maintenance helper identity (2026-10-04 candidate)

The existing helper journal now accepts an explicit `maintenance_v1` role.
Historical measurement records omit the role and keep their previous names and
serialization. Maintenance records use a distinct name, nonce label and ownership
scope. Recovery requires their exact pinned image, immutable container ID, isolated
profile and single writable named-volume mount at `/bosn/cache`; accounting
helpers still require a read-only mount at `/cache`. Unknown roles are rejected.
The shared cache volume is not authorized for deletion by either role.

Isolated Docker verification passed all four registry helper tests and the new
service profile test. These cover historical JSON, durable role recovery after
registry reopen, unknown role rejection, cross-role mismatch, wrong destination,
read-only maintenance mounts and extra mounts. Independent review passed.

This establishes recovery identity only. Maintenance create/start orchestration,
offline verified act installation, finite helper lifetime and periodic/restart
scheduling remain unfinished. Production enrollment remains guarded.

Strict registry/service all-target Clippy passed (20.89 seconds), after moving
the new test module to the end of its file to satisfy the existing lint gate.

### Bounded durable maintenance helper (2026-10-04 candidate)

`maintain_cache_with_helper` now drives one independent pass through an owned
helper. It verifies the existing shared volume, commits a maintenance intent
before create and registers the immutable ID before start. It rechecks the
volume and helper profile before executing. The helper has no network, no
capabilities, a read-only root, 128 MiB memory and one CPU; its writable private
executable tmpfs is limited to 64 MiB. A finite 300-second lifetime with Docker
auto-removal bounds abandoned execution after start. A never-started create
still requires durable journal recovery; no auto-removal claim applies to it.

Act installs offline from the machine archive under its artifact lock, verifying
both archive and extracted binary digests and the version. There is no download
fallback in this helper. An absent or corrupt archive is a visible error rather
than permission to discard cached data. The normal workflow installer and this
helper share one archive-path builder. The callable pass does not bootstrap a
missing archive; supervised scheduling must address that explicitly.

The returned helper result separates maintenance outcome from cleanup outcome.
Cleanup re-verifies ownership before exact-ID removal and requires explicit
absence plus durable journal completion. An acknowledged ID that has already
auto-removed can finish after explicit absence. An unacknowledged create remains
pending when its outcome cannot be observed. Existing online/restart helper
recovery understands the new role. The named shared volume is preserved.

The actual published act2.7 pass succeeded in genuine private Docker (1.09
seconds): complete typed maintenance evidence, zero expired fixture bytes under
a long-age policy, exact helper absence, journal Removed state, and preserved
owned shared cache. Independent review passed. The real lost-create-acknowledgement and temporary inspection-outage proof
also passed (0.78 seconds): a fresh backend recovered the pending intent,
verified and removed its exact container, committed Removed and successfully
measured the preserved shared volume afterward.

This is bounded orchestration, not an enabled periodic/restart scheduler.
Production policy consensus, legacy-peer exclusion and repository enrollment
remain incomplete and guarded. Image/build-cache pressure and expiry for other
shared cache classes remain open; logical archive retention does not prove
physical machine footprint convergence.

Strict service all-target Clippy passed (18.52 seconds), after extracting the
Docker creation profile to satisfy the existing function-length gate.

### Participating machine policy agreement (2026-10-04 candidate)

Cohort workflow execution and independent maintenance now agree on a fixed
schema-1 record at `actcache/.bosn-cohort-policy-v1` in the shared machine volume.
All validated policy fields are compared in a fixed numeric representation.
A nonblocking exclusive lock serializes publication; a unique staging file and
hard-link publication prevent overwriting an existing agreement. Conflicts
return exit 78 and preserve the old record; contention returns 75. No policy
change API or automatic replacement exists. Legacy routes do not claim this
record. The agreement happens before cohort act execution or pruning.

The isolated shell proof passed: initial publication, agreement from a new
process, conflicting aggregate ceiling refused and original bytes unchanged,
with no staging-file leak. Actual private-Docker helper verification also passed
with a compatible policy and refused a conflicting policy while still retiring
both helpers and preserving the shared volume. Review passed.

This coordinates participating callers only. It does not enroll namespaces,
recover a deleted agreement, exclude old/nonparticipating writers, prove record
provenance against arbitrary cache-volume writes, or establish power-loss
persistence. Losing/replacing this record while existing servers run is not a
supported policy transition. Production activation remains guarded. A trusted
scheduler still needs policy discovery/bootstrap, duplicate-pass coordination,
restart/cancellation supervision and durable reporting before rollout.

Strict service all-target Clippy passed (24.65 seconds). The actual cohort
workflow proof then passed through the new agreement path (97.45 seconds):
a later job restored saved bytes, a distinct fresh engine restored them again,
and a third repository namespace using the same cache key stayed isolated.
All three owned engines retired and the private Docker daemon had no remaining
fixture containers. These participating workflow and maintenance tests use the
same machine policy (100 MiB repository, 200 MiB aggregate, 30-day maximum age,
seven-day unused age, 60-second maintenance interval). This is not a production
machine-policy migration or contention/retry throughput proof.

### Callable periodic maintenance supervisor (2026-10-04 candidate)

`supervise_cache_maintenance` runs independently of workflow engines. It starts
with an immediate bounded helper-recovery pass, then an independent maintenance
helper pass. Recovery has a 200-second budget and carries its fair cursor between
ticks. A maintenance attempt has a 600-second outer budget, including pinned-image
availability and the existing bounded helper operations. Started helpers retain
their 300-second finite lifetime. Recovery errors do not erase maintenance
outcomes or permanently suppress all subsequent passes.

Each tick reports recovery and maintenance separately, preserving maintenance
outcome versus cleanup status. A bounded report channel applies backpressure:
the supervisor cannot launch further passes while the consumer is stalled.
After reporting, it waits the configured interval before another pass. A fresh
supervisor has no stale in-memory next-run timestamp and attempts immediately.
Shutdown cancels recovery, execution, reporting or sleep; incomplete helper work
remains in the durable journal for the established reconciliation path.

This is a callable supervisor with an explicitly supplied trusted policy, not
production daemon wiring or an enrollment API. Reports are typed; a bounded latest snapshot is now persisted before delivery
(see the evidence below), rather than an unbounded maintenance history. Machine-wide duplicate-supervisor coordination,
policy discovery/bootstrap, cancellation during individual Docker operations,
old-peer exclusion and production admission still require work. The existing
configuration guard stays in place. The actual idle periodic/restart proof
passed in private Docker (73.75 seconds): two ticks with distinct helper IDs,
complete typed maintenance outcomes, exact helper absence, stop during the
interval and immediate restart pass within its 15-second observation budget.
The shared volume survived and no fixture containers remained. Independent
review passed. This does not test cancellation during create/exec or establish
production daemon wiring.

Strict service all-target Clippy passed for the supervisor (24.83 seconds).

### Participating maintenance exclusion (2026-10-04 candidate)

The actual maintenance command now opens
`actcache/.bosn-maintenance-v1.lock` in the shared machine volume and acquires
an exclusive nonblocking lease on FD 6. It then execs the verified act binary
with unchanged literal arguments. The descriptor spans the command lifetime;
process/container death releases the lease. A competing participating pass
returns exit 75 with `machine cache maintenance busy` before invoking act.
The bounded maintenance adapter preserves that diagnostic as an outcome error,
not as successful reclamation or a complete empty report. The helper still
retires and its durable journal finishes. The scheduler reports the contention
and tries again on a later tick.

The genuine private-Docker proof passed (2.24 seconds): two independent backend
instances shared the same volume; one owned helper held the lease, a competitor
was refused and cleaned up, killing/removing the exact holder released the
lease, and the competitor's next pass completed. The shared volume survived.
The fixture cleans its holder before asserting operation errors; existing helper
identity verification and explicit absence gates are used. Review passed.

This excludes participating maintenance commands, not helper creation itself,
nonparticipating older binaries or workflow cache servers. Act2's cache-server
root/store locks remain a separate boundary. It does not establish production
policy discovery/enrollment, durable maintenance outcome history, or cancellation
during an uncertain Docker operation. Those rollout requirements and broader
cache-class/image/build-cache expiry remain open.

Strict service all-target Clippy passed for maintenance exclusion (12.04 seconds).

### Durable latest maintenance accounting (2026-10-04 candidate)

The supervisor now commits a typed latest snapshot to its local registry before
delivering each tick. The fixed meta key has a 4 KiB read/write ceiling and
replaces the prior snapshot, so report retention cannot itself grow without
bound. Registry and read-only registry readers can retrieve it after restart.
This is the latest committed observation, not a machine-wide cumulative history.

Observed snapshots retain command exit status, partial status, logical archive
budget, nullable remaining/protected bytes and budget outcome. Total reclaimed
archive bytes are computed with checked arithmetic only for complete root
reports whose namespace retention data is all known. Partial or missing totals
remain unknown. Transport/protocol errors use an explicit unknown outcome;
cleanup and orphan-recovery errors remain separate. UTF-8 diagnostics are capped
at 128 characters (at most 512 bytes). A failure replaces old totals rather than
presenting a prior successful snapshot as current evidence.

Helper references must match the registry's existing maintenance-role nonce and
immutable container ID. Claimed cleanup success requires its durable journal
state to be Removed. The trusted actor performs the transaction; no new client
wire operation grants receipt or deletion authority. Snapshot persistence errors
are reported separately without discarding maintenance/cleanup outcomes.
Cancellation during an already-enqueued registry operation can leave its commit
acknowledgement unknown; readers still see only committed snapshots.

The isolated registry restart/conflict test passed (0.22 seconds): premature
cleanup success and a different container ID were refused; reopening preserved
the valid snapshot; a later unknown transport outcome replaced it and survived
read-only reopening. Independent review passed. The actual periodic/restart
proof with committed snapshot lookup passed in private Docker (64.33 seconds):
two idle ticks, stop/restart, persistence acknowledgement before each delivered
report, and a read-only latest snapshot matching the restarted helper ID.
All fixture containers retired and the shared cache volume survived.

This stores bounded local latest evidence. It does not aggregate snapshots from
multiple registries, preserve cumulative reclaimed-byte history, retain all
namespace receipts, expose a new user-facing maintenance-status command, or
prove physical block reclamation. Current helper journal reconciliation may
advance after a snapshot; its earlier cleanup error remains historical evidence.
Production startup policy discovery/enrollment and broader image/build-cache
and shared cache-class expiry remain open.

The registry boundary also rejects a partial root carrying a claimed total
reclamation value. The expanded registry test passed (4.61 seconds), accepting
an unknown reclamation total for that partial report. This closes the typed
snapshot invariant even for another future trusted caller.

Final registry/service all-target Clippy passed (31.42 seconds), including the
partial-reclamation guard and latest snapshot integration.

### Current-main integration and promotion gate (2026-10-04 candidate)

The retention branch was rebased onto main `219ee2b1` (Bosn 0.1.14 preparation,
verified desktop widget and raw task-stream persistence). The configuration
conflict was resolved by preserving both desktop opt-in UI installation and
retention admission checks. The existing desktop test now includes a cache
policy, checks that installation preserves it and verifies that installing the
UI cannot bypass the enrollment guard.

The accumulated candidate is being prepared for a PR through the repository's
clean exact-source local gate. The gate must replay both the Rust and Linux
workflow lanes through released Bosn, validate executed-step/source receipts,
and stamp tree-bound attestations before push. Focused private-Docker proofs
above establish their stated behavior; they do not substitute for this promotion
gate. No push, merge or release is asserted by this entry. A promoted candidate
still needs production policy discovery/bootstrap, repository enrollment and
old-peer exclusion; broader cache-class and host image/build-cache expiry remain
open. The existing production guard will stay until those requirements are met.

The first promotion gate correctly refused the candidate at Rust formatting
(run `d52a8b2a-e1cc-4367-ac5e-eaf4b3a35696`, engine cleanup Removed). It found
an extra blank line left when the helper test module was moved. Python static
and repository guards passed; Rust tests and Linux tests were not claimed.
The changed Rust files were formatted again before replaying the clean gate.

The second gate (`33f51278-d38e-4018-a2e4-24f590db7a37`, cleanup Removed)
passed format, Clippy and kernel boundary checks but failed the 50-submission,
10-key concurrency test: its roughly five-second observation window expired
with a run still Running before an engine ID was recorded. The private Docker
service suite reproduced the failure (457 passed, one failed, 15 ignored),
while that test alone passed in 2.14 seconds. Its burst-specific observation
budget is now bounded at 30 seconds, and it additionally requires every run's
conclusion to be Success; timeout/error completion cannot satisfy the test.
With this change the full private Docker service suite passed: 458 passed,
zero failed, 15 ignored, 35.70 seconds. Production run deadlines are unchanged.
This is evidence of load-dependent test observation, not a proof of production
latency. The exact-source promotion gate still needs a successful replay; its
Linux lane did not run in either failed attempt.

The third gate (`6dbc5fe7-c19e-472e-b7b6-ac1e32f48ac8`, cleanup Removed,
481 seconds) passed format, Clippy, boundary checks and the service suite,
including the corrected burst test. It then failed an embedded Python fixture:
two setup submissions exceeded its 250-millisecond timing assertion
(`crates/bosn-python/src/tests.rs:494`). The failure needs investigation before
the next replay. No successful promotion or Linux-lane coverage is claimed.

Private-Docker investigation found that fixture's timer included spawning its
Python thread and attaching to the interpreter before either submission call.
An instrumented isolated sample measured the two API calls at 10.40 milliseconds
and passed; this does not identify the precise timing of the failed gate sample.
The fixture now measures the API calls after attachment, retaining their
250-millisecond bound and the isolated process's ten-second outer deadline.
Coalescing, raw-log and cancellation assertions remain. This test adjustment
does not alter production behavior or establish end-to-end startup latency.
All seven embedded Python tests passed in private Docker (0.85 seconds), and
independent review passed for the measurement adjustment.
Python all-target Clippy with embedded-test features also passed (57.62 seconds).

The fourth exact-source gate (`5847ae1f-0d10-4e5d-b415-854a9b043cce`,
375 seconds, cleanup Removed) again passed the service suite but failed the
embedded Python submission assertion: the two calls themselves took 410.23 ms.
Excluding interpreter attachment did not resolve the gate failure. Submission
latency and the fixture's actual asynchronous-admission contract need further
investigation; no successful promotion or Linux-lane receipt is claimed.

The current-thread runtime experiment did not resolve the failure (private
fixture measured 254.86 ms) and was reverted. Code inspection shows setup-ensure
awaits a durable SQLite submission audit before acknowledgement, unlike the
other fake submission cases. Eight WAL/FULL commit samples in the genuine Docker
harness took 292.66–605.72 ms on `/tmp` disk storage and 0.014–0.040 ms on
`/dev/shm`. These samples demonstrate that storage latency alone can exceed the
fixture's 250 ms bound; they do not attribute every millisecond of the failed
gate sample. The Linux protocol timing fixture now owns a RAM-backed temporary
directory, retaining the SQLite audit, 250 ms API check, coalescing and subsequent
log/cancellation assertions. Other platforms retain their original temporary
storage. Disk-backed production acknowledgement latency remains unbounded by
this timing proof; ordinary durable registry and service tests are unchanged.
All seven embedded Python tests passed with this fixture (0.33 seconds), and
independent review passed. The exact-source promotion replay is still required.
Embedded-feature Python all-target Clippy passed (44.81 seconds), and Rust
format checks passed for the integrated status reporting and timing fixture.

### Cancellation at the Docker create acknowledgement boundary

A follow-up private-Docker proof cancels the actual maintenance helper future
after Docker creates its container but before the client returns the immutable
ID. The durable intent remains Pending without an acknowledged container ID,
and cancellation releases the backend's active-helper guard. A fresh backend
recovers the exact helper from its frozen labels/profile, commits Removed,
confirms the immutable ID absent, and measures the shared cache successfully.
The first version of this proof passed in 60.21 seconds. Its wrapper deliberately
withholds the acknowledgement for 60 seconds; that total includes runtime
shutdown waiting for the outstanding command. It does not establish prompt
command cancellation or prompt daemon shutdown. The expanded proof separately
bounds maintenance-future cancellation return to five seconds and checks the
unacknowledged journal shape and explicit immutable-ID absence. That expanded
proof passed (one test, 63.29 seconds): maintenance-future cancellation returned
in 72.197 microseconds. The deliberate client command still delayed fixture
shutdown, so prompt daemon shutdown during an outstanding Docker command remains
unverified. Production startup wiring remains open.

Final service all-target Clippy passed (61.42 seconds) after splitting the
proof's cancellation operation from its recovery fixture. Independent review
passed for the proof and that extraction. The verified follow-up is now included
in the next promotion candidate; the exact-source gate has not passed.

### Maintenance evidence in cache accounting (follow-up candidate)

`bosn runners cache` now requests the fixed latest-maintenance row through the
trusted registry actor's read path, without entering a write transaction. Its
typed reply distinguishes no recorded pass, unavailable registry evidence and
a recorded unknown or observed outcome. The CLI prints the last observation's
Unix timestamp, inventory completeness, command exit code, logical archive
budget result and reclaimed archive bytes; unknown totals stay unknown. Helper
recovery and cleanup failures remain separate. Raw helper diagnostics, nonce
and immutable container ID are omitted from this public summary.

This is the latest evidence in the queried daemon's registry, not a current
machine-wide inventory, cross-registry aggregate, active-supervisor indicator,
cumulative reclamation counter or physical disk convergence guarantee. An old
daemon's reply omitting this optional field still decodes. Default production
planning still uses warm legacy caches and rejects configured retention; this
read-only visibility change does not enroll a repository or start maintenance.

Focused private-Docker maintenance tests passed (seven tests, five explicitly
ignored live prerequisites, 4.43 seconds), including unknown diagnostic
redaction and partial/protected-over-budget evidence with unknown reclamation
and failed cleanup. The published typed JSON Schema was regenerated and its
equality test passed. All-target service Clippy passed (31.05 seconds), and
independent review passed for the Rust, JSON Schema and documentation changes.

### Read-only participating policy discovery (follow-up candidate)

The backend can now discover the canonical shared policy through the existing
journaled helper lifecycle. The helper mounts the owned cache volume read-only,
uses a shared nonblocking policy lock and never creates a record or lock. A
missing volume or record returns explicit absence; malformed, oversized,
unsupported, noncanonical or busy records return an error. Discovery does not
authorize defaults, bootstrap policy, enroll repositories or exclude older
daemons. Production routing and supervisor startup remain open.

Two focused tests passed, including absence without filesystem changes and
canonical policy validation. The real private-Docker discovery test passed in
0.42 seconds: it read the agreed 100 MiB repository / 200 MiB aggregate policy,
retired its helper and left no pending helper intent.

Promotion gate c0819217-e6e3-401a-b403-5d29030cb74f failed after 367
seconds; cleanup was Removed. Format, Clippy and boundary checks passed. The
service suite had 459 passes and one failure in the setup admission timing
fixture. That fixture also awaits the SQLite audit commit, so the follow-up
uses Linux memory-backed storage while retaining the 250 ms protocol assertion,
durable events, coalescing, cancellation and restart assertions. This does not
prove a disk-backed production acknowledgement SLO. The 20 focused setup
tests passed in 0.71 seconds. Accounting and helper recovery tests passed
(20 tests, four explicit live prerequisites ignored, 0.34 seconds), format
passed and final all-target service Clippy passed in 7.03 seconds. Independent
review passed. The integrated candidate still requires its exact-source gate.
