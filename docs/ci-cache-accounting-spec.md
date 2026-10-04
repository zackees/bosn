# CI cache and Docker footprint: living spec

This document records the verified behavior of Bosn's act2 engine and the
remaining work to make repeated CI runs fast without unbounded disk growth.
Update the survey, implementation table and verification evidence in the same
change as each implementation step. The desired behavior below is a contract,
not a claim that it is already implemented. Related issue: #456.

## Current implementation status (2026-10-04)

This status is reconciled against main at
`d0b313fdf80c121d3fe213720779702fcde1ca92`. Historical evidence below records
intermediate states; descriptions of an unmerged candidate there are not
current rollout status. No new Bosn release is claimed.

| Requirement | Implemented on main | Verification and remaining gap |
|---|---|---|
| Warm caches across jobs and fresh engines | Shared machine volume for act archives, runner-image tar, actions, tool installs and repository cache archives; unique tool publication stages and hidden-stage exclusion | Real private-Docker proofs establish later-job and fresh-engine hits. Shared class expiry and sustained physical ceilings remain open. |
| Machine cache accounting | Allocated and apparent bytes for five classes, bounded per-repository detail, partial/unknown results and independent volume total | Sparse files, malformed samples, failed reader cleanup and actual cache measurement are covered. Samples are non-atomic; nested engine bytes are separate. |
| Private containers/images/build cache | Exact owned engine/storage journal and retirement, with bounded recovery after failures/restarts | Removing a verified private engine retires its nested Docker storage. This is not host owned-image or shared builder expiry. |
| Archive retention | Verified act2.7, typed age/byte policy, bounded maintenance reports and journaled finite-lived helpers | Actual private cohort maintenance and warm-cohort reuse pass. Default legacy planning remains outside configured retention. |
| Idle maintenance | Normal daemon discovers an existing immutable shared policy and runs periodic maintenance without jobs; bounded latest outcome persists for diagnostics | Actual idle daemon and restart proofs pass. Absence never bootstraps defaults; policy bootstrap and production repository enrollment remain open. |
| Warm migration | Typed legacy/cohort routes, participating lifetime leases, exclusive import, historical publication journal and sampled current inventory | Callable imports preserve warmth and recover historical publication. They do not authorize production enrollment or physical-footprint claims. |
| Routing admission | Participating legacy wrapper rechecks routing after acquiring its shared migration lease | Real lock race proves published routing refuses a stale legacy plan. Routing publication/selection and exclusion of older nonparticipating writers remain open. |
| Other shared classes and host pressure | Accounting and warm restore exist; unmanaged census reports host build-cache bytes | Tools, actions, shared image tar and tool-install expiry are unfinished. Host build cache is report-only in unmanaged GC; owned image and scoped builder pressure control are unfinished. |

Merged implementation evidence:

- Foundation [PR #473](https://github.com/zackees/bosn/pull/473):
  `b23398ae`, owned footprint and private-storage reconciliation.
- Retention/startup [PR #483](https://github.com/zackees/bosn/pull/483):
  `0f9c2302bd172e5492015c18b196bac6775f6cde`, including the act2.7 pin, tool
  publication corrections, policy discovery, current inventory and startup
  maintenance. These changes are on main, rather than only a candidate.
- Routing admission [PR #485](https://github.com/zackees/bosn/pull/485):
  `d24c25d676b94910ece3141788fa340ea33a8b10`; required remote checks passed.

Act2 v0.2.89-act2.7 was released from
`1a6782d5bbabb715f425cad5601b1f706fc23fa8` (merged PR #28). Exact-commit full
CI 37190653388 and release run 37191477610 passed. The Linux x86_64 archive
SHA-256 is `61d640112af87278075c70cd67412ec9c44ceffd1e269e88632d50d4a83ebd27`;
the extracted binary SHA-256 is
`328147f59cc101aa86cc826ecb5e7b2493f74765d520d4809997b2a9c6ba56bd`. Published
checksums and the reported version match the main pin. This release includes
empty-cutover correction and historical publication receipts; historical receipt
parsing alone cannot authorize enrollment.

The cache lifecycle follow-up [PR #486](https://github.com/zackees/bosn/pull/486)
is merged at `d0b313fdf80c121d3fe213720779702fcde1ca92`. Its exact-source gate
passed (Rust397s, Linux305s, total702s; tree
`3580afa77edd7db51100ed9623265fa14a11d562`) and required remote checks passed.
Main now avoids duplicate snapshots for identical helper registration and uses
cancellation-safe bounded process controls for normal CI commands and cache
maintenance/inventory/import/receipt reports. These preserve journals and
nonzero/partial outcomes; local client cancellation does not establish remote
Docker rollback. Distinct-nonce history and physical registry convergence remain
open. No Bosn release is claimed.

The new survey and isolated copy-on-write seed primitive are documented below.
They are evidence for the next toolcache implementation, rather than activated
immutable generation publication or production mounts. Production warm
enrollment, older-writer exclusion, shared class expiry and host image/build-cache
retention remain open.

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

### Current destination inventory before warm admission (follow-up candidate)

Bosn now reads act2 `cache audit` through a supplied verified engine, using a
30-second deadline and 64 KiB output bound. The initial page has at most 12
entries; act2's count, logical archive byte total and fingerprint describe its
whole-store scan. Continuation entries are not represented as a full exported
catalog. Typed validation rejects identity/schema mismatches, contradictory
page/count/byte evidence, invalid entries and unbounded diagnostics. Missing,
busy, partial, failed-command and unknown inventory cannot satisfy the current
inventory requirement. Nonzero commands retain valid partial evidence.

The published act2.7 private-Docker import/expiry proof passed in 1.66 seconds.
Current inventory first observed the imported archive (one entry, 80 bytes);
after independent idle maintenance it observed zero entries/bytes and a changed
fingerprint while the historical import receipt still described the original
publication. Source hashes remained unchanged and the exact helper was removed.
A focused parser test passed and service all-target Clippy passed (18.09 seconds)
before the live fixture extension. The initial final lint attempt exposed the
fixture's 106-line function; extracting its current-inventory assertion kept
the gate intact. The expanded live proof passed again, and final all-target
Clippy passed in 18.91 seconds. Independent review passed.

This supplies current sampled evidence, not an enrollment transaction, snapshot
lease across routing publication, proof of old-peer exclusion or physical block
accounting. Production admission and maintenance startup remain open.

Promotion gate c602bb62-eaf2-4906-9027-5aa290ad1d9c completed after 378
seconds with all Rust workspace tests and lint steps successful, but its Bosn
0.1.14 driver marked the overall run Incomplete: reusable-workflow execution
identity was not qualified. Cleanup was Removed. This is not a passing gate
receipt; the Linux lane was not run. Bosn 0.1.15 is now available as a released
driver and the next gate will use an isolated state directory with that version.

### Normal daemon startup maintains existing agreed cohorts (follow-up)

Normal `Service::serve` now starts the independent cache maintenance worker and
keeps the registry writer alive until worker cancellation finishes. It polls
for an existing canonical shared policy; no volume/record means no defaults,
bootstrap, migration or deletion. Discovery errors persist an Unknown latest
result without a helper reference and retry after 60 seconds. Once discovered,
the bounded periodic supervisor runs independently of job admission and saves
its latest outcome before the service consumer receives it. The existing FD6
nonblocking cross-daemon lock continues to exclude concurrent maintenance
commands, though duplicate helper creation is still possible.

Each maintenance helper additionally requires the existing canonical policy on
its actual mounted volume, rather than publishing policy itself. A replaced
volume or removed/conflicting record cannot inherit cached in-memory agreement.
Explicit participating cohort workflow execution still publishes agreement;
maintenance never opts legacy repositories into the cohort or deletes source
stores. Production route selection remains Legacy and configured cohort
retention stays refused pending warm enrollment and old-peer coordination.

The genuine private-Docker normal-daemon fixture passed in 1.45 seconds: startup
without submitted jobs discovered the existing policy, recorded a successful
complete budget outcome, stopped normally and left the exact helper journal
Removed. The no-volume/Docker-outage test issued only volume inspection, never
helper creation or maintenance; absence left no snapshot and outage persisted
Unknown. Focused safety tests and all-target Clippy passed (14.12 seconds).
The tightened per-helper existing-policy check and full service suite are being
verified; no shipped startup behavior or prompt uncertain-command shutdown
guarantee is claimed yet. Other shared cache classes and host image/build-cache
expiry remain open.

The tightened policy check passed the real idle-daemon proof again (2.66
seconds). Full-suite runs exposed setup-only fixtures using the real CI backend:
background discovery legitimately added helper intent events to their exact
setup event expectations. Setup and rollover fixtures now explicitly supply
the existing fake CI backend; their setup executors and exact event assertions
are retained. The corrected full suite and final lint are running.

The promotion candidate before this startup follow-up passed its Rust lane
under the released Bosn 0.1.15 driver (375 seconds), with exact-source executed
step evidence. Its Linux lane is running as
41f19684-93f1-4b0d-bb80-de4603894b89. This supersedes the earlier driver
0.1.14 Incomplete result but does not yet establish a complete promotion gate.

After both setup fixture groups used explicit fake CI backends, the full private
Docker service suite passed: 463 passed, zero failed, 18 explicit live/large
prerequisites ignored. Final service all-target Clippy passed in 19.67 seconds.
The normal-startup slice and fixture isolation received independent review.
The preceding promotion candidate now has a complete passing exact-source gate:
Rust 375 seconds, Linux 332 seconds, total 707 seconds, stamped commit
85bc9afcba5760b4c3c418479c240a6620bc5a01 and tree
d27b7f0616d9e4dfccc9118d35d759896af1dd53. The startup follow-up still
requires its own integrated exact-source gate before promotion.

### Stale Legacy planning cannot cross participating route publication

The fixed shared route-record location is now
`/bosn/cache/actcache/.bosn-cohort-routes-v1/<16-hex-namespace>`. This follow-up
defines its admission fence only; it does not publish or parse routing records.
Actual Legacy workflow invocation acquires the shared lifetime FD8 lease, then
checks the namespace's record. Any record, including malformed content or a
dangling symlink, refuses the stale invocation before act starts and requests
replanning. Invalid/unreadable routing directories also refuse. Listing and
explicit cohort invocations retain their existing lease behavior.

The check belongs after lease acquisition: migration can publish while a
previously planned Legacy job is waiting. The real shell/OS-lock proof marks
the attempted acquisition before publication, confirms the job stays blocked,
publishes while holding the exclusive migration lease, releases it, and requires
exit 78 with no binary execution. The original unfenced wrapper is run against
the same publication and reproduces the unsafe admission. Malformed/dangling
records and an invalid routing directory refuse; genuine absence runs normally.
Two focused lease tests passed in private Docker (0.14 seconds), preserving
process-death lease release. Final all-target service Clippy passed (17.89
seconds) and independent review passed. Integration still requires its own
exact-source promotion gate.

Inspection of the actual released act2.4 source confirms it has no cohort
marker/admission protocol (`pkg/artifactcache/cohort.go` does not exist at that
tag; its legacy handler directly opens the selected directory). Consequently
a new marker or this participating Bosn wrapper cannot fence older binaries.
Old-peer exclusion remains an enrollment prerequisite, alongside durable shared
routing publication, current inventory and snapshot coordination. Neither an
empty census nor this guard authorizes production migration. Default planning
still selects warm Legacy; configured retention remains guarded.

The preceding startup candidate passed its rebased exact-source gate: Rust
325 seconds and Linux 330 seconds, total 654 seconds, stamped commit
06deb89d233e with tree13207a267328092bda66ffaa54c55fad16b43ee7.
The outer gate tool used the older pin and warned that `gate.replay` was unknown;
the lanes themselves used the current source-bound checker and passed. Verification with the current repository's gate-tool pin passed:
GATE-003 accepts the attestation for the exact tree. PR #483 now carries the
verified rebased startup head and is mergeable; remote CI is running. No merge
or Bosn release is claimed. This stale-route follow-up requires its own gate.
### Cleanup retries preserve the ledger without duplicate snapshots

A new survey of helper accounting found that every successful re-registration
of an already known immutable ID appended another full snapshot. Uncertain
removal retries can repeat indefinitely. The private-Docker regression test
reproduced 102 journal entries for a single helper after intent, first
registration and 100 identical retries (RED, 16.76 seconds).

Registration now validates identity, state and time first, then treats the
same ID in Created state as a no-op. The original transition timestamp and
full intent remain unchanged. A first registration and terminal removal still
append durable events; Removed nonces remain reserved forever and conflicting
IDs, invalid times and registration after removal still fail. This preserves
the append-only ledger and nonce protection instead of deleting audit history.

The change bounds repeated registration of one unresolved helper to its real
transitions. It does not bound the number of distinct helper nonces or the
whole registry, compact SQLite pages, reclaim historical audit data, or
establish current liveness from a transition timestamp. Periodic policy
discovery without a record still creates fresh read-only helper intents;
that cadence and the overall accounting-history footprint remain open.
GREEN: five registry helper tests passed (2.39 seconds), including reopen and
terminal nonce protection; 20 service accounting/helper recovery tests passed
(0.59 seconds, four explicit live prerequisites ignored). Registry/service
all-target Clippy passed (29.70 seconds) and independent review passed. The
follow-up still requires its exact-source gate before promotion.


The helper idempotence candidate passed its exact-source gate13 before
integration: Rust 494 seconds, Linux 567 seconds, total 1062 seconds, stamped
`1edf29024f8b389627522fa7086663b3fd47293c` and tree
`742947b21eebd930f8b3af174c3147a84b2bc474`. It is now integrated with merged
routing guard main `d24c25d6`; the doc append conflict preserved both evidence
sections. The current status table has been reconciled with merged behavior.
This combined tree requires fresh exact-source proof before publication.


### Verified locally: normal daemon shutdown during stalled Docker discovery

A normal `Service::serve` daemon with no workflow jobs was tested against a
synthetic Docker transport that enters the real shared-volume discovery call
and stalls for 12 seconds. An explicit marker proves the command started before
the client requests daemon shutdown. The assertion includes kernel runtime
teardown, rather than only completion of the service future.

The baseline failed: shutdown took **12.001524079 seconds**. CI control commands
used `capture_async`, whose kernel implementation dispatches bounded capture to
the runtime blocking lane. Dropping the maintenance future left that blocking
command alive, delaying runtime teardown. Evidence:
`retention-maintenance-shutdown-red.log` (isolated Docker).

The correction uses the existing process-session streaming runner with a bounded
event channel drained concurrently. The session owns and kills the local Docker
client when its future is dropped, while retaining the existing deadline and
total output ceiling. The first regression passed with shutdown in
**113.567175 milliseconds**. The final regression passed in **113.687191 ms**,
including a check that the captured client PID no longer exists after runtime
teardown. The complete service suite passed **470 tests** (18 explicit live
prerequisites ignored), and all-target Clippy passed. An initial broad run found
a stack overflow from the larger nested session future; boxing that future fixed
it, with both the discovery regression and the complete suite rerun successfully.

This does not prove that killing a Docker client rolls back a remote create or
stops a remote exec. Durable helper intents, exact ownership checks and recovery
still handle uncertain remote effects. The real lost-create-ack recovery test was rerun against the private Docker
daemon: cancellation returned in **140.777 microseconds**, the exact helper was
recovered and removed, and the shared cache remained measurable. The whole test
completed in **1.08 seconds**, compared with the previous 63.29-second test whose
blocking wrapper slept for 60 seconds. This proves direct helper cancellation
and recovery plus normal daemon discovery shutdown; normal daemon shutdown
during a real uncertain create/exec and every other shutdown phase remain
separate checks. Evidence: `retention-maintenance-shutdown-boxed-checks.log`,
`retention-maintenance-shutdown-final-checks.log`, and
`retention-maintenance-shutdown-real-recovery.log`. Publication still requires
the exact-source local gate.


### Cache report commands share cancellation-safe bounded controls

A follow-up survey found four report-producing cache operations still bypassed
the normal CI control runner: cohort maintenance, current inventory, import and
publication-receipt recovery. Each used kernel blocking capture directly. Thus
the discovery cancellation correction alone did not establish cancellation of
a maintenance report command.

A regression waits for the actual `prune-cohort` Docker client invocation to
enter a 12-second stall, cancels its future, and includes runtime teardown in
the latency assertion. Baseline RED was **11.999834665 seconds**. The correction
uses one common bounded process-session control runner for all four calls,
retaining each original deadline (30 seconds, or 90 seconds for import), its
**64 KiB** total output ceiling, and typed nonzero/partial stdout handling.
Ordinary controls retain their separate 1 MiB ceiling. The runner is extracted
by responsibility from the nearly 1,000-line engine module.

Focused GREEN was **593.295 microseconds**, including runtime teardown and a
check that the exact local client PID was absent. The complete service suite passed **471 tests** (18 explicit live prerequisites
ignored; 87.42 seconds) against the isolated private Docker daemon. The actual
import/current-inventory/historical-receipt proof passed (11.11 seconds), and
maintenance contention/death-release proof passed (9.01 seconds). All-target Clippy passed (47.76 seconds). The slice is independently reviewed;
its final integrated tree still requires an exact-source gate before publication. Local client cancellation
does not establish remote command termination or undo a published import;
durable journals and ownership-verified recovery remain required. Production
enrollment and shared class/image/build-cache expiry remain unfinished.
Evidence: `retention-cache-command-cancellation-red.log` and
`retention-cache-command-cancellation-green.log`.


### Shared class survey and immutable tool seed experiment (2026-10-04)

A bounded read-only survey used this task's live gate engine
`bosn-act-4326b744-5536-4fe8-94ea-fd34b639d670` to inspect the existing machine
cache without creating a reader or changing its data:

- Five act archive variants occupy the shared `tools/` class, including .2, .3,
  .4 and diagnostic builds. Several historical archive paths have no existing
  lock file; this does not prove either an active reader or safe exclusion.
- The shared runner-image tar has apparent length **566,650,368 bytes**. Engine
  retirement removes a private loaded copy, rather than this shared tar.
- Shared `toolcache/` measured **8,088,304 KiB** allocated (about 7.71 GiB), and
  this fresh engine's `act-toolcache` measured **8,088,652 KiB**. These are
  separate, non-atomic samples; their small difference is not a leak verdict.
  The seed script explicitly uses BusyBox `cp -a` on every visible tool family,
  so cache hits still require copying the broad tool set into each fresh
  engine. The actual BusyBox cp has no reflink option.

The classes contain Python, Node, Go, uv, Soldr toolchains/syslibs/bundles,
runner tools and binfmt data. This is evidence that warm reuse alone does not
solve per-engine tool-storage amplification. No old shared artifact was deleted
and no current archive was declared unused. Survey artifact:
`retention-shared-class-age-survey.txt`.

A genuine private-Docker experiment then tested a closed read-only lower volume
with a completed-install marker, a 16 MiB payload and a small settings file.
Two successive fresh privileged containers each mounted an OverlayFS view with
the same immutable lower and a distinct private tmpfs upper/work directory.
Both read the identical payload SHA-256 and marker with **0 KiB** allocated
upper data initially. Mutating settings allocated **4 KiB** privately, preserved
the original lower settings, and did not copy the 16 MiB payload into either
upper. Container wall times were 0.790 and 0.519 seconds. Both containers were
auto-removed; the exact task-owned source volume was removed and cleanup was
confirmed. Artifacts: `retention-toolcache-cow-experiment.py` and its JSON result.

An additional private Docker experiment mounted that overlay at a nested
Docker named volume's data path. Two actual job containers in the first fresh
engine shared their private settings change; a job in the second fresh engine
read the original settings. All three read the same 16 MiB payload digest.
Each engine's upper grew from **0 KiB to 4 KiB**, without copying the payload.
Engine readiness took 16.37 and 16.60 seconds. Exact task-owned engines and
the fixture volume were removed and their cleanup confirmed. Artifacts:
`retention-toolcache-cow-nested-experiment.py` and its JSON result.
This used the Docker image's normal Dind entrypoint, not Bosn's production
engine profile or an act workflow. It verifies nested volume visibility and
within-engine sharing; production initialization, real installers, lifetime
protection and expiry remain unverified. The current
mutable tool cache cannot serve as a live overlay lower: the kernel documents
undefined behavior when underlying trees change while mounted. Immutable
generations are therefore required before applying the primitive to Bosn.
See [the kernel OverlayFS contract](https://docs.kernel.org/filesystems/overlayfs.html#changes-to-underlying-filesystems).

#### In-progress act2 immutable install publisher

The unpublished `feat/immutable-tool-cache` act2 worktree adds
`cache tool-publish --from ... --cache-server-path ... --max-bytes ...
--apply --source-quiescent`. It copies one completed install into a private
stage, validates typed metadata and file hashes, and atomically publishes a
manifest-identified object without replacing an existing destination.
Mutable source inodes are never hard-linked into the object. Identity includes
permissions, ownership and timestamps as well as file data. Source quiescence
is an explicit caller assertion; a completion marker alone cannot prove it.

The focused CLI test failed before implementation and then passed in an
isolated Go Docker container: unchanged installs reuse an object, changed
installs produce a new object, and the old payload remains intact. Additional
focused tests passed for executable modes, relative symlinks, sibling completion
markers, incomplete/over-budget/cancelled sources, corrupt existing objects and
unknown stores. Corrupt objects are preserved and reported as partial failures.
Initial execution found and fixed missing coordination-file initialization.
The complete artifact-cache package test run passed (14.27 seconds), `go vet`
passed for the cache and command packages, and their golangci-lint run reported
zero issues after scanner decomposition and explicit returns.
Review then found external symlinks could violate object closure and eager
directory enumeration could defeat the entry bound. Both were corrected:
links must be relative and resolve inside the object (dangling/cyclic links
are rejected), traversal reads 256 entries per page with a depth limit of 64,
and initial-store recognition reads at most two entries. Absolute, parent and
transitive symlink escape regressions failed before their fixes and passed
afterward; excessive-depth refusal and all existing focused tests also passed.
The corrected package/command lint run reported zero issues. The same reviewer
returned PASS for the corrected slice. Artifact-cache package tests passed
again (14.25 seconds), and `go vet` passed. The act2 candidate is committed at
`1941705`; it has not been pushed or released. Artifacts: `act2-toolcache-link-red.log`,
`act2-toolcache-transitive-link-red.log`, `act2-toolcache-review-fixes.log`.
Artifacts: `act2-toolcache-publish-red.log`,
`act2-toolcache-publish-green.log`, `act2-toolcache-boundaries.log`.

The publisher also has a full-operation fault test: inject a final directory
sync failure after the no-replace rename, observe `published=true` together
with `partial=true`, then verify a subsequent normal publication reuses that
same object and acknowledges durability. This tests truthful uncertain-state
reporting and replay; it does not simulate a power loss or prove crash survival
for every filesystem. Artifact: `act2-toolcache-durability-replay.log`.

This is local work, not released or enrolled by Bosn. Publication gates,
power-loss evidence, reader protection, accounting
and coordinated expiry remain open. The publisher alone does not solve
per-engine tool copying or authorize deletion of existing caches.

#### In-progress immutable generation assembly

The act2 candidate `cbbd365` adds `cache tool-generation --manifest ...
--cache-server-path ... --max-bytes ... --apply`. A bounded, typed schema-1
specification lists install paths and closed object IDs. Assembly rejects empty
or overlapping paths, path escapes, completion-marker collisions, missing or
corrupt objects, and an exceeded logical payload-byte bound. The specification
is limited to 64 KiB and 256 installs; the complete tree remains subject to the
100,000-entry and depth bounds.

Under the same participating writer lock as object publication, it validates
every object, creates new directories and relative symlinks, and hard-links
file data only from closed objects. It preserves object file metadata and
materializes sibling completion markers. A sorted typed manifest identifies
the complete generation. Stage re-audit, file/directory sync, atomic no-replace
rename and final directory sync precede success. Reuse requires a complete
manifest/payload audit; unexpected existing data is preserved and rejected.
The resulting tree must be exposed as a read-only lower. Writable use would
mutate the shared file inodes and is not an approved enrollment path.

The CLI regression failed before implementation (`unknown flag: --manifest`)
and passed afterward. It proves a repeated recipe reuses the generation,
reordered installs yield the same ID, and both old and successor generations
share the original object's file inode rather than copying it. The old
generation does not acquire the successor's additional install. Boundary tests
passed for invalid requests, cancellation, budget failure, missing objects and
unexpected existing payload. Full artifact-cache package tests passed (14.27
seconds), `go vet` passed, golangci-lint reported zero issues, and the existing
reviewer returned PASS. Artifacts: `act2-tool-generation-red.log`,
`act2-tool-generation-green.log`, `act2-tool-generation-boundaries.log`,
`act2-tool-generation-package-checks.log`.

The reported generation bytes bound logical file payload; they are not a
physical allocation total. Generation assembly does not yet supply lifetime
reader protection, safe retirement, policy convergence or Bosn enrollment.
Those remain necessary before production activation. The candidate is local
and unpublished.

A further isolated-Docker experiment built the actual candidate CLI, published
a completed 16 MiB install, assembled its generation, and transferred only the
closed tree into a task-owned lower volume. Two nested job containers in one
fresh Dind engine shared a private settings mutation; a job in a second fresh
engine read the original settings. All read the same payload digest and the
materialized completion marker. Each private upper grew from **0 KiB to 4
KiB**, without copying the payload. The exact two engines and lower volume
were removed with ownership verified. Fixture source/store artifacts remain
in the task's private Go container tmpfs. The first transport attempt failed
because its outer Docker exec did not attach stdin; its lower volume was
removed, stdin attachment was corrected, and the subsequent experiment passed.
This uses standard Dind initialization, not Bosn's production profile or a real
act workflow. Artifacts: `retention-tool-generation-nested-experiment.py` and
its JSON result.

#### In-progress generation reader coordination (2026-10-04)

The living spec through generation assembly is on main via PR #487, merge
`c8ab6697`. Its exact-source local gate passed in 627 seconds (Rust 341s,
Linux 286s), stamped `6d1b8a77fd0cb2036c811548830cc8de41dee65c` for tree
`ff728e2a68bc4b2fc5339989c9dc487b6a8f4f85`; pinned verification and required
remote PR checks passed. No Bosn release was made.

The act2 candidate `01a69fa`, published in
[act2 PR #31](https://github.com/zackees/act2/pull/31), creates a per-generation
`.readers-v1.bolt` coordination file in the private stage before directory sync
and atomic publication. Reuse refuses missing reader coordination. The typed
`AcquireToolGenerationLease` API validates a canonical established store,
generation identity/schema and payload under the catalog writer lock, then
acquires that generation's shared reader lock before releasing the catalog.
Multiple readers coexist; publication of other generations remains possible.
Future retirement must take the catalog lock and that generation's exclusive
lock in the same order, and preserve their inode identity throughout deletion.

Reader admission never initializes a store or recreates missing coordination.
Review exposed an established-store bug that could replace a renamed catalog
lock while another process still held its original inode. The regression
reproduced that failure, then passed after both admission and publication were
made to refuse a missing established-store catalog lock. An empty-store
admission also leaves the directory empty.

The caller must retain the reader descriptor for the entire engine lifetime,
including preparation and uncertain cleanup, and close it only after all
mounts/readers are gone. Admission's context bounds validation; it does not
automatically close a successfully acquired lifetime lease. Holding this lease
only in the Bosn daemon is insufficient: daemon death could release it while
the Docker engine still reads its lower. Actual engine-owned holder integration
is still required. There is no generation retirement command or activation yet.

Focused tests verify published coordination, concurrent readers, exclusive
retirement-lock refusal, successor publication while an old reader is held,
missing coordination refusal, and OS lock release after killing the exact
task-owned helper process. Full artifact-cache package tests passed after the
fix (14.39 seconds), `go vet` passed, golangci-lint reported zero issues, and
Darwin/Windows compilation of unsupported-platform stubs passed. The same
reviewer returned PASS for the corrected slice. Artifacts:
`act2-tool-generation-reader-red.log`, `act2-tool-generation-reader-tests.log`,
`act2-tool-generation-catalog-reader-red.log`,
`act2-tool-generation-reader-catalog-fixes.log`,
`act2-tool-generation-reader-final-checks.log`.

Source-bound checks then passed on the clean committed candidate: all 2,547
exported Git files matched the private Docker build source before and after
execution. Complete cache tests, focused tool command tests, vet, lint and
Darwin/Windows compilation passed in 18 seconds. Source SHA:
`01a69faac05e88cf0e2b5b84b23c2adeb47995a1`; tree:
`f6fe365745366bd8e698997c903511c352947f6e`. This scoped local gate does not
replace full act2 CI on the exact release candidate. Artifacts:
`act2-immutable-source-bound-gate.json` and its execution log. No act2 release
or Bosn activation is claimed.

Admission currently audits the full payload while holding the catalog lock.
Its cost on the surveyed 7.7 GiB cache has not been measured; this is a
correctness baseline, not verified efficient production admission. Engine-owned
lifetime protection, efficient admission, coordinated accounting/retirement,
current-generation selection and physical budget convergence remain open.

#### Next implementation contract: immutable generations and private writes

- Publish typed, content-identified closed generations from completed installs.
  Stage and validate their manifest/data before durable publication; readers
  must never mount a stage or treat an incomplete record as an empty cache.
- Reuse closed generation objects when building a successor, so new generations
  do not copy the complete tool set. Hard links may join immutable generation
  objects; they must not expose mutable source objects or job-written upper
  data as shared lower data. No installed lower object changes in place.
- Pin a generation in the engine's frozen ownership/storage profile. Acquire
  its reader protection before mounting and preserve protection for the actual
  engine lifetime, including preparation. Lost create/start acknowledgements
  need the existing exact-ID journal and cleanup rules. A missing generation
  requires replanning or known warm-copy fallback, rather than silently starting
  with an empty tool cache.
- Mount a private writable overlay at the nested tool-cache volume's data path.
  Verify that nested job containers see it, that normal installers can create
  new tools, and that edits/deletes/chmod/copy-up remain private. Save only
  completed new installs back through coordinated publication.
- Account shared closed data once in the toolcache class, include stages and
  unknown objects, and report private upper storage with the owned engine.
  Independent shared/private samples must not imply a single atomic total.
- Expire old generations, unreferenced objects and stages only after verified
  writer/admission and reader exclusion. Deletion must preserve a current warm
  seed and in-use generations, expose protected overflow, and converge under
  sustained publication; stage cleanup and old generations cannot be left
  outside the byte/age policy. This also requires resolving distinct-nonce
  ledger growth and host owned-image/scoped builder retention.

Activation is not implemented. Required acceptance remains actual later-job and
fresh-engine warmth, private mutation isolation, generation durability/replay,
concurrent publisher/reader/GC safety, exact retirement and sustained logical
and physical ceilings. The original broad shared-cache/expiry goal remains open.


### Engine-owned reader descriptor: actual init-chain experiment (2026-10-04)

The reader-contract follow-up is merged in [Bosn PR #488](https://github.com/zackees/bosn/pull/488)
at `5fb7adbd74ccf964ba966156636f72773df4956a`. Its exact-source local gate
passed in 605 seconds (Rust 304 seconds; Linux Docker tests 301 seconds),
and required remote checks passed. [Act2 PR #31](https://github.com/zackees/act2/pull/31)
is merged at `adefc9f75bfbb789ac54aa18bd1546bcf6cc04d1`; the exact-merge
[full CI run](https://github.com/zackees/act2/actions/runs/37215686314) passed. No release or Bosn pin update is claimed for that merge.

A new private-Docker experiment tests the engine-owned descriptor approach
through the actual current `ENGINE_INIT` body, then `docker-init`, then
`dockerd`. It uses the already cached pinned Docker image, a read-only root,
private cgroup namespace, Unix-only daemon socket, bounded private tmpfs storage,
and a read-only volume containing a generation produced by the current act2 CLI.
A private launching client is killed after readiness; the engine remains alive.

- Negative control: without a held descriptor, an exclusive writer obtains the
  generation reader lock while the engine runs, before and after client death.
- With `exec 5</lower/.readers-v1.bolt` and `flock -s -n 5` before the unchanged
  init body, exclusive writers are refused while the engine runs, including
  after client death. `/proc/1/fd/5` still names the generation reader file.
- After stopping that exact engine and verifying it is no longer running,
  an exclusive writer obtains the lock. Both experiment engines and their
  labelled volume are removed after exact ownership checks.
- Engine readiness was 0.529 seconds for the control and 0.409 seconds for the
  protected engine. These tiny-fixture timings do not measure admission costs
  for the 7.7 GiB production tool cache.

Evidence artifacts in the task's private git directory:
`retention-engine-owned-reader-experiment.py`, `.log`, and `.json`. Run key:
`157021de141146fb9399ff225d5141dc`; generation:
`5b50ccf627083156cc21a7c740d94d3a0e8b177991c0c1c78dcf878721e3a07e`;
unchanged base-init SHA-256:
`e269505630ad33e94fcf314157a7f6376c32f73a538275f0b4b76616b1ac54b3`.
The small source/object fixture remains inside the private Go container's
`/tmp` filesystem; no shared cache or host daemon was pruned or stopped.

**Scope:** this proves descriptor inheritance through the real init chain and
release on engine stop. It does not exercise an actual Bosn daemon crash,
catalog-coordinated admission, production generation selection, or retirement.
The generation and its reader file were transferred into a private experiment
volume; this is not proof of mutex identity between that volume and the original
publisher store. Production must retain the same published reader inode,
acquire catalog protection before generation protection, and validate the closed
generation before any mount. A raw shell lock alone is insufficient admission.
The shell prototype would require FD 5 to be reserved in the frozen engine
command/profile and verified during recovery. The native handoff below instead
preserves its dynamically allocated original descriptor; production must freeze
that policy and observe the actual descriptor and inode during recovery. The existing daemon-only reader API is still insufficient for a
surviving engine. Engine admission, mounting, coordinated expiry, and efficient
large-cache admission remain implementation work.


### Native admission and exec handoff (implemented candidate, 2026-10-04)

[Act2 PR #32](https://github.com/zackees/act2/pull/32), candidate
`7fd5beef176674b7f71f5bcfc062c5ad1b118897`, adds:

```sh
act cache tool-exec --cache-server-path STORE --generation ID \
  --max-bytes POSITIVE --apply -- COMMAND ARGS...
```

`ExecWithToolGeneration` is Linux-only and intended for a dedicated engine-init
process. It validates the closed generation under the catalog writer lock,
then opens the original bounded regular reader file with bbolt's shared lock
before releasing catalog protection. It clears close-on-exec on that exact
original descriptor and replaces its process, reporting the dynamic descriptor
number in `BOSN_TOOL_GENERATION_LEASE_FD`. It does not force a fixed FD number or
close/reopen the mutex. Failed exec restores descriptor flags and releases the
reader. The command must keep the descriptor open throughout engine lifetime;
the generation is still only a read-only overlay lower, never a writable mount.

The focused regression was RED because the API was missing, then GREEN: the
same original-store bbolt writer remains blocked across exec and succeeds after
the replaced process exits. Malformed argv, canceled admission and failed exec
also release the reader. Final exact-source scoped gate passed in 18.73 seconds:
2,552 exported files matched private Docker source bytes/modes before and after
full artifactcache tests (14.542 seconds), focused tool commands, vet/lint with
zero findings, and Darwin/Windows unsupported-stub compilation. Cumulative
independent review passed. These checks do not replace full act2 release CI.

A second actual-init experiment runs the native CLI as engine PID 1 before it
execs the unchanged init chain. Publication and admission use **the same labelled
volume**, with read-only admission attachment; no published mutex is reconstructed
for admission. Control allows writers while alive. Native admission blocks writers
before and after killing the launching client, then releases the lock after the
exact engine stops. PID 1 retains original reader descriptor 7 in this run.
Readiness was 0.428 seconds for the control and 0.381 seconds for the native
protected engine, with an eight-byte fixture. Both owned engines and the volume
were removed after label checks. Run `80d4509034074495abc87218ad2f56c5`;
generation `241e5b4bbdcc223b5f5cb38e55bb533eed89b592034a8ce78390ca6faa16f5c4`.
Artifacts: `retention-engine-native-reader-experiment.py`, `.json`, `.log`;
`act2-tool-generation-exec-red.log`, `-green.log`, and
`act2-generation-exec-source-bound-gate.json`/`.log`, in the task's private git
directory. This verifies init handoff, not an actual Bosn daemon-crash path.

Two prior archive-transfer fixtures were refused by canonical payload validation
and their exact engines/volumes cleaned up. Ordinary tar transfer cannot be assumed
to preserve a generation manifest's metadata. Validation was not weakened; the
passing experiment publishes and admits on the same volume. A later diagnostic
accidentally auto-created an empty replacement private volume after cleanup; that
exact replacement was inspected and removed. No shared cache was pruned.

**Still open:** PR #32 is not released or activated in Bosn. The production frozen
profile must reference a selected generation and the native handoff policy;
recovery must verify its mount and live original-inode holder. Coordinated current
selection, actual overlay mounting, completed-install publication with writer
exclusion, efficient admission of the 7.7 GiB cache, generation/object expiry,
physical accounting and image/container/scoped build-cache expiry remain required.


### Durable warm selection and concurrent completed-install updates (candidate, 2026-10-04)

The native handoff spec is merged in [Bosn PR #490](https://github.com/zackees/bosn/pull/490),
merge `eafc7975840567fe0407533c64955a7b80e97185`, after the exact-source
local gate passed in 525 seconds and required remote checks passed. Native
handoff code is merged in [act2 PR #32](https://github.com/zackees/act2/pull/32),
merge `09d1ad66486e04514d651362f4c0a155bc9ac7f5`; no release or Bosn pin
update for that merge is claimed.

[Act2 PR #33](https://github.com/zackees/act2/pull/33), candidate
`3dd0deddbb4b99f589638a7c575453f169af0759`, implements selection separately
from immutable generation publication:

```sh
# Deliberate first selection during store setup or explicit recovery only:
act cache tool-update --manifest COMPLETED_INSTALLS.json \
  --cache-server-path STORE --max-bytes POSITIVE --apply --initialize

# Normal completed-install update; never a missing-cache bootstrap fallback:
act cache tool-update --manifest ONLY_CHANGED_INSTALLS.json \
  --cache-server-path STORE --max-bytes POSITIVE --apply

# Advisory validated selection; does not hold a lifetime reader:
act cache tool-current --cache-server-path STORE --max-bytes POSITIVE
```

`UpdateToolGeneration` takes the original catalog writer lock, validates the
latest selected manifest, full payload and reader coordination, and merges only
supplied install paths into that latest recipe. An exact path replaces its prior
object reference; unrelated completed installs stay warm. The complete merged
recipe is validated and assembled while holding the same catalog lock, then a
synced private JSON stage replaces `.tool-current-v1.json` atomically and the
store directory is synced. Its typed report separates generation publication
from selection. Private selection-stage cleanup failures retain a reported
`pending_selection` path. The payload-byte ceiling is logical, not unique inode
allocation or a machine-wide disk quota.

Initialization is an explicitly named API (`InitializeToolGeneration`) and CLI
flag, required only when selection is unset. It refuses an existing pointer.
Ordinary updates refuse missing or malformed selection rather than constructing
a smaller cold successor. A new engine must never automatically call initialization
because an ordinary selection lookup failed. Deliberate recovery must first
establish which warm state is authoritative; the API does not infer that proof.

Verified tests:

- Concurrent updates from two stale engines retain three completed installs;
  a living reader on the first generation does not block successor publication,
  and the first generation remains unchanged. Repeating an unchanged update
  verifies/reuses the selected generation.
- CLI regression was RED with unknown `--manifest`, then GREEN: separate update
  recipes retain earlier warm payloads and `tool-current` reports the successor.
- A further actual RED showed a lost pointer silently bootstrapping and discarding
  prior installs. The explicit-initialization fix is GREEN: ordinary update refuses
  and leaves the pointer absent and old generation data intact.
- Byte-budget refusal preserves warm selection; symlink, oversize, unknown schema
  or field, missing generation and missing reader coordination are preserved and
  refused. No fallback empty selection is emitted by the current command.
- Full API injection after selection rename reports `selected=true, partial=true`
  on lost final directory-sync acknowledgement. An ordinary update retry verifies
  and reuses the existing publication, then converges. A visible first selection
  must be retried as an ordinary update, since initialization refuses existing data.

Scoped exact-source gate passed in 17.40 seconds: 2,558 exported source files
matched private Docker bytes/modes before and after full artifactcache tests,
focused tool commands including selection, vet/lint with zero findings, and
Darwin/Windows unsupported-stub compilation. Candidate tree:
`7c6308cd9a1c259e867508e6100a32ba4823f110`; cumulative independent review
passed with one reviewer. Artifacts in the task's private git directory:
`act2-tool-selection-red.log`, `-green.log`, `-command-red.log`,
`-lost-pointer-red.log`, `-fixes.log`, `-final-checks.log`, and
`act2-generation-selection-source-bound-gate.json`/`.log`.

**Not yet proven:** PR #33 is unreleased and not activated in Bosn. Current lookup
is advisory: selection and subsequent explicit-ID engine admission can race with
future retirement, so the production planner must coordinate the intent/reader
handoff or refuse and replan; it cannot mount from this report alone. Full payload
validation remains an unbenchmarked admission/update cost for the 7.7 GiB cache.
Generation/object/stage expiry, machine-wide physical accounting, production frozen
profile and recovery, actual job overlay mounts and quiescent completed-install
publication, old image/container expiry and scoped builder pressure remain open.


### Bounded tool-store inode accounting (act2 PR #34)

Implementation: https://github.com/zackees/act2/pull/34, candidate
`84511a0b61f6461af0487f65f30fc562846ee5ec` (unreleased). Linux API
`AuditToolStoreUsage(ctx, root, maxEntries)` and command
`act cache tool-usage --cache-server-path STORE --max-entries N` inventory a
recognized existing store while holding its original catalog writer lock. Missing
coordination is refused, never initialized as part of inventory. Participating
publication and selection cannot mutate the store during this observation.

The bounded metadata walk includes root directories, objects, generations,
private stages, manifests, coordination files and unknown entries. It follows no
symlink targets, refuses a different filesystem device, pages directory reads,
limits entries to the caller's positive bound (at most one million), limits depth
to 72 for store namespace prefixes, and has a 30-second context deadline. Payload
publication retains its existing depth-64 limit. No payload contents are read or
hashed by accounting.

Typed results distinguish four totals:

| Field | Meaning |
| --- | --- |
| `allocated_bytes` | Inode blocks times 512, once per device/inode; includes directory/control allocation |
| `apparent_bytes` | Regular-file and symlink-text size, once per device/inode |
| `unique_file_bytes` | Regular-file size, once per device/inode |
| `referenced_file_bytes` | Regular-file size at every path, including hardlink references |

All byte totals are null on an incomplete scan, cancellation, invalid bounds,
missing coordination, crossing, or arithmetic failure. Observed entry/inode counts
may describe a subset; they are not complete totals. Negative sizes and overflow
are refused. The report includes observation time, device, partial status and a
bounded diagnostic. CLI emits the typed partial report and exits with an error.

Verified evidence:

- A one-MiB immutable object referenced through two generations has four payload
  paths sharing one inode; referenced minus unique regular-file bytes is three MiB.
- Independent GNU `du` comparisons agree with both allocated and apparent totals,
  including manifest/control allocation and excluding a five-MiB external symlink
  target. The first apparent-size comparison failed because directory `st_size`
  was included; the implementation was corrected before publication.
- A sparse sixteen-MiB unknown file contributes its full apparent size and fewer
  allocated bytes. Its inclusion establishes footprint, not deletion eligibility.
- Entry exhaustion, cancellation, invalid bounds and missing catalog coordination
  produce null totals. Command tests cover successful and partial JSON reports.
- Exact-source gate compared all 2,564 exported files, bytes and modes before and
  after complete artifactcache tests, focused tool commands, vet, zero lint findings,
  and Darwin/Windows compilation. Gate passed in 21.03 seconds on tree
  `ade0e002ad94645cdba030b74f4dadae4265e3d0`; independent read-only review passed.
  Private evidence: `act2-tool-store-usage-source-bound-gate.json`/`.log`,
  `act2-tool-store-usage-apparent-red.log`, and `act2-tool-store-usage-checks.log`.

**Limits and next work:** This measures one tool store. It does not deduplicate
inodes across separately scanned stores/classes, measure filesystem journal or
Docker backing-store allocation, or impose a machine-wide quota. Nonparticipating
writers can still invalidate a coordinated observation, so production cutover must
exclude old writers. PR #34 is not released or activated in Bosn. Coordinated
retirement must protect the selected generation and live readers, account before
and after deletion, and preserve unknown/corrupt state. Generation/object/stage
expiry, whole-machine accounting, old engine/image expiry and scoped builder
pressure remain required. No shared object was deleted for this accounting slice.


### Coordinated generation retirement (act2 PR #35)

Implementation: https://github.com/zackees/act2/pull/35, candidate
`f8709458548f5510264babb96f58baee6a0b9c15` (unreleased). Linux API
`RetireToolGeneration(ctx, root, id, maxBytes)` removes one explicitly eligible
unselected generation. The caller must establish age/pressure eligibility; this
API does not choose candidates or implement an automated retention policy.

The original catalog writer stays held while validating the current selection
and its complete payload/reader coordination, validating the candidate manifest
and complete tree, collecting bounded store metadata, and acquiring the original
candidate reader mutex exclusively. Selection, admission and participating
publication cannot race this decision. Selected generations and live reader
holders are refused. Missing/corrupt selection or coordination is preserved and
refused. Candidate control layout must contain exactly the canonical tree,
manifest and reader mutex; unknown entries are preserved. Immutable shared
objects are never removed by this primitive.

Mount safety requires more than device identity. A same-filesystem bind mount
can preserve expected metadata/hashes while exposing a shared object to recursive
deletion. Independent review found this hazard in the initial implementation.
The correction compares Linux statx mount IDs for the store root, generation
namespace, candidate and every bounded descendant, without following symlinks.
Unavailable mount identity or any boundary causes refusal before deletion.
Production must exclude nonparticipating mount changes and writers during this
operation; this is not a defense against a privileged actor changing mounts after
validation. Both original coordination locks remain held through removal and
parent-directory sync. An error can follow partial deletion or lost durability
acknowledgement: reclaimed bytes must come from a fresh accounting observation,
never the logical generation size or a boolean return value.

Verified evidence:

- Focused regression was RED because retirement did not exist, then GREEN:
  selected generation stays intact, live reader prevents retirement, closing that
  reader permits old-generation removal, current selection remains valid, and
  the shared immutable object remains present.
- Lost selection, unknown control entries and missing reader coordination refuse
  deletion. Mount-ID refusal also has a deterministic injected-boundary test.
- A real same-device bind mount in an owned isolated container was refused;
  source/target device IDs agreed and the mounted shared object remained fully
  valid afterward. The container had no host binds or network and was removed.
- Exact-source gate passed in 21.68 seconds against all 2,569 exported files before
  and after complete artifactcache tests, focused command tests, vet/lint with zero
  findings, and Darwin/Windows compilation. Tree:
  `c3539145d22d2bed423b87622dfc9faa7611e1f4`. The first final gate caught an
  unchecked test unmount error; teardown now checks it. Cumulative independent
  review changed from CHANGES REQUESTED to PASS after mount protection.
- Private artifacts: `act2-generation-retirement-red.log`,
  `act2-generation-retirement-actual-bind-mount.log`, and
  `act2-generation-retirement-source-bound-gate.json`/`.log`.

**Remaining required work:** Automated age/pressure selection, object expiry,
owned-stage expiry, measured physical before/after reclamation and convergence,
protected-overflow reporting and bounded retry after partial retirement. This
API has no production Bosn caller yet. Selected-generation intent/reader handoff,
engine lifetime/recovery integration, large-store admission cost, cross-class
machine accounting, old image/container expiry and scoped builder pressure all
remain open. PR #35 needs remote CI/merge and an exact-default-branch full-CI
release before Bosn can pin and activate it.


#### Physical reclamation evidence for the pressure loop

A private Docker regression published a one-MiB object, selected a generation
with one reference, then selected a successor with two references to that same
inode. Retiring the unselected predecessor reduced observed allocated store
bytes from 1,118,208 to 1,097,728: only 20,480 bytes were reclaimed, while the
1,048,576-byte shared payload stayed allocated and valid. The selected successor
and object retain three payload paths sharing one inode. The test checks positive
metadata reclamation smaller than the payload, retained unique payload bytes,
and reference-minus-unique bytes of two MiB; it does not require these exact
filesystem-dependent allocation totals. Evidence:
`act2-generation-reclamation-evidence.log`.

The retention pressure loop must therefore use a fresh complete physical scan
after each mutation, including partial deletion, and continue toward its target
only while eligible candidates remain. Logical generation size is not a predicted
reclamation credit. Reaching a protected/unknown-only remainder above the cap must
report protected overflow rather than deleting selected/live/unknown state or
claiming convergence. This loop and object retirement are not implemented yet.


#### Generation policy implementation in progress

The local act2 `feat/tool-retention-policy` branch now has typed
`ToolRetentionPolicy` and `RetainToolStore` APIs. The positive allocated-byte cap,
explicit publication-age cutoff, maximum scanned entries (one million), maximum
namespace candidates (ten thousand), and payload-validation ceiling are validated
before mutation. The original catalog writer remains held throughout selection
verification, complete initial accounting, bounded candidate enumeration and the
sweep. Candidates are sorted by publication-directory modification time with an
ID tie-break; the current selected generation is always preserved. Older eligible
generations expire even below the cap; under pressure, younger unselected
candidates can also retire. Reader-lock timeout protects a candidate. Other
retirement failures mark the report partial. Every attempted mutation is followed
by a fresh physical observation, including failures that may have partially
removed data. Unknown namespace entries and all immutable objects stay intact.

Focused regression was RED with missing APIs, then GREEN for expiry, live-reader
protection followed by retry, unknown-state preservation and protected overflow.
Further tests pass for young-generation preservation below the cap, pressure
retirement and pre-mutation entry/candidate-bound refusal. Full artifactcache
tests passed; initial lint found excessive function complexity and a deprecated
bbolt error alias. The sweep is now a separate helper and uses the dependency's
current errors package; focused boundary tests and lint pass. Independent review
and final exact-source gate remain pending. No policy change is released or
activated in Bosn. Object/stage expiry, last-use evidence beyond publication age,
CLI integration, measured convergence across classes and production scheduling
remain required.


### Maintenance shutdown observation correction (2026-10-04)

The object/stage spec's required Bosn gate failed in the existing stalled-client
shutdown test: runtime teardown completed in 16.48 ms, but an immediate PID
probe still observed the owned process. The process-session drop contract sends
an asynchronous owner-drop notification; it does not synchronously prove reaping.
The original isolated fixture also failed once in 32 concurrent attempts.

A bounded reaping observation alone remained RED (2/64 attempts). Capturing the
probe output in another 64-attempt series identified four failures caused by
`ValueError: invalid literal for int() with base 10: ''`: cancellation observed
the marker after creation but before its PID contents were written. The fixture
now writes a private sibling and atomically renames it to publish a complete PID.
The probe waits for actual PID absence, and the final elapsed-time assertion
includes runtime teardown and observation within the original five-second
cancellation budget. A live 12-second negative-control process was rejected.

The corrected isolated focused test and all 64 attempts with four concurrent
workers pass. Independent review passes. These are diagnostic checks in the
private Rust harness, not the required gate on this exact source commit; that
gate remains pending. No production cancellation code changed. This evidence
does not prove that stopping a Docker client stops a remote command, nor does it
close the daemon-crash, uncertain creation/start, or engine-lifetime recovery
requirements.


### Verified object retirement and pressure-sweep integration

Act2 PR #38 adds `RetireToolObject(ctx, root, id, maxBytes, maxGenerations)`.
The original catalog writer spans complete current-selection validation, bounded
retained-generation manifest inventory, verified object payload/layout and mount
identity checks, removal and root-directory sync. Every retained generation
reference protects its object, including idle old generations and live readers.
Unknown namespace entries, missing/corrupt manifests, and exhausted reference
bounds refuse deletion. Candidate objects must have exactly the closed tree and
manifest control layout. Unknown state and unverified payloads stay intact.

Missing API regression was RED then GREEN: an object stays while selected,
while an old reader lives, and while its idle old generation remains; after that
last generation is retired, object deletion reclaims at least its one-MiB payload
allocation and the selected replacement remains valid. Strengthened tests use an
otherwise unreferenced orphan to prove unknown/missing/bounded reference refusal.
Complete package tests, vet/lint and cumulative review passed. Exact-source gate
passed in 20.65 seconds on `f63c19243cf128b35923e2d626bad303922b9344`, tree
`958851bf886d9e565ec5b4fad70490c6365c7d3e`, covering 2,578 exported files and
Darwin/Windows compilation. Evidence: `act2-tool-object-retirement-source-bound-gate`
JSON/log and `act2-tool-object-retirement-reference-checks.log`.

The following local `feat/tool-object-retention-policy` slice integrates object
retirement into the age/pressure sweep. A real failing regression showed that
retiring generation metadata alone left an unreferenced one-MiB object allocated.
It now passes: bounded object candidates are inventoried before mutation,
generation retirement runs first, then verified unreferenced objects expire by
age or pressure, oldest first. Each attempted mutation gets fresh accounting.
Typed reports distinguish retired/protected objects and generations. Referenced
objects are protected without claiming a failure; unknown/corrupt reference state
is partial and preserved. The catalog writer remains held throughout both sweeps.
Full package tests, vet/lint and review pass; exact-source gate on
`e9281ab` is pending. Production activation remains absent.

Stage expiry, automatic production scheduling, CLI entry point, last-use evidence,
7.7-GiB scan efficiency, whole-machine cross-class allocation and sustained caps
remain open. These slices have not been released or pinned into Bosn. Old
containers/images and scoped builder-cache pressure remain separate required
parts of the original objective.


#### Crash-leftover stage ownership gap

Source inspection confirms object and generation publishers create private stages
with `MkdirTemp`, report their paths and attempt deferred cleanup. They do not
persist ownership evidence for a later crash-recovery sweep. A stage-looking
prefix is therefore insufficient proof for deleting a discovered old directory.
The planned fix records typed relative path, creation time and original
filesystem/inode identity in synced ownership files while the original catalog
writer is held. The catalog descriptor remains a read-only mutex. Publication and cleanup must retire this record after successful
rename/removal. Crash windows before registration leave unknown preserved state;
a record after rename must never authorize deleting a replacement at its old path.

Focused new regressions are RED with missing stage-ownership and expiry APIs:
original catalog exclusion blocks expiry during a living publisher, registered
old stages expire, same-prefix unregistered directories remain, and replacement
inode identities refuse deletion. Stage cleanup must also apply bounded metadata
and mount-ID checks and report physical accounting after mutation. No stage was
deleted from a production/shared cache during this investigation. Evidence:
`act2-tool-stage-retention-red.log`. This work remains unimplemented.


### Durable stage ownership and bounded expiry (local implementation)

Candidate `32ce983` wires registered stage creation into both object and generation
publishers. Ownership records live in `.tool-publication-ownership-v1`, coordinated
by the original catalog writer. An attempted bbolt transaction failed in the
focused tests because coordination descriptors intentionally stay read-only; that
mutex contract was preserved and the design moved to separately synced files.
Each exclusive record leaf is the SHA-256 of a typed relative stage path, with
canonical schema-1 JSON (at most 1,024 bytes), original device/inode/mount identity
and UTC creation time. Stage parent, record file and ownership directory are
synced before payload fill. Records are bounded at ten thousand; creation refuses
at capacity before creating another stage. Unknown or malformed ledger state
refuses recovery rather than inferring ownership from names.

`RetireToolStages(ctx, root, expireBefore, maxEntries)` holds catalog exclusion,
requires an explicit age cutoff and bounded complete store inventory, and deletes
only registered expired stages with matching original inode and mount identity.
Descendant mount-ID validation rejects bind mounts; unregistered stage-looking
directories and replacement identities stay intact. Cleanup reports physical
allocation after each attempt. Publication/removal sync precedes record deletion.
A stale record whose original path is absent is cleared after parent sync without
touching any published destination. Failed registration retains `pending_stage`
for the private footprint. Deferred publisher cleanup uses bounded time derived
from the caller context with cancellation detached for recovery.

Verified regressions: original catalog prevents expiry while a publisher lives;
registered old stage expires; unknown prefix is preserved; replacement directory
identity refuses; a lost post-rename sync acknowledgement leaves an ownership
record that recovery clears while the published object remains fully valid.
Independent review caught creation of record 10,001 at capacity. Fixed regression
uses ten thousand actual records, proves refusal before stage creation, then
confirms an existing expired stage remains recoverable. Full package tests,
vet/lint and cumulative review pass. Final exact-source gate is running.
Evidence: `act2-tool-stage-capacity-check.log`,
`act2-tool-stage-publication-checks2.log`, `act2-tool-stage-split-lint.log` and
`act2-tool-stage-source-bound-gate` JSON/log.

Not released or activated in Bosn. Stage expiry is not yet part of the combined
retention scheduler or CLI. Earlier unregistered leftovers are deliberately
preserved until separate ownership evidence exists. Root-level selection JSON
stages are still a separate lifecycle requirement. Large-store efficiency,
production overlay/reader admission, whole-machine accounting and old engine,
image and scoped builder-cache expiry remain required.


### Combined sweep and native retention command

Act2 PR #41 runs owned stage expiry before generation/object retirement under the
same original catalog writer. Selection, complete initial accounting and bounded
candidate inventories validate before mutations. A partial stage result stops
later deletion; fresh stage allocation becomes the next sweep's observation.
Regression was RED with a registered one-MiB stage remaining, then GREEN for
stage removal, physical reclamation and selected-generation preservation. Full
package tests, vet/lint, review and exact-source gate passed in 22.40 seconds on
`edd6acc1e331c53883424229493153caa1e8628b`, tree
`4d980f78cf235b7d826bd2926dd1c4faf39e4124` (2,586 exported files).

Local CLI candidate `e6eaabc` exposes the sweep:

```sh
act cache tool-retain --cache-server-path /absolute/tool-store \
  --max-allocated-bytes 8589934592 --max-payload-bytes 17179869184 \
  --expire-before 2026-10-01T00:00:00Z \
  --max-entries 1000000 --max-candidates 10000 --apply
```

These are example policy values, not default production settings. All bounds and
an explicit RFC3339 cutoff are required. Missing `--apply` refuses before mutation;
`tool-usage` remains the read-only inventory command. The retention command emits
a typed JSON report before returning an error for partial results or protected
overflow. A complete pass that remains over cap therefore does not masquerade as
successful quota convergence. The cap is scoped tool-store inode allocation,
not machine-wide or Docker backing allocation.

Command regression was RED with the command/flags absent, then GREEN: omitted
`--apply` preserves an old generation, explicit mutation retires it, cap-one emits
complete protected-overflow JSON and nonzero status, and the selected successor
remains valid. Command vet/lint and cumulative review pass; final exact-source
CLI gate is running. Evidence: `act2-tool-stage-policy-source-bound-gate` JSON/log,
`act2-tool-retention-cli-red.log`, `-green.log` and CLI source-bound gate JSON/log.

The dependent PR chain has been reconstructed on current act2 master to resolve
conflicts from earlier squash merges; rebuilt trees exactly match reviewed trees.
PR #37's updated exact-source gate passed on `6684ba9`. Rebuilt later commits
still require their own source verification before branch updates. All changes
remain unreleased and unused by production Bosn. Automatic scheduling, frozen
engine profile and reader handoff, real workflow overlay/publication, selection
JSON stage lifecycle, large-store efficiency, whole-machine accounting and old
engine/image/scoped-builder expiry remain required.


### Retention and selection recovery checkpoint (2026-10-04)

The reconstructed act2 branch chain retains the previously reviewed source trees.
Each pushed candidate passed a fresh exact-source scoped gate: stage ownership
`dd8e94b` (2,585 exported files), combined stage policy `57fffbd` (2,586 files),
and retention CLI `9fa189c` (2,588 files). Coverage includes full artifactcache
tests, focused tool-command tests, vet, lint, and Darwin/Windows compilation.
These gates do not replace full CI on the exact default-branch release commit.
PRs #40, #41 and #42 are updated; their parent object-retirement PR #38 is waiting
for a rerun of Linux CI after an unrelated hosted-runner test hit GitHub's API
rate limit. Downstream conflicts must be resolved against each actual merged
parent before their remote gates and merge. No newer act2 release or Bosn pin
is claimed.

The remaining selection temporary-file ownership gap has a local implementation
in `8143cf3`. Selection JSON is now written inside the existing registered
`.tool-stage-` private directory while holding the original catalog writer.
The file and its directory are synced before the pointer rename. Successful
selection and stage cleanup leave no ownership record. A failed post-rename
root sync reports `Selected=true`, `Partial=true`, and the owned directory in
`PendingSelection`; cleanup deliberately preserves that uncertainty for recovery.
The existing stage expiry verifies the original inode and mount, then removes
only the private directory and its record. It preserves the selected pointer and
published generation; unknown historical `.tool-current-stage-` files remain
unregistered and cannot be deleted from a name or age inference.

The focused regression was RED because the prior implementation reported an
empty pending path after injected sync failure. It is now GREEN: recovery expires
the owned stage, pointer bytes remain identical, full selected-generation
validation succeeds, and a successful retry leaves no ownership rows. Full
artifactcache tests, focused command tests, vet/lint and cumulative independent
review pass. Final exact-source scoped verification passes for `8143cf3334a930a40a674a9bc8d0267cd0561642` (2,588 exported files, 29.73 seconds), including Darwin/Windows compilation. This is bounded
recovery of owned staging, not daemon-crash or remote-command lifetime proof.
Production handoff, real jobs sharing the warm generation across fresh engines,
whole-machine cross-class accounting, automatic expiry of owned containers/images
and scoped builder cache, and sustained pressure convergence remain required.


### Merged object retirement and rebased retention chain

Act2 #38's Linux rerun passed and the object-retirement primitive merged as
`e8b69c8`. PRs #39–#43 are now reconstructed on that actual merged parent.
Fresh source-bound gates pass before every branch update, and each reconstructed
tree is identical to its reviewed predecessor:

| Slice | Exact source commit | Exported files |
| --- | --- | --- |
| Object sweep (#39) | `8d84fc2e39974771030b7e6d8b4c6253d292d0d9` | 2,580 |
| Stage ownership (#40) | `c58390b1cb0cf27a988236dff4c66c99eb85ee50` | 2,585 |
| Combined stage policy (#41) | `f02fe9bf97231916035f7b71d19ff34baed9db4b` | 2,586 |
| Retention CLI (#42) | `bf47a59619ed8cb84bb2dcff1bd6700c79d53172` | 2,588 |
| Selection-stage recovery (#43) | `6db80530c2d81f6b517a35fe2c8d98dbafbe7bc2` | 2,588 |

The private Go harness's 6 GiB tmpfs filled during the final source export:
3.2 GiB of build cache, 849 MiB of modules, 283 MiB of lint cache and repeated
116 MiB source copies were observed. The incomplete export was not used for
push or verification. A new task-owned, read-only-root container with a 12 GiB
private tmpfs received the existing Go caches and a complete clean source export.
Disabling networking initially failed existing cache-server tests' outbound-IP
discovery; restoring the original bridge configuration fixed the harness. The
final source-bound gate passes in 74.24 seconds, with full cache tests, focused
commands, vet/lint and Darwin/Windows compilation, and 4.5 GiB observed tmpfs use.
This is test-harness capacity evidence, not proof of production cache convergence.

Bosn's committed shutdown-fixture correction passed its exact-source Rust lane
in 669 seconds with clean-source and executed-step proof. Its Linux lane remains
running; no completed full Bosn gate or publication is claimed yet. Act2 remote
checks are running on the reconstructed heads. Preserve ancestry with ordinary
merge commits for this chain where repository settings permit; exact default-
branch full CI and artifact checks remain required before the necessary release.
No release newer than act2.7 exists, and Bosn still has no activation of these
shared-tool-generation retention features.


### Production bootstrap boundary: current code and required change

Inspection of `act_engine/create.rs` and `ci/engine.rs` confirms that the
controller currently registers and starts the created engine before
`prepare_engine` waits for readiness and installs the pinned act binary.
`prepare_run` then copies the legacy tool cache into the inner volume. The current
PID-1 command executes `ENGINE_INIT` directly; it does not admit or retain a
shared-tool-generation reader. The earlier native-exec experiment therefore
does not establish production reader ownership.

The production change must freeze the tool-store mount, chosen generation and
act artifact digests together with the exact startup command. Bootstrap can
reuse the current digest-verified cached archive installation before entering
the existing engine INIT, then replace PID 1 through `act cache tool-exec` so
the original reader descriptor survives into INIT and the running engine.
Installing act only after readiness cannot supply that startup guarantee.
The inner writable tool-cache volume must be an overlay of the verified
read-only generation and private upper/work directories, rather than the
current whole-tree seed copy.

Downloading or staging before reader admission leaves a possible retirement
race. The helper must refuse missing or invalid chosen generations; the
controller must replan/retry from the latest valid warm selection, never
substitute an empty cache. Durable engine ownership remains required before
creation/start, recovery must validate the frozen profile, and uncertain start
or cleanup must not release protection based only on client cancellation or
daemon death. Completed installs may publish successors only after actual
workflow writers are quiescent. These are planned production requirements,
not implemented behavior or completed lifetime proof. Large-cache admission
currently hashes full payloads and still requires measured optimization.


### Publication checkpoint after passing gates (2026-10-04)

Act2 #39, #40, #41 and #42 are merged with ordinary merge commits after all
remote checks passed on their exact reviewed heads. Their ancestry is retained
for the remaining selection-stage PR #43, now retargeted to master. Its Linux
check remains pending. Full release CI on the final default-branch candidate
has not yet been dispatched, and act2.7 remains Bosn's pin.

Bosn #497 is merged as `51df270e`: the atomic PID fixture and bounded reaping
observation passed the complete local gate in 1,379 seconds (Rust669s, Linux710s).
The pinned verifier accepted stamped source `9407300a38d78bfb27fda619a47f52145579812f`
and remote local-gate attestation/minimal checks passed. Earlier pending-gate
statements describe the preceding checkpoint. Production cancellation behavior
is unchanged; remote-command and engine-lifetime proof remains outstanding.

The queued object/stage/CLI/selection documentation is being consolidated onto
this fixed Bosn main so the required documentation publication gate includes
the corrected shutdown test. This checkpoint does not claim an act2 feature
release, production generation admission, machine-wide convergence, or automatic
expiry of all owned engine/image/builder-cache classes.


### Actual hosted-workflow sharing: metadata copy-up regression

A private nested-Docker experiment built act from the exact reviewed source
`6db80530c2d81f6b517a35fe2c8d98dbafbe7bc2`, used Bosn's pinned Ubuntu runner
image, and mounted a task-owned read-only lower containing a 16 MiB payload and
completed install marker. Two actual dependent act workflow jobs read the same
payload; the second observed the first job's settings change. Both workflows
completed successfully, but the physical-efficiency assertion was RED: the
runner's recursive ownership reconciliation copied the entire payload into the
private upper. Its payload allocated 32,768 blocks (512 bytes each), and the
upper grew from zero to 16,404 KiB. Plain Docker jobs had not exercised that
hosted-user setup, so their earlier small-upper result did not cover this case.

The same experiment is GREEN with the tool overlay mounted `metacopy=on`,
confirmed in the live engine's mountinfo. Two actual hosted workflow jobs share
changes in the first engine; a job in a second fresh engine reads the original
warm lower. All three payload digests match. Upper growth is 20 KiB and 12 KiB;
upper payload entries have logical size 16 MiB but zero allocated blocks. Small
settings writes copy their data while the shared payload remains in the lower.
Both exact owned engines and the lower volume were removed after label checks.

[Kernel OverlayFS documentation](https://docs.kernel.org/filesystems/overlayfs.html#metadata-only-copy-up)
explains that metadata-only copy-up delays payload copying for ownership/mode
changes until a write needs file data. It requires trusted upper/lower layers;
production must enforce that boundary, check support and actual mounted mode,
and refuse/replan rather than silently regress to full payload copies or an
empty cache. This fixture used fresh task-owned layers, not arbitrary imported
overlay metadata.

This establishes actual act workflow sharing and the metadata allocation
requirement in a controlled prototype. It does not prove Bosn's frozen
production profile, immutable-generation/native-reader admission, actual
completed-install publication, daemon/uncertain-start recovery, sustained
whole-machine pressure convergence, or cache-action sharing. Artifact and
report evidence: `retention-toolcache-cow-actual-workflow-metadata-copy-red` and
`retention-toolcache-cow-actual-workflow-experiment` JSON/log/script under the
session git artifacts. First transport, minimal-runner-image and capacity
failures are retained separately; their owned fixtures were cleaned. The pinned
runner needed about 2,752,220 KiB of inner image storage, and the successful
fixture gave each engine a 4 GiB private storage tmpfs instead of 2 GiB.

### Release and documentation publication checkpoint

Act2 #43 is merged as `2029595941c40866b7114bf62b33d52ea54fc781`. Full default-
branch CI run `37226748284` passed all five required jobs on that exact commit.
A clean default-branch candidate checkout, absent tag/release and unchanged
remote master were verified before pushing necessary tag `v0.2.89-act2.8`.
Its existing tag-triggered release workflow is publishing; binary/checksum
verification and Bosn pin update are still pending. No runtime activation is
claimed.

The consolidated object/stage/CLI/selection spec passed the complete pinned
Bosn gate in 921 seconds (Rust474s/Linux447s), and exact verification accepted
stamped source `e68f8fe5e7b387ca77a67491898090752632a907`, tree `ea1e13959129`.
Independent documentation review passes; its PR is being published. This
follow-up records the newer workflow experiment and release evidence.


### Verified act2.8 artifact and Bosn pin candidate

The release workflow `37227718381` succeeded on the exact fully tested candidate
`2029595941c40866b7114bf62b33d52ea54fc781`; the annotated release tag resolves
to that same commit. All eleven release archives match both the checksums file
and GitHub asset digests. Their contained binaries have the expected target
architecture and embedded version. The actual Linux x86_64 binary reports
`act version 0.2.89-act2.8` inside the private Linux container; released
`tool-retain` and `tool-exec` command flags are present.

The Linux x86_64 archive digest is
`95b1b7f01da6f5ca22e206847d419c80fa04198f0cd568f986f96448ba7230d4`;
its binary digest is
`743c13bf6c8ee8ff948a14f940f6d1033ab4e09d11d29d94fda8e1bbaeea8628`.
The Bosn candidate updates the one ACT_VERSION constant, artifact URL and both
digests together. The stock runner image remains the pinned published image.
Source review and Bosn's required source-bound gate are pending before push.
The consolidated spec is now merged in Bosn #499, main `fe0a3d3d`.

This pin makes the verified retention and native-reader CLI available to Bosn
engines; it does not enable the new production generation profile, metadata-only
overlay mount, quiescent successor publication or whole-machine scheduling.
The default engine still follows the inspected legacy copy path until those
production changes are implemented and verified.


### Released-binary workflow verification (2026-10-04)

The same actual hosted-runner workflow fixture was repeated with the verified
released Linux x64 binary from `v0.2.89-act2.8`, rather than the source-built
prototype CLI. Its binary SHA-256 is
`743c13bf6c8ee8ff948a14f940f6d1033ab4e09d11d29d94fda8e1bbaeea8628`.
All three jobs passed: two dependent jobs in one engine shared their writable
cache, and a fresh second engine began with the same warm immutable payload.
The first engine's settings mutation was visible to its dependent job and
did not change the shared lower or the fresh engine's initial settings.

All jobs read the same 16 MiB payload digest
`811e721a3e02f4407710f89f854e913d9f5f661104f7e41c8610c58b13705f9d`.
The upper payload had zero allocated blocks in both engines, despite hosted
runner ownership setup; their final upper allocations were 20 KiB and 12 KiB.
The fixture verified the metadata-only overlay mount mode. Both exact engine
IDs and the shared lower volume were removed after checking their original
experiment ownership labels. Evidence: local audit artifacts
`retention-toolcache-cow-released-v8-workflow.{py,json,log}`.

This verifies the released executable against the private workflow fixture.
It does not activate production generation admission, prove daemon recovery
or completed-install publication, measure the real 7.7 GiB cache's admission
cost, or establish machine-wide accounting and automatic expiry. Production
still installs act after engine startup and uses the whole-tree tool seed.
The next implementation must change that startup and recovery contract before
replacing the production seed with the verified shared-generation overlay.


### Production startup candidate: verified act before engine INIT (2026-10-04)

After pin PR #501 merged as `139953f509075ff59a60c19d49414e5abe1e3f9f`,
the next source candidate changes the actual cache-backed engine command.
`creation_profile_with_cache` now freezes a bootstrap command that installs
the pinned archive, checks both archive and extracted binary SHA-256, closes
the archive writer descriptor, and replaces itself with the existing PID-1
INIT → docker-init → dockerd chain. The shared install script is extracted
into `ci/engine/act_install.rs`; startup and post-readiness verification use
the same implementation. Creation refuses an act version/archive identity
that differs from this startup artifact and refuses a noncanonical cache
mount. Engines without the cache retain their original command identity.
Historical engine recovery still checks its recorded command digest; it does
not substitute the new producer's command. This source is not yet published.

The focused regression was RED on the old command and GREEN after the change.
A second RED showed that creation accepted a mismatched recorded artifact;
it is now refused. All 21 engine-boundary tests passed. The compiled Rust
command generator was also exercised in the private Docker daemon: a valid
released archive started dockerd 29.7.2 in 1.66 seconds, exposed the verified
act2.8 binary, and allowed exclusive acquisition of the archive writer lock
while the engine remained live. A corrupt cached archive with no network
exited with code 1 after 5.46 seconds. The exact observed Docker command
matched the compiled generator. Both engines and the cache fixture volume
were ownership-checked and removed. Audit evidence is
`retention-bootstrap-{focused-red,focused-green,identity-red,identity-green,generated-command,runtime-proof}`
(local logs and the runtime proof's Python/JSON artifacts).

Because installation now precedes readiness, readiness has the previous
30-minute install allowance plus the previous 60-second daemon allowance.
It parses Docker's startup state into a typed struct after failed daemon
probes and immediately refuses stopped or unprovable engines; it does not
wait the download budget after an observed exit. Each probe uses the smaller
of the remaining total allowance and the control-operation budget. The typed
state refusal test passed. Full source gates and cumulative review are still
pending for this candidate.

This moves the production bootstrap boundary needed for native generation
admission. It does **not** acquire a generation reader yet: the production
planner still selects the legacy cache route, and `prepare_run` still copies
the legacy tool tree. Frozen generation selection/native exec handoff, trusted
metadata-only overlay mounting, recovery/lifetime proof, quiescent completed
install publication, large-cache optimization, and whole-machine convergence
remain unfinished. The warm-archive runtime proof does not measure cold
network download latency.
