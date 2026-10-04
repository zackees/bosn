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
   eventually remove their exact nested engine and anonymous storage. Cleanup
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
| 4 | Account for and expire eligible old CI engines, host images and build cache | Fault/restart live Docker tests, exact ownership checks, repeated-run footprint trend | Existing lifecycle passes live end-state/restart tests; image/build-cache attribution, expiry and online failure recovery open |
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
