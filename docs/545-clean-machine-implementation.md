# Clean-machine lifecycle implementation (#545)

Tracking issue: https://github.com/zackees/bosn/issues/545

This is an implementation and evidence record. The issue remains open until all
acceptance criteria are demonstrated and the fixing PR is pushed and merged.

## Verified progress

- Automatic retention defaults on; an explicit `auto_retention = false` opts out.
- Legacy setup containers and manifest volumes enter discovery through their
  actual immutable labels. Reconciliation requires an exact registry record,
  observed engine name, kind, and generation digest. Canonical foreign ownership
  is never overwritten. Explicit engine pins remain pinned.
- Newly ensured setup/manifest containers and images use warm retention instead
  of an unconditional factory pin. Declared volume pins and guest pins remain.
- Idle manifest retirement is serialized with job admission, checks durable
  sessions/leases/intents, and verifies that the only processes are the fixed
  shell/sleep keepalive before graceful stop. Confirmed task completion refreshes
  the stack's idle clock; uncertain execution retains its session.
- Apply refreshes the census between containers, volumes, and images, so released
  references become eligible during the same invocation. Global candidate and
  byte budgets are carried across stages.
- Registered image discovery verifies exact inspected Docker digests against
  setup/manifest image registry records. Shared pins and latest use protect the
  image, as does any remaining container reference.
- Reports expose held reasons and report actual removal counts.
- Removal rechecks measured size against the remaining byte budget, deferring
  growth or missing measurements instead of exceeding the selected ceiling.
- A machine catalog now records initialized registries at daemon startup.
  Offline peer collection acquires a read-only registry snapshot with writer
  exclusion across its whole pass, verifies identity, and shares object/byte
  caps with the current registry. A focused test proves an active writer is
  refused and another daemon cannot acquire the peer during collection.
  All cataloged registries contribute recent use, pins, leases, sessions, and
  volume-intent protections for shared objects. Explicit peer opt-outs protect
  their resources from another registry's sweep. A machine-wide shared/exclusive
  gate permits concurrent jobs but prevents setup/manifest/CI creation or exec
  from starting between a retention read and removal. Job waits remain
  cancellable and consume the selected deadline. Rollout still requires the
  deleted-state recovery and remaining evidence below.

## Reproduction evidence

All tests run inside a Docker container with `BOSN_TEST_ISOLATED=1`. Compilation
uses `soldr cargo`, with sources in the isolated issue worktree.

The production-path integration test is
`crates/bosn-service/tests/managed_retention_docker.rs`. Its first failing run
left the running manifest container because creation had pinned it. After
correcting factory retention, its second failing run removed the container but
left the warm volume because the census had been collected before removal.
Staged refresh made that same reproduction pass.

The expanded live test builds a unique image through a real manifest Dockerfile,
executes a declared task writing 16 MiB into a warm volume, measures at least
16,384 KiB allocated using `du`, and verifies exact container, warm-volume, and
image disappearance after one apply. A declared pinned volume survives. The
fixture then removes only its own pinned volume. This passed in 4.35 seconds.

After image reconciliation, the full service unit suite passed:
545 passed, 0 failed, 18 ignored. The earlier environment failures were resolved
by mounting the checkout at its compiled absolute path and supplying git and uv
inside the test container. Rerun after subsequent changes before treating this
as final evidence.

After machine admission and shared ownership changes, the full service unit
suite passed: 550 passed, 0 failed, 18 ignored. The two-daemon production test
`tests/retention_peer_docker.rs` passed in 9.18 seconds: a foreign declared task
fenced GC; after that daemon stopped, the machine daemon reclaimed its container,
warm volume, and image, preserving the declared pinned volume. A repeated pass
accepted proven absence. The expanded opt-out test passed in 7.46 seconds.
The further shared-container test passed in 19.16 seconds: two real ensure paths
recorded the same physical container, a foreign task remained admitted until
explicitly released, the primary's own idle clock expired, and foreign task
completion refreshed shared use sufficiently to preserve the container. The
peer opt-out then protected the shared resources; re-enabling it allowed full
reclamation and a repeated empty pass. The keepalive check now requires exactly
one shell and one sleep process, rejecting additional shell/sleep tasks.
All 34 focused retention unit tests passed after the report and process-check
changes. Final lint/review and the complete issue acceptance audit remain pending.

A subsequent live run exposed another defect: `Client::managed_retention` used
the three-second status RPC deadline and timed out while GC was still running.
It now uses a separate 30-minute reply deadline; individual Docker reads and
removals retain their existing bounds. A total daemon pass budget remains to be
implemented before this is a complete latency guarantee. Fixture teardown now
retains exact registry records and repeats volume ownership proof even on panic.
The failed run's one leftover pinned fixture volume was identified from Docker
events carrying its unique `bosn.peer.fixture` root `/tmp/.tmpBNnDMc`; re-deriving
the production volume digest for stack `app`, volume `durable`, and that workspace
matched `5c0f124edf7adf0c48e707f5c262105bfcc2c800a2c57379af710f20f2c17b4f`
exactly. Only that verified fixture volume was removed.
The live shared-registry test then passed in 22.69 seconds with the dedicated
reply deadline and ownership-checked fixture guards. A subsequent Docker listing
found no remaining setup-managed fixture volumes.

## Current acceptance audit

This is an interim audit of the local implementation checkpoint, not a
completion claim. Issue #545 remains open; no PR has been created for
`fix/545-clean-machine`. The journal below records historical progress and may
mention tasks that were subsequently completed. Final delivery still requires
the remaining acceptance audit, review, push, required checks, and merge.

| Requirement | Current evidence | Remaining proof or implementation |
| --- | --- | --- |
| Production RED → GREEN reproduction | Manifest metadata/pin/one-pass repro and live lifecycle driver | Full supported CI creation coverage remains unproven |
| Default automatic retention | Missing config enables retention; normal frontdoor registers maintenance; Linux and macOS generated units tested | Actual manager liveness, upgrade behavior, and supported platform coverage |
| Every creation path | Prepared image/container/volume checkpoints; twelve live lifecycle cases including native daemon death after image export and physical deletion, whole-state-path loss and immutable pull recovery with pin preservation | Missing-export intent retirement, CI/act creation and abandonment audit |
| Existing alternate labels | Exact registered identity/generation bridge | Historical objects whose ownership rows are missing require verified recovery evidence |
| Temporary/multiple/deleted state | Stable authority, publication-boundary restarts, abrupt native daemon death, whole-state restart, opt-out preservation | Uncataloged historical state and unobserved policy-change loss; crashes inside publication operations |
| Idle retirement | Exact fixed manifest launcher, fresh jobs/protection/process checks, TTL, oldest-first capped stops | Other persistent keepalive origins need explicit provenance/lifecycle proof |
| One-pass ordered reclamation | Live manifest case verifies container/volume/image absence and measurable volume storage; failed-start case verifies intent-backed volume cleanup | Final exact candidate SHA verification |
| Actionable diagnostics | Read failure bounds/totals, held decision totals, zero-candidate reports, manager-confirmed autostart status | Distinct lease/session/intent reasons and full diagnostic scope audit |
| Repeated/abandoned storage bounds | Physical lifecycle cases, removal/time/read budgets, inventory memory ceiling | Confirmed deletion prunes resource rows and matching intents; crash/absence recovery, events/history bounds, repeated-run storage proof, nested act/external builder scope |
| Live host confirmation | Incident cleanup evidence; native Docker fixture results | Exact final source SHA/version, host inventory, protection and cleanup evidence on the candidate |
| Delivery | Local work only; no PR exists | Final review, commit, push, required checks, merge and issue update |

### Registry cleanup boundary

Confirmed physical deletion now produces typed receipts. The admission guard
moves from the blocking retention worker into the registry actor worker and
survives caller cancellation until the transaction finishes. Offline peer apply
holds both machine ownership and registry writer fences through receipt cleanup.
Shared objects are matched against exact recorded metadata in each offline
registry. Live Docker checks prove reclaimed resource rows and volume intents
are removed while pinned ownership remains. Resource use deletion cascades.

Remaining accounting work includes shared records in other active registries,
bounded events/job/generation history,
and repeated-run SQLite storage evidence. The 65536-record refusal remains a
memory safety ceiling, not a steady-state storage bound.

### Deletion crash boundary

The physical mutation boundary now persists and syncs a bounded deletion intent
beside the resolved machine authority before Docker deletion. Replay requires
fresh complete absence evidence and the transactional pin/lease/session/new-use
checks before pruning the exact old incarnation. A native fault test pauses
after successful Docker image removal, kills Bosn before accounting, moves the
entire original state directory away and restarts. It verifies the same registry
UUID, removed ownership/intent and zero additional claimed physical removals.
A reopen regression covers protected rows, replacements, already committed
accounting and incomplete engine reads. Repeated-run storage evidence and
broader supported creation coverage remain outstanding.

## Implementation journal

The shared ensure pipeline now checkpoints the inspected prepared image before
any container inspection/create/start. The native setup job supplies its actor
recorder through `execute_recorded`; the manifest executor supplies the same
recorder directly. Typed `PreparedImageOwner` derives setup and manifest image
namespaces/stacks in one helper, reused by the final manifest receipt, so the
early checkpoint does not create a duplicate image ownership row. A regression
rejects the checkpoint for both owner kinds and verifies exact image ID/stack
facts and zero container operations afterward. All 12 setup-rollover/pipeline
tests passed in Docker, and full lint passed. The exact CI Docker driver passed
the real manifest lifecycle (3.96 seconds), peer recovery/opt-out (20.84 seconds),
and all three prepare/task/app-task image cases (3.23 seconds). This protects
failures after successful image inspection; interrupted preparation before
that proof and container creation before final registry handoff still require
durable intent/recovery coverage.

Setup app-task preparation now sends inspected image facts through the existing
actor-owned session recorder before application ownership inspection. A missing
managed application therefore leaves reclaimable image ownership instead of
dropping it with the failed receipt. Task-only execution now refreshes the image
clock after its execution primitive returns, including declared-task failure.
The expanded live target passed all three cases in 3.37 seconds: prepare-only
success, declared-task exit 7, and the exact missing-application ownership error.
Each case verifies one Warm image row and subsequent exact image disappearance.
Full lint passed with project dependencies available; a fixture image inventory
was empty afterward. Setup ensure/manifest preparation failures before durable
container handoff, interrupted builds, and crash-dirty recovery remain open.

Task-only setup execution now receives a typed image recorder from the job
runner and persists the inspected prepared image before the declared task begins.
Consequently task failure or cancellation after preparation retains image
ownership rather than losing it with the receipt. The real Docker preparation
acceptance target now covers both prepare-only success and a task exiting with
code 7; both cases assert one Warm image row, no leftover task container record,
and exact image removal by owned GC. Both live cases passed in 2.91 seconds,
including an assertion of the actual declared-task exit-7 error. The existing
prompt/coalescing/bounded-output/cancellation unit test passed in Docker (0.28
seconds). A label-filtered inventory found no preparation fixture images left.
This does not cover interruption before the inspected image reaches the recorder,
nor preparation in all app/manifest/CI failure paths.

Managed retention now starts a ten-minute scoped work budget in both its public
synchronous entry and the daemon blocking worker. Nested work retains the earlier
deadline, so peer sweeps cannot reset it. Every engine invocation clamps its
individual deadline to remaining time; registry/protection pagination, peer
accounting, and idle stops check expiry before continuing. Apply defers remaining
candidates and reports an incomplete pass on expiry while retaining counts for
completed removals. Two regressions prove nested deadline preservation/restoration
and refusal of an already-expired apply before any engine access. All 41 focused
managed-retention unit tests passed in Docker (0.31 seconds), and rustfmt plus
`git diff --check` passed. This is a work deadline with bounded in-flight metadata
reads, not a guarantee against a hung host filesystem. Repeated-run storage
ceilings and the remaining lifecycle coverage are still open.

Full lint passed with the declared Python dependencies available (the first
plain-system-Python invocation lacked PyYAML/pytest and was not a valid gate
environment). Factory image/container refreshes now preserve an existing pin
through `put_resource_preserving_pin`; explicit retention updates and declared
volume policy still use the ordinary typed upsert. The prepared-image regression
proves new images start Warm but an explicit pin survives subsequent preparation.
The complete service unit suite exposed a close-only file-lock release race
under concurrent subprocess spawning. Machine owner/admission guards now unlock
before closing; a deterministic duplicated-descriptor regression proves release
is immediate and that closing the inherited descriptor does not release a new
owner's lock. After rebuilding, the complete service unit suite passed inside
Docker: 556 passed, zero failed, 18 ignored, 33.47 seconds. Historical factory
pins remain preserved until their provenance can be distinguished safely.

The existing Linux `ci-test`/full test tier now invokes
`ci/run_retention_docker.py`; no new workflow was added. The runner explicitly
builds the three ignored native acceptance targets through Soldr, verifies one
executable per target, and runs each inside a network-disabled Docker container
with the source/target mounted read-only and the Docker socket exposed for the
production fixtures. It provisions only pinned Python/Alpine images when absent.
Running that exact driver locally passed all three targets: manifest cleanup
4.09 seconds, peer recovery/opt-out 22.24 seconds, prepare-only reclamation
2.48 seconds. Ruff formatting and lint checks passed for the driver. This closes
the gap where Rust ignored acceptance tests were absent from the existing gate.

Prepare-only setup jobs now return typed inspected image ownership through
`SetupPrepareExecution`, and the job runner records that image through the sole
registry actor before exposing successful completion. Preparation and setup
ensure share the same image upsert helper (Warm retention, exact inspected
digest, resource-use record). Existing non-Docker executor test seams retain
their receipt-only behavior. The new real Docker preparation test passed in
3.01 seconds: the actual production inline-Dockerfile path recorded one Warm
image and no container, then owned GC removed that exact image. Fixture teardown
uses a unique build label and repeats its exact value before removing any image.
The existing prompt/coalescing/logging/cancellation test also passed in its
isolated container. Creation interrupted before registration still needs
durable intent/provenance coverage; the successful prepare-only gap is closed.

The live recovery test now also relocates the opted-out peer directory before
an apply pass. Recorded `auto_retention = false` survives source loss, removes
zero objects, reports the opt-out, and preserves its running idle container.
Restoring the source, recording true in a preview, then removing the source
again permits the subsequent recovery cleanup. This expanded Docker test
passed in 23.93 seconds. Recovery now additionally requires the saved retention
setting to be a bounded regular file with exactly one of the two values Bosn
records; missing, malformed, or oversized snapshot settings refuse recovery.
All four peer/recovery unit tests passed in a container after that tightening.
The current issue body was re-read and remains open with its full original
acceptance scope; these checks do not prove the remaining creation paths or
bounded repeated-run storage.

Peer GC and shared-use accounting now recover from a clean machine snapshot
when the original registry path is absent. Recovery checks snapshot status and
the exact catalog UUID; removal retains both the machine owner lock and the
snapshot database writer fence. Original intact registries retain the existing
writer protection. Startup and intact-peer observations atomically retain the
current retention setting beside the snapshot. The three peer unit tests passed
in a container, including clean recovery and refusal of dirty snapshots or live
machine owners. The expanded real Docker test passed in 24.16 seconds after
relocating the original peer directory before collection: its idle container,
warm volume, and image disappeared, its pinned volume survived, and repeated GC
reported no removals. Opt-out changes never observed before source deletion,
crash-dirty recovery, and historical uncataloged state still require an explicit
solution; clean recovery alone does not satisfy the whole issue.

Machine-level ownership snapshots are now enabled at daemon startup under the
stable owner lock. Every registry transaction publishes a durable dirty marker
before it starts; the actor exports committed SQLite state before replying and
then publishes a clean marker. Both renames sync their directory. Reads avoid
redundant exports when no transaction ran. A failed export leaves dirty state
and prevents acknowledgement of success. All 12 registry unit tests passed in
a container, including WAL export and dirty/commit/publication sequencing.
The real cross-registry Docker test passed in 20.92 seconds and additionally
verified a clean machine snapshot containing the newly created resource IDs,
generation identities, and retention settings. Recovery does not yet consume
these snapshots: clean-snapshot validation, deleted-state opt-out preservation,
and crash-dirty reconciliation still require implementation and tests.

Daemon startup now retains a machine-catalog ownership lock for its entire
serve lifetime. Offline peer GC acquires that same lock before inspecting the
original directory, and still acquires the database writer fence for older
daemons. Two container-run peer unit tests passed: machine ownership exclusion
survives relocation of the original state directory, and database writer
exclusion still spans peer GC. This protects live owners during future snapshot
recovery; it does not yet authorize collection from a deleted directory.
After rebuilding the native daemon and integration binary, the real Docker
cross-registry test passed in 19.98 seconds with the machine ownership lock:
active work, shared idle clocks, peer opt-out, offline retirement, pinned-volume
protection, and a repeated empty cleanup pass all remained covered.

The registry now exposes `backup_ownership`, using the already available
SQLite backup facade to export a consistent standalone database. Its container
regression test passed with a committed record still in the source WAL, verified
the registry UUID and exported record, and confirmed that a second export cannot
overwrite an existing destination. This is the snapshot primitive only: durable
publication, mutation fencing, freshness proof, and catalog recovery are not yet
connected, so deleted-state ownership recovery remains incomplete.

Failure diagnostics now retain at most 64 details, each at most 2,048 UTF-8
bytes, across staged and peer merges. The total failure count remains exact;
maintenance output reports omitted details. The container-run regression test
feeds 1,024 oversized Unicode failures and verifies both bounds and valid
truncation. It passed after rebuilding the service unit binary; `git diff
--check` also passed. Held-detail accounting and deleted-state recovery remain
open requirements.

1. Machine-wide reconciliation for multiple and abandoned state directories,
   including retained ownership proof after a temporary directory disappears.
   An active foreign registry must retain writer exclusion and workload safety.
   Merely ignoring `ForeignRegistry` is not acceptable.
   The catalog, admission gate, and clean ownership snapshots cover known
   intact and disappeared registry directories. Crash-dirty snapshot recovery
   remains incomplete. Catalog recovery must also account
   for historical states and daemons that predate the machine admission gate;
   an unknown registry cannot be assumed inactive or unpinned.
2. Safe handling of historical factory pins, distinguishable from explicit pins.
3. Creation/retirement agreement across setup and act/CI paths, including failed
   and interrupted creation, and active CI execution fencing.
4. Explicit, tested storage/object bounds under repeated and abandoned runs;
   nested act caches and external BuildKit scope must be documented. Add a total
   daemon GC pass budget so the client reply limit is backed by a server bound.
5. Startup/service installation evidence that default automatic maintenance runs
   after installation and upgrades, including independent state bootstrap.
6. Complete held/deferred/unknown diagnostics and bounded report truncation.
7. Expanded safety integration coverage for running tasks, leases, and pins;
   final lint/review and live inventory evidence naming exact version and SHA.
8. Push PR, satisfy required checks, merge, and record final acceptance evidence
   in #545 before closing it.

The live test establishes the manifest lifecycle on one initialized registry.
It does not establish cross-state recovery or a global storage ceiling.

Startup investigation found that service-manager registration used an unbounded
child wait despite its bounded-command contract. SystemRunner now limits each
command to 15 seconds and kills and reaps a timed-out child. All seven autostart
unit tests passed in the isolated Docker test environment, including a real
child timeout regression. Default registration and independent state bootstrap
remain incomplete; this change only establishes a bounded registration primitive.

Autostart now has a default-registration API and persists an explicit disable
choice, including disabling before a unit exists or when unregistration fails.
Explicit successful enable clears that choice. Nine isolated autostart tests
passed, covering first registration and both opt-out lifecycles. CLI startup
wiring is still pending the independent machine-state bootstrap safety change;
registering a temporary workspace path as the persistent service would strand
maintenance when that directory disappears.

Independent-state bootstrap now checks exact recorded container/volume names
against validated peer catalog identities before allowing coexistence. Unknown
objects, incomplete probes, and same-path registry loss still refuse. The
negative catalog-only proof regression passed in Docker. Positive bootstrap
integration coverage and immutable image probe identity remain pending, so
default persistent service wiring is not yet complete.

Prior-object image observations now retain an immutable engine digest alongside
the human repository tag. Independent bootstrap compares that digest with the
registered image generation, rejecting missing or malformed digest proof. Both
bootstrap unit tests passed in Docker, including a mutable-tag observation that
retains the exact digest. Positive multi-registry startup and default service
wiring remain pending.

The bootstrap acceptance regression now records real SQLite container, volume,
and image resource rows while the peer writer remains open. It proves that a
distinct new state can account for all three, while same-path replacement, an
incomplete census, and a changed image digest refuse. All three focused
bootstrap tests passed in Docker. This is ownership-check coverage; persistent
service startup and platform integration remain pending.

Normal run/CI daemon startup now attempts default Linux/macOS maintenance
registration against the stable machine state path, independently of the
workspace state override. Explicit autostart disable is honored, registration
failures are visible while workspace startup proceeds, and CI/isolated tests
skip host service registration. The native bosn binary passed Soldr Cargo check.
Real service-manager integration, Windows persistence, idempotent macOS
registration, and upgrade behavior remain unverified or incomplete.

Repeated matching macOS default registration now checks launchd before
loading again; an unloaded matching file retries registration. Eleven isolated
autostart tests passed. The complete current service unit suite also passed
in Docker: 567 passed, zero failed, 18 ignored (34.24 seconds). Actual hosted
macOS service registration and changed-unit upgrade handling remain pending.

Idle retirement now verifies the complete production login-shell argv rather
than accepting any script ending in the fixed keepalive. A caller-prefix
regression and process-table safety tests passed. The actual production Docker
acceptance driver then passed all five tests: manifest reclamation, active and
relocated peer safety, prepare-only, failed-task, and missing-app image cleanup.
Generic setup apps cannot be inferred idle merely from sh/sleep processes; their
retirement contract still needs explicit production provenance.

CI background spare planning/filling and existing cache-cohort maintenance now
hold shared machine retention admission, covering image pulls, object creation,
and durable handoff. Failed spare admission clears the pending fill normally;
maintenance cancellation defers without engine mutation. All 14 spare-related
unit tests passed in Docker. These unit fixtures disable the machine catalog,
so they validate lifecycle compatibility rather than live cross-daemon fencing;
that integration coverage remains required.

Retention ownership reads now refuse above 65,536 combined resource and
protection records per registry or across peer ownership inventories. Refusal
preserves complete protection instead of silently truncating records. All nine
registered-ownership unit tests passed in Docker. This bounds loaded ownership
metadata; it does not yet bound persistent database history or physical storage.

Full repository lint passed after the startup and inventory-bound changes.
Stopped-container reporting now uses the same recorded-use-adjusted idle age
as retention, with unknown usage represented conservatively as zero eligible
age, rather than declaring a recently used old container past its TTL. All
46 managed-retention unit tests passed in Docker after this change. A focused
recent-use report regression remains desirable before final acceptance.

The recent-use report regression now exercises the complete production
retention observation path with a synthetic Docker CLI and a real SQLite
resource row. A container created in 2020 but used just now reports less than
60 seconds idle and zero containers past the gate. The focused test passed
in Docker; this supplies the previously pending direct report evidence.

Generated maintenance entries now quote and escape Linux executable/state
arguments and XML-escape macOS plist paths. All twelve autostart unit tests
passed in Docker, including spaces, quotes, backslashes, expansion characters,
and XML metacharacters. Systemd quoting reference: upstream
https://raw.githubusercontent.com/systemd/systemd/main/man/systemd.syntax.xml .
Actual platform parser/runtime validation remains required.

The current native CLI generated an autostart entry inside an isolated Docker
fixture with a stub service-manager command. Its state path contained spaces,
percent specifiers, and dollar expansion characters. The host systemd 260.2
parser accepted that exact generated entry via `systemd-analyze verify --user`
with exit zero and no warnings. This proves parser compatibility, not daemon
liveness or restart behavior. The disposable fixture was removed with safe-rm.

Automatic owned retention now runs in a worker independent of the unmanaged
size census, so the latter no longer precedes and stalls cleanup. Both workers
retain cancellable intervals and owned deletion still goes through admission.
The complete service unit suite passed in Docker after the split: 571 passed,
zero failed, 18 ignored, 33.29 seconds. A deliberately stalled-census integration
repro remains pending; this suite confirms existing daemon lifecycle behavior.

Crash recovery work now has a writer-relocation primitive: consistent SQLite
backup to a new machine-stable path, validated same UUID, replacement writer
lock acquired before releasing the original, then all later transactions use
the stable database. Thirteen registry tests passed in Docker, including
exclusive stable writer ownership, post-handoff commits absent from the old
file, and overwrite refusal. Durable authority publication and service/catalog
startup selection are not wired yet; dirty-snapshot recovery remains incomplete.

Writer relocation now retains the original database writer lock for the
relocated registry lifetime, rather than releasing it before the caller can
publish authority. This prevents an older daemon from writing the historical
copy during publication. The Docker regression passed with both original and
stable writer acquisitions refused while relocated ownership remains live.
Relocation retains at most eight historical writer fences. Durable authority
publication and restart resolution remain the next required integration work.

Relocated registry authority can now be atomically published as a private,
synced, bounded typed record. Resolution verifies the exact target UUID,
absolute target, schema, regular file, and absence of redirect chains. The
relocation regression proves resolution survives loss of the original database
and rejects malformed proof. All thirteen registry tests passed in Docker.
Normal opens and the service/catalog still require explicit wiring to this
authority; current snapshot recovery is not yet replaced.

Normal registry writer, read-only, and retention-snapshot opens now resolve
published stable authority. Writer restart retains the original fence when
that file exists; fresh creation refuses an existing authority. Resolution
validates both target and any surviving original UUID using raw read-only
connections, avoiding recursive redirect races. Thirteen registry tests passed
in Docker, covering redirected reads, writer restart after original-file loss,
and refusal to mint a replacement. Service relocation/catalog recovery wiring
still remains incomplete.

Peer recovery now recognizes published machine-stable authority before the
legacy clean-export marker. It validates the owner UUID and saved policy,
refreshes policy from intact verified original state, and resolves the live
SQLite database even with a dirty historical export and vanished original
state. Both recovery unit tests passed in Docker, including preserved explicit
opt-out. The daemon still does not automatically publish this authority; that
startup promotion is required before crash recovery acceptance can be claimed.

Daemon startup now promotes the registry writer into machine-stable storage
and publishes machine and original-path authority before accepting workloads.
Restart resumes existing published authority. The production Docker acceptance
driver passed all five cases, including active peer protection, vanished peer
state reclamation, explicit opt-out, and prepare/failure image cleanup. Its peer
assertion now verifies the authoritative database instead of a clean export.
The same live test caught a cache supervisor admission lock held across sleeps;
the lock now spans individual discovery/maintenance passes only. Interrupted
promotion before first authority publication still requires recovery handling,
and daemon crash/WAL integration evidence remains pending.

Authority restart coverage now simulates publication of machine proof before
the workspace redirect and verifies promotion preserves newer stable commits
through two restarts. Offline peer retention additionally fences both surviving
historical and authoritative writer locks, protecting older daemons that do
not follow redirects. Ten peer-related service tests and all thirteen registry
tests passed in Docker, including legacy-writer rejection during retention.
Interrupted promotion before any authority record still remains incomplete.

Startup handoff now durably records typed preparing/published phases with
exact owner, original path, and stable path, using bounded private atomic
publication. A retry accepts only the same preparing proof; published or
changed identity refuses. Missing machine publication with an existing
workspace redirect also refuses rather than replacing authority. Both focused
authority tests passed in Docker. Refresh of a proven unpublished interrupted
copy is still pending; the new proof is the prerequisite for that recovery.

Promotion retry now refreshes a stable copy only after validating matching
unpublished preparing proof and absence of published redirects. It excludes
both writers, checkpoints and closes the unused destination, atomically
replaces it from a consistent current-source backup, and transfers the existing
destination lock to the replacement connection. Three authority tests passed
in Docker, including preservation of a source commit newer than the interrupted
copy. Crash injection across each filesystem publication boundary and complete
state-loss scenarios remain required for final acceptance.

The production peer integration now kills the real native peer daemon
abruptly after completed workloads instead of authenticated graceful shutdown.
It then makes the original state path unavailable and verifies machine GC
reclaims the container, warm volume, and image while preserving explicit pins
and the saved opt-out. The complete five-case production Docker acceptance
driver passed (peer crash case 26.56 seconds). Publication-boundary crash
injection and interrupted creation still require separate coverage.

Setup and manifest ensure now checkpoint their exact planned container
identity and resource use through the registry actor before any container
engine operation. Factory warm retention preserves existing explicit pins;
the intent event distinguishes planned accounting from successful ensure.
Manifest guests keep their distinct namespace. All five production Docker
acceptance tests passed after the checkpoint, including crash-state recovery.
A focused rejected-container-checkpoint test and crash between physical create
and final receipt remain pending before interrupted-creation acceptance.

### Container checkpoint rejection regression

The shared setup/manifest pipeline now has a focused regression covering both image and planned-container checkpoint failures. For both ownership namespaces, a rejected checkpoint leaves the fake engine with zero container operations. The container case also checks the stored resource namespace, stack, generation and canonical workspace. Verified in the isolated Docker unit runner: one test passed (four checkpoint/owner combinations), 592 filtered out. This establishes the pre-creation ordering; interruption after Docker creation remains a separate acceptance check.

### Bounded incomplete-read diagnostics

Incomplete engine-read accounting now retains the exact failure count while bounding stored detail to 64 entries of 2048 UTF-8-safe bytes each. Refusal text explicitly reports omitted details; truncation never changes the all-or-nothing census refusal. Automatic reports also print zero-candidate outcomes and any failure details instead of returning before those diagnostics. The isolated Docker runner passed all 52 managed-retention tests, including 1024 long read failures with exact total accounting. Held-object totals and interrupted-creation acceptance remain outstanding.

### Protection detail totals

Managed retention now carries `held_total` through the daemon wire response and CLI JSON, with `held_details_omitted` in JSON and an omitted-detail notice in automatic logs. The count represents protection decisions across stages (not unique physical objects, which can be re-observed). Peer opt-outs and unavailable-peer decisions contribute to the total; staged merges retain totals even after the 64-detail limit. Added a 300-decision merge regression; all 53 managed-retention tests passed in isolated Docker. Wire decoding of older responses falls back to the number of available details.

### Full verification after diagnostic response changes

The rebuilt service unit binary passed 577 tests, with 18 explicitly ignored, in isolated Docker (33.17 seconds). The repository lint driver passed Ruff formatting/checks, Pyright, Rust formatting, KBI, workflow/platform restrictions, source length, and embedded-path checks. The production Docker acceptance driver then rebuilt the native daemon and passed all five cases: manifest lifecycle cleanup (4.09 seconds), abrupt peer-daemon death plus unavailable original state (20.71 seconds), and three prepare/task image-accounting cases (3.47 seconds). These results verify current uncommitted work, not an exact merged candidate SHA; final acceptance, review, push and merge remain required.

### Authority publication boundary restart matrix

Added a seven-case restart matrix around completed publication boundaries: before preparing proof, after preparing proof, after machine alias snapshot, after stable writer relocation, after machine redirect publication, after original-path redirect publication, and after policy persistence. Each case reopens through the original path and runs production promotion, then verifies unchanged UUID, both redirects resolving to stable authority, preservation of the initial ownership receipt and all post-publication stable commits, both writer fences, and a non-replaceable published handoff. All four authority-related tests passed in isolated Docker (1.47 seconds). This simulates interruption between completed operations; process death during an individual write/rename/fsync remains distinct from this coverage.

### Whole-workspace loss startup recovery

Startup now attempts to restore a missing local authority locator from the exact canonical workspace catalog entry before probing or minting a registry. Recovery requires a unique entry, published machine authority with matching UUID, machine owner exclusion, and an offline stable SQLite writer fence. It atomically restores the validated locator and, only when the local policy file is absent, the last verified policy (including explicit opt-out). Existing databases/locators remain subject to normal validation. A Docker regression renames the whole workspace, recreates its empty path, verifies active-writer refusal, then restores the original UUID and explicit false policy after writer exit; repeated restoration is idempotent. One focused test passed. Native-daemon restart integration and concurrent first-run fencing still require audit.

### Native daemon restart after whole-state loss

Extended the real peer-daemon Docker acceptance case to start a new native daemon at the empty original path after abrupt death and whole-directory removal. The authenticated client becomes ready; the registry retains UUID `00000000-0000-4000-8000-000000000548`, all previously recorded resources, and the saved explicit opt-out, with no replacement local SQLite database. The recovered daemon shuts down successfully before subsequent machine GC checks. All five production Docker cases passed: manifest 4.13 seconds, peer restart/crash recovery 21.38 seconds, preparation cases 3.58 seconds. Concurrent first-run/restore admission remains a separate audit item.

### Serialized startup identity handoff

Native service startup now takes a machine-stable exclusive startup lock before restoring a locator or probing/creating a registry, and retains it through owner registration and authority publication. This closes the new-daemon race between missing-state recovery and first-run identity creation. The lock is released once durable registry writer and owner exclusion take over; conflicting concurrent startup currently returns an explicit busy/unavailable error. Isolated unit fixtures use their own state directory so unrelated fixtures do not share host locks. A focused lock-lifetime regression verifies competing acquisition refusal and acquisition after release. All 15 peer-related tests passed in Docker. Native multi-process startup/retry behavior remains to be exercised.

### Native multi-process startup exclusion

Added a real-process acceptance case that holds the machine startup lock, launches two native daemons, and requires both to exit unsuccessfully without publishing a locator or changing the initialized UUID. After releasing the lock, the native daemon becomes ready, retains the same UUID, rejects a duplicate daemon through writer exclusion, and shuts down successfully. The production Docker driver passed all six cases: manifest cleanup 4.07 seconds, two peer/startup cases 21.61 seconds, three image-preparation cases 3.60 seconds. Current contention behavior is explicit refusal followed by caller retry; this evidence does not claim unattended startup retries are already implemented.

### Workspace Clippy gate

Ran the exact CI Clippy gate (`soldr cargo clippy --workspace --exclude bosn-python --all-targets --locked -- -D warnings`). Initial failures identified two collapsible conditions, an iterator expression, and oversized retention functions. Extracted typed fresh-removal observations, refusal outcome construction, and bounded held-detail construction; simplified the conditions and iterator. The three deliberately sequential live acceptance scenarios have documented test-only lint expectations for scenario length (and peer scenario complexity); production limits remain enforced. The complete workspace Clippy gate passed. Rebuilt service unit tests and passed all 56 managed-retention tests in isolated Docker (1.56 seconds).

### macOS maintenance restart policy

Generated launch agents now include a `KeepAlive` dictionary with `SuccessfulExit = false`, preserving clean shutdown while requesting relaunch after unsuccessful exit, matching the Linux unit's existing `Restart=on-failure`. Apple's primary [launchd manual source](https://github.com/apple-oss-distributions/launchd/blob/main/man/launchd.plist.5) documents this condition and startup implication. The generated-unit regression asserts the condition; all 12 autostart tests passed in isolated Docker. This verifies configuration generation and registration behavior through the fake runner, not live launchd crash recovery; changed-unit upgrade handling still requires work.

### Autostart status verifies the manager

Autostart status no longer equates a unit file with successful registration. It queries `systemctl --user is-enabled --quiet` or `launchctl list`; `registered` is true only after a successful manager query, and false also covers an unavailable manager. The public status API is unchanged and uses the bounded system runner. Added both-platform regressions where the unit exists but the manager refuses, plus successful confirmation. All 13 autostart tests passed in isolated Docker. Changed-unit macOS upgrade handling remains unresolved because reloading must preserve active-work safety.

### Recovery refusal evidence

Added negative recovery regressions for a previously cataloged workspace whose published machine authority is absent, and two catalog identities claiming the same lost workspace path. Both cases refuse with specific reasons and leave the recreated workspace without a database, locator, or generated policy. Together with active-writer refusal and successful exact-identity recovery, all three restoration tests passed in isolated Docker (0.22 seconds). These tests verify refusal before any replacement identity can be created by the startup path.

### Registry verification and setup-command scope

Rebuilt the current registry unit binary and passed all 13 registry tests in isolated Docker (0.33 seconds), verifying redirected opens, consistent ownership backups, writer fencing and refusal behavior against current registry source. Inspection of setup creation confirms `SetupEnsureCommand::Create` accepts an optional application command and may retain the image's own default command. Consequently generic setup must not be retired merely because its process table resembles a sleep loop. The implemented idle reaper remains limited to exact registered manifest-container ownership plus the full fixed Bosn login-shell command, jobs/leases/pins and fresh process checks. Supporting other persistent keepalive origins requires explicit provenance or their existing runtime lifecycle proof.

### Bounded idle retirement

The idle reaper now stops at most the remaining per-pass object allowance, including a zero-allowance fast return before registry/engine access. Eligible keepalives are ordered by oldest verified shared use, with deterministic name ties, while pins and activity protection remain excluded. Successful stops consume the allowance; skipped/protected engine observations do not. This caps stop operations as well as the existing removal cap; objects beyond the stop cap remain running and protected for subsequent passes. Added ordering/pin regression and passed all 59 managed-retention tests in isolated Docker (1.55 seconds). A live multi-container capped-stop acceptance case remains needed to directly prove the nonzero cap at the engine boundary.

### Stop allowance at the engine boundary

Added a focused two-container engine-boundary regression backed by real registry rows and a synthetic Docker CLI. Zero allowance emits no stop request; one-object allowance emits exactly one stop for the older verified manifest keepalive and does not stop the newer one. Fixture commands use the shared production login-shell generator, now re-exported by bosn-setup to avoid copying launcher syntax. The isolated Docker unit runner passed the regression (0.08 seconds). This proves CLI request budgeting with production ownership checks; it is not a claim of two real Docker containers being stopped by this particular test.

### Shared container ownership transaction helper

The pre-creation checkpoint and completed setup ensure now use one typed container ownership/resource-use helper. This removes duplicate factory policy, keeping machine scope, Warm default, explicit pin preservation and use timestamps identical at both boundaries. Inspection confirms the registry upsert preserves the existing creation timestamp while updating last use; no creation-clock reset was introduced. All 12 setup rollover/pipeline tests passed in isolated Docker (0.30 seconds), including rejected checkpoints and transactional rollover conflict.

### Planned volume ownership checkpoint

Manifest planning now records each exact volume resource and use in the same transaction as its durable creation intent, before the engine primitive can create any volume. The checkpoint retains declared retention and scope, preserves existing pins, and uses actor-owned timestamps. Intents remain protected: this adds ordinary ownership proof without prematurely authorizing deletion of an unfinished creation. The full service suite passed 585 tests with 18 ignored (33.17 seconds). Extended the intent lifecycle regression to verify resource identity, generation, scope, workspace, retention and use timestamp before engine work, then intent consumption on success; that focused test passed (0.07 seconds). Abandoned intent retirement/reconciliation is a newly concrete remaining lifecycle gap: a later ensure can recover it, but an abandoned workspace must also get a bounded automatic recovery path.

### Pin preservation through completed volume handoff

The final manifest volume receipt now uses the same pin-preserving upsert as planning. Previously a warm factory receipt could undo an existing registry pin even though the new planning checkpoint retained it. Extended the volume-intent lifecycle regression to reuse the pinned resource with a Warm factory value, assert the pin survives both planning and completion, and verify intent consumption. All 10 manifest ensure tests passed in isolated Docker (0.44 seconds). Explicit operator retention edits remain separate from factory refreshes.

### Abandoned planned-volume eligibility

Managed ownership now distinguishes unverified creation intents from intents backed by an exact ordinary ownership row. An intent remains protective unless the row matches manifest volume identity, name, stack, generation, scope, workspace and finite use time, and every stored intended label matches the actual engine observation. Pending proof is carried through peer merging and counted toward inventory limits. Once this proof is present, ordinary lease/session, physical liveness, pin, age and admission gates still apply; the intent alone no longer strands the volume forever. Legacy intents without matching rows remain protected. The token-bound manifest release path is unchanged. Added regressions for missing rows, changed workspace, mismatched actual labels and peer intent protection; all 61 managed-retention tests passed in isolated Docker (1.54 seconds). Real interrupted-volume creation and registry intent/history compaction remain required acceptance evidence.

### Live regression verification after intent eligibility changes

The production Docker driver rebuilt the native daemon and passed all six acceptance cases after planned volume ownership, pin-preserving completion and verified intent eligibility changes: manifest cleanup 4.67 seconds, two peer/startup cases 21.81 seconds, three image preparation cases 3.61 seconds. Full repository lint also passed, including all source-length and embedded-path gates. This verifies completed-volume behavior and existing crash/state-loss scenarios, but does not substitute for direct interruption after engine volume creation. The next focused acceptance case should use a real production ensure whose image entrypoint makes container startup fail after volumes are created, then require container/intent-backed Warm volume/image disappearance while explicit pins survive.

### Real failure after volume/container creation

Added a native-daemon acceptance scenario whose uniquely built image has a missing entrypoint. Production manifest ensure creates both declared volumes and the container, then fails at container startup before its final receipt. The test confirms Failed status, a physically present stopped container, two unconsumed volume intents and both physical volumes. One zero-TTL managed pass then proves exact disappearance of the stopped container, intent-backed Warm volume and prepared image while retaining the explicit pinned volume. Teardown reads exact planned ownership rows even if an assertion fails before final accounting. The complete production Docker driver passed all seven cases: two manifest scenarios 4.70 seconds, two peer/startup scenarios 21.60 seconds, three image scenarios 3.59 seconds. This verifies failure before final receipt; abrupt process interruption during image build remains separate. Durable resource/intents/history pruning is still required to bound registry storage after physical reclamation.

### Typed physical-deletion receipts

The retention worker now retains typed fresh ownership labels, physical identity/name, observation time and originating registry path only after successful physical removal. Staged and peer merges retain these receipts; automatic per-object reporting bounds receipt details with the existing 64-entry/2048-byte limits. Complete typed proof is required for a registry-cleanup receipt; older physical candidates accepted by the existing classifier continue to be reclaimed when eligible even if they cannot produce that stronger receipt. Added receipt success/failure assertions and a compatibility regression for an invalid legacy scope. All 62 managed-retention tests passed in isolated Docker (1.53 seconds). Receipt consumption in an admission-protected registry transaction and absence reconciliation are still pending; these receipts do not yet prune storage.

### Registry accounting after confirmed physical deletion

Added `Immediate::delete_removed_ownership` with transactional registry UUID, identity, creation incarnation, last-use, pin, lease, session, and shared-use checks. Exact volume intent metadata is removed with the eligible volume row. A Docker-isolated regression passes for pinned resources, newer use, replaced creation time, foreign registry identity, and unchanged removable ownership. The job actor now transfers the machine admission guard with deletion receipts into the registry actor worker, preventing caller cancellation from releasing admission during the commit. Native service compilation and all 62 managed-retention unit tests pass. Offline peer accounting cleanup and full physical/registry regression coverage remain outstanding. An initial `cargo check` encountered a Soldr broker handoff failure; the subsequent Soldr test compilation succeeded.

### Offline peer accounting and live row reclamation

Offline apply now opens the validated registry under its sole writer fence while retaining the stable machine owner lock, and commits exact deletion receipts before dropping either fence. Preview retains the read-only path. All 62 managed-retention unit tests passed. The failed-manifest-start Docker regression now verifies both physical cleanup and accounting: only the pinned volume ownership row and its creation intent remain. Fixture teardown was corrected to support already-reclaimed container/image rows. The complete seven-case Docker acceptance runner passed, including foreign-work protection, lost-state recovery, prepare-only images, failed tasks, and failed container start. Offline peer row deletion still needs an explicit regression assertion; bounded history/storage and other acceptance-audit items remain outstanding.

### Shared-object offline accounting regression

The explicit offline ownership assertion initially failed despite successful physical reclamation. The fixture intentionally records the same physical objects in two registries, so deletion receipts naming only the collecting registry left the other registry rows behind. Offline sweep now checks all prior receipts against exact kind/name (or immutable image ID), stack, generation, scope, workspace, and observed creation time in the writer-fenced peer registry, then applies the transactional pin/use/lease/session guards. Accounting also runs when the physical removal allowance is exhausted. A receipt captures the matching current registry creation incarnation independently of Docker creation labels. The offline fixture now proves only the pinned ownership row survives and no completed volume intents remain. All seven Docker acceptance cases pass again. Two Docker-isolated registry transaction tests pass, including lease and execution-session refusal followed by successful deletion after protection removal. Full CI lint/review, crash-durable receipt replay/absence reconciliation, repeated history/storage bounds, and the other current-audit requirements remain outstanding.

### Post-accounting implementation gates

Re-read the current open GitHub issue and its ten acceptance criteria; the objective still requires a validated pushed and merged PR. Latest `soldr cargo clippy --workspace --exclude bosn-python --all-targets --locked -- -D warnings` passed after correcting a needless test borrow. Full `ci/lint.py` passed (including formatting, Python analysis, workflow constraints, source length and embedded path rules), and `git diff --check` passed. Updated the current audit boundary to distinguish proven confirmed-deletion cleanup from missing crash/absence, active shared-registry, and history/storage evidence. No release, commit, PR, or merge is claimed.

### Manifest app task preparation checkpoint RED → GREEN

Creation-path inspection found `DockerManifestAppTaskExecutor` prepared an image and attempted adoption before recording ownership. Added a real production manifest-app-task failure case using a unique Dockerfile and a deliberately absent managed container. After correcting the fixture to the actual `[task.fail]` manifest syntax, the unchanged production implementation failed at the ownership assertion (`resources = []`, expected one image). Added a required manifest session-recorder image checkpoint, implemented by the registry actor, immediately after successful image preparation and before adoption/deadline checks. The identical regression now passes and verifies the immutable image is recorded Warm and subsequently disappears after GC. All eight live Docker lifecycle cases pass. This addresses a post-preparation task/adoption failure gap; crashes during build or between image export and the checkpoint still require durable preparation-intent recovery. Latest full lint/Clippy success predates this new callback and must be refreshed before delivery.

### Setup adoption preparation ownership and admission

Audited the separate setup-adopt RPC: it prepares an image before fallible adoption and previously had neither the prepared-image checkpoint nor the machine workload gate used by jobs. Added `SetupAdoptExecutor::execute_recorded` and its production implementation; the installed daemon passes its registry actor recorder. The adoption RPC now holds machine workload admission across preparation, ownership checkpoint, adoption, and final adoption record. A production failed-adoption fixture proves the unique prepared image is Warm-owned and exactly reclaimed despite adoption failure. All nine live Docker acceptance cases pass. The service unit suite immediately before this adoption change passed 587 tests with 18 ignored in Docker (605 total); full unit/gate refresh for the adoption change remains pending. Interrupted build/export recovery and other current-audit criteria remain outstanding.

### Adoption code gate refresh

Full repository lint passed after the adoption checkpoint/admission change. The first simultaneous Clippy attempt failed in Soldr broker handoff before project diagnostics; a sequential one-job retry reached code and reported the adoption executor at 102/100 function lines. Extracted its output-budget validation into a separate responsibility, then the full workspace/all-target Clippy gate passed with warnings denied. The latest helper extraction is behavior-preserving; the nine live lifecycle cases passed immediately before it. Interrupted image creation remains an acceptance gap: `prepare_setup_image` builds/pulls first and inspects afterward, and all daemon image checkpoints presently require that inspected immutable ID. A durable pre-build intent plus verified post-crash reconciliation is the next substantive creation-boundary work.

### Validated pre-engine image preparation identity

Added exported `ImagePreparationIntent` and `image_preparation_intent(&SetupPlan)` in bosn-setup. The API uses the same plan/hash/private-asset validation and reference derivation as actual preparation, returning the exact build tag or immutable pull reference plus source kind, content digest, and workspace. It explicitly represents intent rather than existing engine ownership proof. Preparation tests assert the pre-engine identity agrees with the completed preparation receipt and tampered assets cannot yield an intent. All six preparation tests pass in Docker. Durable registry persistence, callbacks at every prepare boundary, and verified interrupted-build reconciliation are still required; no interrupted-build cleanup completion is claimed.

### Durable bounded registry preparation-intent storage

Added typed `ImageCreationIntent` plus source/owner enums in bosn-registry, using v5 metadata storage without adding a schema version or silently changing existing database tables. Intents validate canonical content hashes and build tags/immutable pull references, finite creation time, nonempty workspace/stack, a 16 KiB record limit, and a 1024-record admission ceiling. Reads are bounded and verify key identity plus typed content. Completion deletion matches the complete serialized attempt so an old completion cannot consume a newer attempt. The Docker-isolated persistence regression passes: intent survives writer close/reopen, stale completion retains the newer attempt, exact completion removes it, and an unrelated mutable reference is rejected. Daemon actor callbacks, build ownership labels, pending-intent diagnostics/recovery, and full interrupted-build runtime proof remain unwired; the public storage API alone is not an acceptance completion.

### Pre-engine durable actor checkpoint in shared ensure

Added a typed registry-actor command for image preparation intent publication. Its dedicated mutation helper commits SQLite and publishes ownership durability before acknowledging the executor, keeping registry_actor below 1000 lines. `ActorSetupImageRecorder` now records validated source/reference/content/workspace/stack/owner and timestamp; the shared production setup/manifest ensure pipeline awaits that checkpoint before any pull/build/inspect. Native service compilation passes. A Docker-isolated fault regression proves a rejected preparation checkpoint causes zero image commands and zero container commands. Completion consumption, all other prepare origins, verified engine ownership labels, and interrupted-build recovery remain pending. In particular successful prepares currently leave their bounded intent until completion consumption is implemented; this temporary state is not claimed as steady-state retention acceptance.

### Successful preparation intent consumption

The shared setup/manifest ensure pipeline now retains the exact preparation attempt through image preparation. After the inspected image ownership checkpoint acknowledges durable storage, the actor consumes that exact intent; stale completion cannot consume a newer attempt. A failed prepare or failed image checkpoint retains intent for future recovery. Added bounded read-only intent enumeration using the same typed decoder as writer reads. The two live manifest ensure fixtures now assert no pending image intents remain after preparation, including the case where later container startup fails. All nine live Docker cases pass. This removes the previously documented successful-ensure intent accumulation; remaining prepare origins, physical ownership labels and verified interrupted-build recovery remain outstanding.

### Preparation checkpoints across prepare/task/adoption origins

Extended pre-engine intent publication and exact completion consumption to setup declared-task execution and setup adoption. Added an actor-backed recorded preparation entrypoint for prepare-only jobs; the default trait adapter retains artifact ownership for fake executors, while production checkpoints the validated planned reference before Docker and records/consumes after inspection. Job completion no longer redundantly records production prepare ownership. The prepare lifecycle fixture now checks the pending-intent set is empty for every completed preparation before GC. All nine live Docker cases pass, including prepare-only success, task exit 7, failed setup adoption, and the two missing-app task cases. Setup/manifest app-task preparers still need their own pre-engine intent callbacks (their image completion ownership is already durable). Interrupted physical-image recovery, full code gates/unit refresh, and remaining audit criteria are still incomplete.

### App-task preparation intent boundaries

Setup and manifest app-task session recorders now expose preparation checkpoint callbacks, with production actor implementations that await durable publication/consumption. Each app-task executor records its validated intent immediately before the image preparer and consumes the same attempt immediately after inspected-image ownership commits, before adoption or task failure. The six production `prepare_setup_image` call sites now all checkpoint intent: setup prepare, setup declared task, setup app task, shared setup/manifest ensure, setup adoption, and manifest app task. All nine live Docker cases pass, including both missing-app failures and empty pending-intent assertions. The source audit establishes checkpoint wiring, not interrupted-build recovery: trusted physical-image metadata/reconciliation and kill-at-export integration evidence are still missing. Full unit/lint/Clippy refresh for these latest trait callbacks remains pending.

### Checkpoint latency and unit-suite refresh

Full workspace/all-target Clippy found setup adoption at 101/100 lines after new intent callbacks. Extracted adoption result validation/construction as a separate function; the full Clippy gate then passed. Review also found pre-engine checkpoint awaits were using a previously captured Docker duration. All affected preparation/inspection calls now refresh the original absolute deadline immediately before engine execution. A new delayed-checkpoint regression proves the elapsed checkpoint exhausts the deadline and causes zero image/container commands. The updated full service unit suite passed in Docker: 589 passed, 18 ignored, 607 total, 33.07 seconds. Clippy success predates only the added test; production changes were included. Interrupted physical-image ownership/recovery and remaining acceptance criteria still require work.

### Stable preparation ownership proof identity

Added a validated `ImageCreationIntent::ownership_proof()` digest of the exact reference/workspace/stack/owner identity already used by durable intent keys. Retries of the same preparation retain the proof, while another workspace changes it; the Docker-isolated intent persistence regression passes with these assertions. This avoids using a tag name alone as future recovery authorization. The digest is public identity evidence, not a secret capability: recovery must additionally verify immutable engine identity, actual image metadata, registry ownership/protection and fresh liveness under admission. Exporting this proof as a production build label and verified recovery remain unwired. No physical interrupted-image reclamation success is claimed.

### Production image exports carry preparation proof

Added a finite owned-build command and `ImagePreparationEngine` wrapper. For locally built images it injects `com.zackees.bosn.image-preparation=<validated intent proof>` into the actual Docker build; pull and inspect commands are unchanged. All six production preparation paths use the exact committed intent proof. Direct unowned executor tests retain their unowned command path. The wrapper initially exposed a Rust Send/lifetime inference failure; an explicitly bounded boxed Send preparation future resolved it. Seven Docker-isolated preparation tests pass, including exact proof argument emission. All nine live lifecycle cases pass; prepare-only, failed-task, failed-adoption, and both missing-app cases inspect exported Docker image labels and confirm the canonical 64-character proof exists before reclamation. Recovery must still verify that label against durable intent plus immutable image identity, protection and fresh liveness; no interrupted-export recovery has been implemented yet. Full unit/lint/Clippy gates need refreshing for the new engine wrapper.

### Verified export reconciliation transaction

Added privately constructed `VerifiedImageExport` evidence from a build intent, canonical immutable image ID and matching preparation proof label. Partial canonical labels, foreign registry ownership, mismatched canonical stack/workspace/scope/generation, and pinned/unknown engine retention labels refuse recovery. The transaction verifies registry UUID again and compares the exact current serialized intent, records proven image ownership/use, and consumes intent atomically. Existing ownership metadata, pins, and the maximum resource/use timestamps are preserved; an older intent cannot age a shared image backwards. Two Docker-isolated intent/reconciliation tests pass, including missing proof refusal and retained pin/recent use. This supplies the typed verification/commit boundary only: bounded fresh Docker inspection in active/offline daemon maintenance and real kill-at-export recovery evidence are still unwired. Missing exported images must not automatically consume intent while a surviving external builder might still export them.

### Offline maintenance recovers inspected build exports

Wired bounded fresh inspection of each pending build reference into offline peer apply while machine admission, stable owner lock, and database writer fence remain held. Exactly one typed image inspection and matching durable proof are required before the verified registry transaction runs; recovered rows then enter the ordinary idle/container/volume/image retention pass. Missing/unreadable/nonunique exports and foreign/protected/mismatched proof retain intent and add bounded, counted held diagnostics. Immutable pull intent recovery remains explicitly unimplemented and retains proof. Preview uses read-only peer access and performs no reconciliation writes. A Docker-isolated engine-inspection regression passes for missing proof retention followed by exact proof recovery/intent consumption. All nine existing live lifecycle cases pass with recovery enabled. Current-registry actor recovery and an actual native-daemon kill between export and completion still need implementation/evidence; the existing live cases alone do not prove that crash scenario.

Current-registry image recovery now runs through the registry actor while holding
machine admission. The same absolute pass deadline covers recovery, physical
reclamation and peer sweeping. Admission transfers back to the physical worker
and then into ownership pruning, so cancellation cannot release it while a
registry transaction is still running. Registry recovery/pruning workers were
split into `registry_retention.rs` to keep the actor below the file length gate.
The service unit binary compiled, and the live driver passed all nine existing
Docker cases (manifest 5.11s, peer/startup 22.09s, preparation 4.44s). These cases
verify existing lifecycle behavior; native daemon death between image export and
ownership checkpoint still needs an explicit live fault-injection test.

A new live production fault test pauses only the child daemon's Docker CLI after
successful image-ID inspection and before that ID reaches Bosn. It verifies one
durable preparation intent and zero ownership rows, kills the native daemon,
restarts on the same authority and requests GC. GC recovers the exported image,
removes exactly that image and leaves neither intent nor ownership row. The shim
has a 30-second bound and releases on fixture teardown. All ten live Docker
cases passed (manifest 5.19s, peer/startup 22.19s, preparation/crash 4.95s).
This proves interrupted build export recovery for the current registry; immutable
pull recovery, missing-export intent retirement and the broader acceptance audit
remain open.

Pending immutable pull recovery now reads Docker's typed `RepoDigests` and
requires an exact match to the planned immutable repository reference before
creating ownership. Both build and pull recovery share immutable image-ID,
canonical ownership and retention-label protection checks. Mutable tags, wrong
repositories, malformed IDs, pinned labels and partial foreign metadata fail
closed. The newly compiled registry binary passed all three image-intent tests;
the service recovery test now exercises rejection followed by successful pull
reconciliation. Equivalent repository spellings and real interrupted pull
coverage still require validation; missing-export intents remain retained.

The real pending-pull case seeds a durable immutable pull intent and an existing
pinned image row, then runs the native daemon's recovery against actual Docker
`RepoDigests`. It proves the intent is consumed, the pin and newer last-use clock
survive and the image remains. Docker's documented omitted host/namespace forms
are normalized to `docker.io/library` while explicit registries stay distinct
(https://docs.docker.com/reference/cli/docker/image/tag/).
An initial broader run caught a transient maintenance admission refusal and one
adoption intent still present. A later run exposed generic adoption error replies
as insufficient diagnostics. Adoption now carries its actual failure string
through the existing error field; Rust and Python preserve it. The pending-pull
fixture retries only the explicit busy admission refusal within its readiness
deadline. The latest driver passed all eleven cases (manifest 5.00s, peer/startup
22.24s, preparation/recovery 5.10s), including the expected actual adoption
ownership refusal. The original intermittent adoption failure has not yet been
explained, so its diagnostic coverage must remain in place during final checks.

Workspace `cargo check --workspace --all-targets --locked -j 1` passed, including
Python's handling of the new remote adoption error. A cross-registry audit found
that opted-out preparation intents without ownership rows were absent from the
shared protection census. Registered ownership now loads bounded typed image
intents, counts them in the inventory ceiling and carries their protection state
when merging peers. An opted-out build protects matching actual preparation
proof labels before the ownership checkpoint. An opted-out pull has no intrinsic
build proof before immutable identity recovery, so it currently conservatively
protects image reclamation rather than guessing an ID. All twelve registered
ownership tests passed, including the new peer intent test. Narrowing that pull
ambiguity and reporting its distinct hold reason remain acceptance work.

Opted-out pending pull protection is now scoped using a fresh bounded inspection
of each candidate's immutable `RepoDigests`. A matching planned repository digest
protects that image; a complete inspection proving a different repository leaves
unrelated candidates eligible. Failed reads, nonunique results, wrong IDs and
missing digest fields retain protection. Missing digest fields are typed
separately from present empty arrays. No ownership is granted by this comparison.
The updated service binary compiled and all thirteen registered ownership/pending
image tests passed. Distinct intent hold diagnostics and live cross-registry pull
protection evidence remain to complete this part of the acceptance audit.

The refined pending pull protection passed the complete eleven-case live driver
(manifest 5.02s, peer/startup 22.12s, preparation/recovery 5.53s). Workspace Clippy
with all targets and warnings denied passed in 29.01s (Python excluded there;
the preceding workspace check included Python). The acceptance table now records
actual native interrupted-build evidence and names the unresolved deletion crash
boundary explicitly. No PR, candidate commit or merge exists yet.

Deletion intent implementation has started: the physical worker now writes a
bounded proof document in a private directory beside the resolved authoritative
registry before Docker deletion. It syncs the file and directory and refuses
physical deletion when proof persistence fails or the 1024-intent ceiling is
reached. Current actor pruning acknowledges the exact unchanged document after
registry commit and backup publication. This is incomplete: offline peer
acknowledgement, bounded typed intent enumeration, fresh absence verification,
restart replay and crash fault tests must be added before relying on this path.
The initial build found one borrowed-path move; that was fixed and the same
service compilation is running again. No cleanup completion claim is made.

Deletion replay is now connected to the current/offline image recovery entry
point. Intent enumeration is bounded to 1024 files and 16KiB per file, refuses
symlinks/non-files, eagerly parses ownership labels and verifies the filename's
identity digest. The digest now includes resource kind and scope. Replay uses
the actual SQLite database path, refuses foreign registry intents and requires a
successful bounded full Docker identity inventory proving the exact object is
absent before the existing transactional accounting guards run. The first build
caught LabelError's missing Display implementation; its diagnostic uses Debug
and compilation is rerunning. Remaining work includes acknowledgement when the
old incarnation is already gone, offline peer acknowledgement, total held
counts, malformed/missing identity validation and focused crash/replacement/pin
fault tests. This replay implementation is not yet validated as complete.

Deletion replay now distinguishes protected ownership from an old incarnation
that is already gone, so a crash after the accounting commit can acknowledge
its surviving intent without touching a replacement. Held counts remain complete
while detail storage stays bounded. Offline peer pruning acknowledges its own
unchanged intents after commit/publication. Physical identities are validated at
write/read boundaries. A Docker-isolated reopen regression passed seven cases:
absent old object cleans row/intent; pinned row retains both; already deleted row
clears intent; replacement incarnation survives while old intent clears; present
object holds; malformed inventory refuses; failed inventory refuses. The live
lifecycle driver is now running against the new ledger/replay path. Native
kill-after-physical-deletion and stable-authority state-loss fault coverage are
still required.

The native delete-crash test reached actual successful Docker image deletion
while registry ownership and one stable deletion intent remained, killed the
daemon before Docker returned, moved the entire original state directory away,
then restarted. The same UUID survived and replay removed the ownership row and
intent without claiming another physical deletion. This case passed. The broader
run exposed the previously intermittent adoption failure as a client `Deadline`:
setup adoption used the generic three-second RPC wait despite allowing a much
longer image preparation deadline. Its client now uses operation deadline plus
RPC response allowance. The full twelve-case driver is rerunning against that
fix. Server admission/preparation must also share one absolute operation deadline
and deterministic slower-than-three-second RPC coverage remains to add.

The client adoption deadline fix passed all twelve live Docker cases (manifest
5.13s, peer/startup 22.38s, preparation/recovery 6.82s), including native physical
deletion followed by original state-path loss. Adoption dispatch now captures
one absolute deadline before machine admission and forwards only the remaining
budget to execution, refusing when admission exhausts it. A deterministic
Docker-isolated RPC regression delays the adoption executor beyond the ordinary
three-second wait and verifies the actual remote ownership refusal reaches the
client; it passed in 3.46s. Full service unit tests and all-target workspace
Clippy are running against the expanded ledger/replay and deadline implementation.

The expanded full service suite reported 590 passed, five failed, eighteen
ignored. All five failures involved synthetic physical IDs that were not Docker
container digests; the fixture now emits one canonical 64-hex identity through
its environment. The investigation also found a compatibility bug in deletion
proof validation: complete canonical labels can use RFC3339 creation times,
whereas registry incarnation times are numeric. Intents now accept both valid
finite timestamp forms; the registry incarnation query treats a nonnumeric engine
creation label as unable to match a numeric registry row. Clippy additionally
found retention_stage at 102 lines. Intent persistence moved into remove_owned,
which is the actual mutation boundary and keeps the stage within its gate. The
service binary is rebuilding before targeted retention/replay validation.

The corrected synthetic fixture also supplies Docker's container Name; this was
the remaining invalid deletion-proof field after replacing its placeholder ID.
All 24 focused retention tests then passed. The subsequent full service run
passed 595 tests with eighteen ignored in 33.18s. All-target workspace Clippy
with warnings denied passed in 40.36s. Read-only host manager inspection reports
com.zackees.bosn.service loaded, enabled, active/running with MainPID 75359 and
ExecStart /home/niteris/.local/share/uv/tools/bosn/bin/bosn. That is the installed
binary, not candidate-SHA confirmation. The general lint suite is running next.
An event-history audit found append_event currently inserts without trimming;
job memory already has terminal retention but registry event/generation storage
bounds still need implementation/evidence. The acceptance audit now reflects
implemented native deletion crash recovery instead of its earlier design note.

General lint completed with every gate passing except client.rs's length (1002
lines). Authenticated RPC transport was extracted into client_transport.rs;
client.rs is now below the limit. The targeted repository length gate and
`git diff --check` passed. The service binary is compiling again, after which
the slower-adoption RPC regression will validate the moved transport boundary.
No storage-history bound has been added yet; its audit remains open.

History audit found ordinary events grow on every lifecycle transaction with no
cap. Normal appends now retain the newest 4096 event IDs and validate finite
 timestamps, kind <=256 bytes and detail <=64KiB before insertion. A regression
writes three full history windows, verifies the count/newest record each cycle
and checks SQLite allocation plateaus in later cycles. Legacy import preserves
source event rows/IDs before normal append aging; it is not silently rewritten.
Generation writes were found only in legacy import, with no native service
producer, so they are not currently a repeated-run growth source. The new
registry unit binary compiled and the isolated history test is running.

CI audit found cache policy discovery and maintenance supervision returned
permanently when machine admission timed out. Both now back off for sixty
seconds with cancellation and retry instead of silently ending maintenance.
The service binary compiled; the existing discovery/outage/cancellation test
passed (the private-Docker periodic helper test remains ignored). The live
ci_live lifecycle binary compiled and is running the real success/failure/
timeout/client-kill/daemon-kill host-object cleanup test inside Docker with host
network access for nested-engine endpoints. Separate cache-scope audit found
shared CI cache volumes remain deliberately pinned, and archive policy requires
verified coordinated enrollment; default cache storage bounds are therefore not
yet proven by managed-object GC. That remains part of the issue acceptance work.

The CI lifecycle process remains live. Its first real warm-up run completed
successfully with cleanup=removed under act2.15; later runs are progressing in
the same process, so it has not been restarted. Nested Docker emits an image
signature/index warning but the observed run still succeeded; that warning alone
is not a terminal test result. The CI state audit confirms native completion
keeps 200 finished records and ten source snapshots via prune(None,None).
The command named runners prune-cache operates on those saved runs/snapshots,
not shared-volume archives. docs/ci.md now states this explicitly and identifies
the enrolled archive policy/explicit idle clear path and cold-cache consequence.
The automatic shared archive policy/default enrollment gap remains unresolved;
this documentation correction is not a claim that the cache volume is bounded.

### Real CI lifecycle validation completed

The existing `ci_live` integration test
`the_host_engine_is_unchanged_after_every_way_a_run_can_end` passed against the
candidate native CLI and live Docker engine: one passed, zero failed, in
267.16 seconds. The run covers normal success, workflow failure, timeout,
client interruption, and daemon interruption/restart. Output is recorded in
`/tmp/bosn-545-ci-live-lifecycle.log`. This proves that test's engine lifecycle
contract; it does not prove bounded shared archive storage. Current production
planning still selects `CacheRoute::Legacy`, and `load_engine` refuses configured
cache retention until verified enrollment exists. Those paths remain an explicit
implementation gap before the repeated-run storage criterion can be claimed.

The candidate now defines the default archive policy centrally: 8 GiB per
repository, 32 GiB for the machine cohort, 30-day maximum age, seven-day unused
age, and a 60-second maintenance interval. Both policy boundary tests passed in
the isolated Docker test runner. These are archive payload ceilings, not total
allocated filesystem ceilings. The default is not yet selected by production
planning: verified migration, durable route publication, and exclusion of
nonparticipating historical writers must be wired before activation.

### Policy agreement rejects redirected filesystem objects

Enrollment-path inspection found that policy agreement followed symlinks even
though policy discovery rejected them. A new isolated regression failed on the
redirected cache directory before the fix. Agreement now rejects symlinked
directories, records and locks, and nonregular existing records/locks before
opening the lock or publishing policy. The regression verifies foreign files
remain unchanged. The complete policy module reports three passed, zero failed,
one existing private-engine test ignored; `git diff --check` passed. These checks
cover static redirection, not adversarial concurrent filesystem replacement.

### Shared routing record boundary

Added a bounded typed routing record binding schema, repository namespace,
validated immutable archive policy and the imported source fingerprint. Its
isolated test passed, including cross-repository and policy disagreement,
unsupported schema, invalid fingerprint, disabled ceiling, unknown fields and
oversized input. Parsing does not establish writer exclusion or fresh inventory.
Production still does not select this record. Migration audit also identified
that the callable import releases its exclusive lease before host-side receipt
validation and routing publication; enrollment must span that interval rather
than treating a successful import as sufficient authorization.

### Migration session retains participating writer exclusion

Added an owned interactive Docker transport and a bounded migration protocol.
The remote process holds its exclusive legacy lease across import, audit,
host-side validation and immutable route publication. Publication syncs the
staged record and routing directory before acknowledging; conflicting records
are preserved. EOF/abort releases the lease, and a separate 180-second remote
timeout bounds a client interruption even if Docker disconnect does not end the
remote process. The isolated process test passed in 0.21 seconds: a competing
shared lease is refused after both import and audit, exact route publication
releases it, and stdin disconnect releases it without another publication.
Service check and compilation passed. The fixture substitutes the act binary;
real act import, interactive Docker transport cancellation, historical-writer
exclusion and production enrollment remain to validate and connect.

The owned migration session now has a bounded typed host reader: 150-second
absolute protocol deadline, 64 KiB per command/buffer, 256 KiB cumulative output,
and validated exit framing. Import, receipt and current inventory checks run on
the same lease; publication requires all three. A second import attempt refuses
even after a failed response, because publication can be unknown. The transport
regression uses real owned subprocess sessions and verifies truncated, oversized,
invalid-exit, invalid-report and expired responses cannot publish or repeat
import. Both migration session/transport tests passed (0.22 seconds). This still
needs real Docker/act verification, matching receipt/import evidence, durable
journal wiring, old-writer exclusion and production activation.

Receipt/import consistency now requires identical imported count, imported
bytes, retained-source byte total and the frozen repository ceiling. Current
inventory must contain at least the observed imported entries and bytes. All
three migration protocol tests passed in isolation (0.21 seconds), including
valid-but-disagreeing receipts that previously could reach publication.

Added trusted registry actor commands to begin/read a migration and record its
publication evidence. The actor test verifies an independent reader sees intent
after acknowledgement, duplicate intent refuses, conflicting publication cannot
replace evidence, and the committed receipt survives writer reopen. It passed
in isolation (0.07 seconds). This supplies durable actor APIs; production runtime
still needs to call them in the enrollment sequence before route activation.

### Migration session commits intent and evidence before remote mutation

The public session import entrypoint now commits a typed migration intent through
the registry actor before issuing the remote import. The unrecorded import helper
is private. Route publication requires the session's journal and awaits committed
publication evidence while retaining the migration lease. Existing intent refuses
a new import instead of blindly retrying an unknown publication. A real subprocess
test reads SQLite independently before accepting the import command and verifies
a second session never enters import. All four migration session/transport tests
passed in isolation (0.21 seconds). Production runtime selection, recovery of an
existing intent, exclusion of maintenance and historical writers, and live
Docker/act validation remain outstanding.

### Migration excludes maintenance and reconciles interrupted publication

A focused regression reproduced an independent maintenance lease entering the
migration interval. The remote migration session now holds both the machine
maintenance lock and the participating legacy writer lock until exit.

Existing-intent recovery reads the durable actor record and the remote import
receipt without repeating import. It refuses a different frozen ceiling or
conflicting committed evidence, and still requires fresh inventory before route
publication. The recovery test covers lost acknowledgement, already committed
evidence, conflicting fingerprint, different ceiling and missing intent; its
remote subprocess asserts the only command received is `receipt`. All five
migration session/transport tests passed in isolation (0.45 seconds). Historical
writer exclusion, real Docker/act validation and production enrollment selection
remain outstanding; this is not yet default archive retention activation.

### Pinned act2.15 interactive Docker proof

Ran the actual leased-session integration against Docker with the pinned act2.15
binary (`b4be8d7ef98729ad16a9a6ddba331f1d2b0feb8abd52eb9e5f3a93155fb4f1df`).
The test imported a copied closed legacy store, read its receipt and inventory,
verified concurrent maintenance received exit 75, committed publication evidence,
published and parsed the route, then verified exact helper-container absence.
It passed in 1.19 seconds; output is in
`/tmp/bosn-545-migration-session-live.log`. Original machine cache was mounted
read-only only to obtain the fixture and verified release bytes. The fixture is
an empty store, so this proves actual protocol/CLI compatibility and lifecycle,
not populated warm-cache preservation or the repeated-run storage requirement.
Normal runtime routing remains legacy until enrollment and historical-writer
exclusion are connected and validated.

### Execution consumes verified shared routing

The production Docker backend now reads the shared repository route immediately
before execution. A valid published default-policy record requires the matching
existing machine policy and selects cohort arguments; absent records retain the
legacy route, while malformed, oversized or redirected records fail without
fallback. The isolated routing test passed (0.33 seconds). The actual pinned-act
Docker fixture now also establishes policy agreement and calls the production
route selection method after publication; it passed in 1.35 seconds with exact
helper removal. This connects existing publication to execution. Initial
automatic enrollment and historical-writer exclusion remain unfinished.

### Current service regression and source gate

After connecting published routing to execution, the full isolated service suite
passed: 605 passed, zero failed, 19 explicitly opt-in tests ignored, 33.29 seconds.
Output is `/tmp/bosn-545-routing-full-unit.log`. Strict service/engine all-target
Clippy with warnings denied also passed; output is
`/tmp/bosn-545-migration-clippy.log`. These gates cover the current source, not
automatic initial enrollment, which still has no runtime lifecycle hook. The
remaining integration must handle a fresh missing legacy store and verify
historical writer exclusion before claiming the default archive contract.

### Fresh-store initialization through the pinned binary

Added source initialization inside the continuously leased migration protocol,
after the actor intent commits. A fixed dry-run workflow with an always-skipped
job initializes and closes the empty cache without creating a job container.
Existing stores are preserved; symlinked or non-directory sources refuse. The
initial real fresh-store test waited in act's default-image prompt. Inspection
confirmed the act process; the owned test process was killed to finish the failed
test, and its output is `/tmp/bosn-545-bootstrap-prompt-red.log`. Bootstrap now
supplies the pinned runner mapping and redirects stdin from `/dev/null`.

Both actual Docker tests passed afterward (1.73 seconds): fresh initialization
and enrollment, and enrollment of a copied closed legacy store. Each verifies
durable evidence, published route consumption, maintenance exclusion and exact
helper removal. Output is `/tmp/bosn-545-migration-session-live.log`. Five focused
unit tests also passed before that final bootstrap argument correction; those
need refreshing with the final source gate. The automatic runtime lifecycle
hook and historical-writer exclusion still remain to connect.

### Prepared-engine cache lifecycle hook

The production lifecycle now invokes typed cache-route preparation after engine
and source preparation and before workflow listing/execution. It uses the run's
absolute deadline and cancellation token, then re-verifies the execution claim.
The Docker implementation currently consumes verified published routing; initial
enrollment still needs to be added there. A fault/timeout test verifies neither
case lists or executes a workflow and both retire the engine with a terminal
registry record. It passed in 1.17 seconds. The complete lifecycle unit group
passed: 15 passed, zero failed, one opt-in live test ignored (2.12 seconds).

Integration inspection also confirmed `CiRuntime::maintain_existing_cohort`
currently delegates directly to the cache supervisor without checking the state
directory's explicit automatic-retention opt-out. That must be corrected while
wiring default enrollment; otherwise an enrolled cache could be pruned despite
`auto_retention = false`.

### Cache supervisor follows automatic-retention opt-out

A focused runtime regression failed before the fix: with explicit false, the
backend cache supervisor still started once. The runtime now checks the same
retention configuration as owned-resource GC, waits while disabled, and cancels
the current maintenance cycle when configuration changes to false. It rechecks
every 60 seconds and resumes after enabling without restarting the daemon.
Existing cleanup intent recovery remains separate from archive pruning.

The startup opt-out test passed (0.09 seconds). A runtime test with a shortened
poll interval verifies disabled startup, enable, pause, re-enable and bounded
shutdown; it passed in 0.16 seconds. These tests drive the real runtime wrapper
and a cancellable backend, not Docker archive deletion. Automatic initial
enrollment and policy selection for workflow cache-server retention are still
unfinished; this proves supervisor gating only.

### Policy publication durability and workflow opt-out

Added a regression injecting failure into the policy staging-file disk flush.
Before the fix the policy was published and the command succeeded; after the
fix it exits with the flush failure before creating the canonical record.
Agreement now flushes the staged record before hard-link publication and the
parent directory before acknowledgement. Four machine-policy tests passed in
isolated Docker, with the existing live-engine discovery test ignored.

Normal run planning now snapshots `automatic_retention_enabled` into the typed
act invocation. An explicit false passes `--no-cache-server`: act2.15's age
retention remains enabled when its server runs, and its age/interval validation
does not allow zero values. This preserves archived storage at the cost of
local archive-cache access for opted-out runs; documentation states that tradeoff.
A focused command regression failed before argument wiring, then all five
engine command tests passed. A separate actual pinned-act dry-run acceptance
passed for both a missing archive directory (never initialized) and an existing
store (sentinel contents and directory entries unchanged). The fixture uses
explicit loopback server addresses because it has no Docker daemon connection.
The verified binary SHA-256 remains
`b4be8d7ef98729ad16a9a6ddba331f1d2b0feb8abd52eb9e5f3a93155fb4f1df`.
Source-length lint and diff whitespace checks passed. This does not claim
cross-daemon opt-out propagation or automatic production enrollment is finished.

The refreshed complete service unit run passed 610 tests, with 21 ignored,
631 total, in 33.08 seconds (`/tmp/bosn-545-optout-full-unit.log`). Strict
service/engine all-target Clippy initially identified a 101-line live fixture
function and a test module preceding implementation items. Extracted fixture
creation and moved the test module to the end; the strict gate then passed in
35.10 seconds (`/tmp/bosn-545-optout-clippy.log`). These last edits only organize
test code; the unit and real act results cover the production changes above.

### Normal CI admission now performs automatic enrollment

The actual Docker regression initially failed because the production admission
hook returned a Legacy route for a fresh namespace. Normal planning now carries
the validated configured/default policy and the explicit opt-out into that hook.
Admission reads an existing route, agrees the machine policy, acquires the
continuous migration/maintenance lease, rechecks publication, records or resumes
its durable intent, verifies import/receipt/current inventory, publishes and
selects Cohort. Valid `[cache]` settings no longer fail prematurely in engine
planning. An opted-out run still skips new enrollment.

A second actual regression showed concurrent first admissions failed at lease
contention. Migration now reports a distinct busy acknowledgement; admission
retries within 150 seconds and rechecks routing so concurrent callers reuse the
first publication. The immutable policy lock waits for at most 150 short polls.
The Docker image's BusyBox does not implement flock's `-w`; wrapping flock in
BusyBox timeout also left an inherited lock holder until the guardian expired.
The bounded poll and explicit unlock avoid both problems. Current live tests
pass simultaneous first admissions plus subsequent reuse.

A bounded typed census checks all cache-volume container attachments, including
stopped containers that could restart. Writable attachments must identify the
participating workflow or machine-maintenance protocol; read-only readers are
allowed. Maintenance creation emits its coordination label. Missing/old labels,
missing mount fields, changed IDs, incomplete inspection and excessive/duplicate
IDs refuse enrollment. This does not turn two census snapshots into permanent
exclusion of future older daemon admissions. Complete cohort producer upgrades;
uncataloged historical daemon/future-writer exclusion remains a separate audit
item, and source-copy retention remains unfinished.

### Interrupted pre-import admission recovers under the lease

The real Docker/pinned-act regression with a previously committed intent and
no initialized source initially failed looking for a nonexistent receipt. The
leased protocol now reports fresh typed destination presence. Recovery retries
the original nonce only when that destination is absent and no publication
proof committed; existing destinations require receipt reconciliation. A failed,
partial, redirected or contradictory read never authorizes import. A published
but missing destination preserves the committed proof and refuses overwrite.
The four real migration/admission cases pass (2.54 seconds), including the
pre-bootstrap interruption boundary and exact fixture-container absence. The
copied legacy fixture is empty; these cases do not prove populated warm-cache
migration or shared-volume allocation ceilings. Strict service/engine all-target
Clippy passed in 36.82 seconds before the additional recovery refusal unit test.


### Refreshed gates and native acceptance checkpoint

The complete service unit suite passed 612 tests, with 23 ignored, 635 total,
in 33.42 seconds. Recovery refusal coverage preserves the original nonce and
committed proof for missing intents, changed ceilings, existing destinations
with invalid receipts, partial/failed reads and published destination loss.
Strict service/engine all-target Clippy passed after extracting that test's
fixed protocol fixture.

The native Docker refresh exposed fixture interference after unrelated shared
cache resources appeared on the host. The export/deletion crash fixtures had
attempted a cold registry startup and correctly hit the missing-identity guard.
They now explicitly initialize their intended registry before starting the
crash scenario; production refusal remains intact. Fixture UUIDs derive from
their unique temporary state paths rather than repeated fixed IDs. Serial
acceptance execution prevents one fixture's deletion from invalidating another
fixture's census. No deletion counts, exact absence assertions or pin checks
were loosened. The refreshed driver passed two manifest cases (10.20 seconds),
two peer/startup cases (26.99 seconds), and all eight preparation/crash cases
(28.21 seconds). See `/tmp/bosn-545-enrollment-retention-live.log`.

Full lint passed with the locked dev dependencies installed using
`uv sync --only-dev --locked`, followed by `UV_NO_SYNC=1 ./lint`. An initial
plain lint launch unnecessarily started a project wheel build; its exact
process group was interrupted, and its disposable worktree target directory
was moved to trash with safe-rm. The intended shared target was preserved.
The final strict Clippy refresh also passed. These results are a local
implementation checkpoint, not evidence that all retrospective criteria or
GitHub delivery have finished.
