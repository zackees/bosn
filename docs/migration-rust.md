# Rust migration contract (Phase 0 working inventory)

This is the authoritative migration inventory for issue #153.  It describes the
implemented Python behavior at this checkout, not a promise of Python API compatibility.
The Rust port preserves state and safety semantics; it may deliberately replace command and
daemon protocol syntax. Measurements below provide a reproducible starting point; they
are not performance promises or evidence that the Rust migration is implemented.

## Phase checklist

- [x] Inventory current Python CLI, Compose, guest, state, and consumer-facing behavior.
- [x] Characterize a portable synthetic Python-v4 registry fixture and its relations.
- [x] Specify proposed typed client/config/protobuf boundaries and kernal-api ownership.
- [ ] Record reviewed consumer inventory beyond this repository.
- [x] Measure startup, idle RSS, status, ensure-reuse, and build/package cost.
- [ ] Review the matrix and classify any behavior intentionally removed before Rust coding.

## Landed implementation checkpoints

- [Bosn #154](https://github.com/zackees/bosn/pull/154): Python state fixture and
  runtime baseline. The broader Docker baseline failure is recorded below.
- [kernal-api #192](https://github.com/zackees/kernal-api/pull/192): optional
  SQLite facade. Exact registry publication and native platform proof remain open.
- [Bosn #155](https://github.com/zackees/bosn/pull/155): pure Rust ownership,
  per-holder lease observations, retention/idle-stop decisions and ordering.
  Thirteen Rust tests pass; this does not implement persistent registry access,
  daemon mutation, or the client front ends. See `docs/rust-domain.md`.
- [Bosn #159](https://github.com/zackees/bosn/pull/159), merge `39e9253`:
  guarded Python-v4 import preparation. Import activation and engine
  reconciliation remain pending.
- [kernal-api #207](https://github.com/zackees/kernal-api/pull/207), merge
  `ed197c5`: owner-private bounded file reads used by the guarded import path.
  This does not complete cutover or activation.

No implementation phase is marked complete solely because one checkpoint landed.

### Rust Docker transport checkpoint (issue #153)

`bosn-engine` provides a local Docker-CLI transport over the pinned public
`kernal-api` process/session facade. It has bounded separated diagnostic capture,
tagged streaming output, explicit deadline/cancellation and direct-client reaping.
It accepts trusted, product-selected Docker argv/environment and is not an operation
authorization boundary or sandbox; it is deliberately not exposed through daemon IPC.
Killing a local `docker exec` client is not evidence that its remote command stopped;
future job cancellation must validate Bosn ownership of the remote container before
reporting it stopped. Health-monitor probing, interactive inherited-TTY execution,
container lifecycle/build/pull/create/start/remove policy, and that remote cancellation
protocol remain subsequent engine work, rather than silently claimed by this transport.
The transport acceptance tests were added RED first (missing crate import), then GREEN:
native synthetic-child tests cover stream separation, ordinary exit 130, spawn versus
deadline classification, oversized output without a newline, pre-exit streaming,
cancellation, and native post-cancellation identity observation proving client reaping.

### Declared setup-task primitive

`bosn-setup` can now execute exactly one named task retained in a validated
`SetupPlan` after a matching `PreparedImage` receipt is supplied.  It verifies
the canonical workspace, prepared image identity, task name, plan receipt,
document-derived environment merge, workspace-relative workdir, and declared
existing workspace mounts before reaching the engine.  Its testable command is
the finite `SetupTaskCommand::Run` semantic shape; the `DockerEngine` adapter
may emit only `docker run --rm` with document-derived bind mounts, environment,
workdir, the observed image identity, and `sh -lc` with the declared task text.
The primitive is submitted through typed daemon IPC and the bounded native,
Python, and MCP job-submission surfaces. It deliberately still has no registry
persistence, container lifecycle, or setup-ensure wiring.

### Setup-app ensure core primitive

`bosn-setup::ensure_setup_app` is the next deliberately narrow core primitive
for one-file Docker Linux apps. It derives a deterministic managed container
name and ownership labels solely from the validated setup-plan content hash,
then performs only inspect, create-if-absent, and start-if-stopped through the
finite `SetupEnsureCommand` protocol. Create receives only the validated
document's image receipt, mounts, environment, workdir, and optional declared
`app.command` (as `sh -lc`). An existing candidate must exactly prove the
expected image and Bosn labels or is refused before mutation. This is not yet
daemon/CLI/Python/MCP exposed or registry-backed, and it never deletes,
replaces, stops, adopts, or garbage-collects a container.

## Current surface and characterization references

### Main CLI (`bosn`)

| Capability / verb | Contract to retain or intentionally decide | Characterization references |
| --- | --- | --- |
| `run`, `shell`, `ensure` | converge persistent stack, execute command / interactive shell, retain content-keyed resources | `tests/test_converge.py`, `tests/test_cli_verbs.py`, `tests/test_scenario_docker.py` |
| `tasks` | list stacks/tasks/digests/readiness without mutating | `tests/test_cli_verbs.py`, `tests/test_manifest.py` |
| `jobs`, `attach`, `cancel` | bounded daemon job list, streamed observation, cancellation | `tests/test_jobs.py`, `tests/test_cli_jobs.py`, `tests/test_daemon_jobs.py`, `tests/test_jobs_docker.py` |
| `status`, `doctor` | bounded diagnostic status and Docker reachability/clock reporting | `tests/test_cli_verbs.py`, `tests/test_doctor_integrity.py`, `tests/test_engine.py` |
| `gc`, `done` | dry-run default; collection only after ownership/lease/final recheck; mark workspace completed | `tests/test_gc.py`, `tests/test_retention.py`, `tests/test_accounting.py`, `tests/test_accounting_pressure.py` |
| `adopt` (including `--legacy`, transfer) | label-based lost-registry recovery and explicit legacy/volume migration | `tests/test_recovery.py`, `tests/test_legacy.py`, `tests/test_resources.py`, `tests/test_cli_verbs.py` |
| `reconcile-volume`, `release-volume` | preview-first repair/release of a declared volume; `--apply --yes`; pinned/attached/foreign protection | `tests/test_recovery.py`, `tests/test_cli_verbs.py`, `tests/test_converge.py` |
| `daemon-stop`, hidden `__daemon`, login autostart | authenticated singleton, idle retirement, maintenance, graceful stop and platform launchers | `tests/test_daemon.py`, `tests/test_autostart.py`, `tests/test_platform_native.py` |
| `init` | Compose-to-`bosn.toml`, refuse overwrite | `tests/test_docker_cli.py`, `tests/test_cli_verbs.py` |
| global options | engine/state/manifest selection, JSON envelopes, unified policy precedence | `tests/test_options.py`, `tests/test_config.py`, `tests/test_cli_smoke.py` |

The installed `bosn` package exposes a deliberately narrow native Python API:
`Client(state_dir).status()` and
`Client(state_dir).plan_setup(workspace, config_locator, *, policy)` and the
daemon-backed `Client(state_dir).submit_setup_prepare(workspace, config_locator,
*, policy, deadline_ms, output_limit)` and
`Client(state_dir).submit_setup_task(workspace, config_locator, *, policy,
task_name, deadline_ms, output_limit)`. Both return a durable job ID promptly;
the latter can select only a declared task by its bounded semantic name.
`job_status(id)`, `job_logs(id, *, after=0, limit=64)`, and `cancel_job(id)`
are typed IPC-only observation controls. The Python boundary has no Docker
command, container, mount, environment, work-directory, or task-execution
parameters. The setup planner requires either `"online_refresh"` or
`"offline_cache_only"`;
it releases the GIL while calling the Rust `bosn-setup` pipeline and returns a
frozen receipt (`SetupPlan`) with source kind, content hash, schema, canonical
workspace, optional private asset root, ordered task names, source shape, and
`applied == false`.  It does not write the selected workspace, contact Docker
or the Bosn daemon, or apply the document.  Other Python internal modules are
not a compatibility contract.

The production extension is deliberately built with PyO3's
`extension-module` feature, so ordinary `soldr cargo test -p bosn --lib --locked`
does not embed or link CPython. The fake-daemon Python boundary integration is
available explicitly for development with an absolute interpreter and its
library directory on the loader path:

```bash
PYTHON_BIN="$PWD/.venv/bin/python"
PYTHON_LIB="$("$PYTHON_BIN" -c 'import sysconfig; print(sysconfig.get_config_var("LIBDIR"))')"
PYO3_PYTHON="$PYTHON_BIN" LD_LIBRARY_PATH="$PYTHON_LIB${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" \
  soldr cargo test -p bosn --lib --locked --no-default-features --features embedded-python-tests
```

Installed-extension behavior is covered separately through
`uv run maturin develop --locked && uv run pytest tests/test_native.py`.

### Native setup submission CLI

`bosn setup prepare` submits a durable, daemon-owned image-preparation job and
returns immediately; it never launches the daemon or invokes Docker itself.
Every input is explicit and bounded:

```text
bosn setup prepare --state-dir STATE --workspace WORKSPACE --config LOCATOR \
  (--refresh | --offline) --deadline-ms 1..=300000 \
  --output-limit 1..=8388608 [--json]
```

The receipt is `action: setup_prepare`, `submitted: true`, and a `job_id`
(the JSON form uses the same fields). Existing daemon jobs, including setup
preparation, can be observed or cancelled without launching a daemon or
invoking Docker:

```text
bosn job status --state-dir STATE --job-id ID [--json]
bosn job logs --state-dir STATE --job-id ID [--after CURSOR] [--limit 1..=256] [--json]
bosn job cancel --state-dir STATE --job-id ID [--json]
```

`bosn setup task` submits the complete daemon-owned plan, image-preparation,
and one declared-task pipeline. It returns immediately with `action:
setup_task`, `submitted: true`, and a `job_id`; use the same job observation
commands for progress and cancellation:

```text
bosn setup task --state-dir STATE --workspace WORKSPACE --config LOCATOR \
  (--refresh | --offline) --task NAME --deadline-ms 1..=300000 \
  --output-limit 1..=8388608 [--json]
```

The command submits only a named task declared in the validated setup document.
It never starts a daemon or invokes Docker itself, and it accepts no task
command, container, mount, environment, working-directory, per-request state
override, or other engine controls.

`bosn setup ensure` submits the complete daemon-owned plan, image-preparation,
and ownership-safe application ensure pipeline. It returns promptly with
`action: setup_ensure`, `submitted: true`, and a `job_id`:

```text
bosn setup ensure --state-dir STATE --workspace WORKSPACE --config LOCATOR \
  (--refresh | --offline) --deadline-ms 1..=300000 \
  --output-limit 1..=8388608 [--json]
```

The daemon may create an absent application or start a matching stopped one.
It refuses foreign, incomplete, or mismatched candidates. It does not replace,
delete, adopt, or garbage-collect an existing application. The native command
never starts a daemon or invokes Docker directly, and exposes no container,
image, command, mount, label, environment, work-directory, network,
privilege, task, state-override, or raw engine arguments.

Log replies include `retained_from`, `next`, and `gap`, so callers can retain
their cursor and detect bounded-log eviction. The command surface is limited
to typed IPC observation; it provides no daemon start, Docker, registry, or
raw process controls.

### `bosn-docker` / `bosn-compose`

| Category | Existing capability | Characterization references |
| --- | --- | --- |
| Governed | `init`; Compose `up`, `down`, `logs`, `ps`, `build`, `run`, `exec`, `config` with overlay labels, registry reconciliation, client-owned compose leases | `tests/test_frontdoor.py`, `tests/test_docker_cli.py`, `tests/test_compose.py`, `tests/test_compose_e2e_docker.py` |
| Accepted Compose flags | global `-f`/`--file`; `up -d`/`--detach`, `up --wait`; `down -v`/`--volumes`, `down --remove-orphans` | `tests/test_frontdoor.py`, `tests/test_docker_cli.py` |
| Forwarded only | `version`, `info`, `login`, `logout` | `tests/test_frontdoor.py`, `tests/test_docker_cli.py` |
| Refusal boundary | all other Docker verbs and undeclared Compose flags/subcommands fail closed with a remedy; no raw resource mutation passthrough | `tests/test_frontdoor.py`, `tests/test_docker_cli.py`, generated `docs/docker-support.md` |
| Compose syntax subset | services/images/build, profiles, dependencies, healthchecks, environment, workdir, ports, named volumes/networks, tmpfs, anchors/merge; unsupported keys fail explicitly | `tests/test_compose.py`, `tests/test_gen_docker_support.py` |

The Rust front door must retain this explicit supported/refused catalog until a reviewed
replacement catalog says otherwise.  It must not infer support from Docker's broader CLI.

### macOS x86-64 guest stacks

| Capability | Contract | Characterization references |
| --- | --- | --- |
| Manifest kind | `macos-x64-guest`, explicit Apple-license acknowledgement, guest sizing/ports/readiness/payload fields; guest data volume can be pinned | `tests/test_guest.py`, `tests/test_manifest.py` |
| Safe preflight | Linux only; require `/dev/kvm` and `/dev/net/tun`; conservative AMD one-core default | `tests/test_guest.py`, `tests/test_converge.py` |
| Lifecycle | create with KVM/tun/NET_ADMIN and SSH port; no bind mounts; wait for SSH with guest logs on failure | `tests/test_guest.py`, `tests/test_converge.py` |
| Execution | optional payload copied with `scp` each task, SSH command/shell, real exit propagation and ambiguous-255 event | `tests/test_guest.py`, `tests/test_cli_verbs.py`, `tests/test_converge.py` |
| Explicit gap | live `dockurr/macos` / KVM proof is not in CI; unit tests prove only manifest/preflight/argv/transport behavior | `docs/macos-guest.md`, `tests/test_guest.py` |

### Domain capabilities and consumers

| Area | Existing contract | References |
| --- | --- | --- |
| Manifest and identity | TOML stacks/tasks; Dockerfile/COPY/dockerignore digest; build args/external image identity; scopes, mounts, tmpfs, env/workdir | `src/bosn/manifest.py`, `tests/test_manifest.py`, `tests/test_converge.py` |
| Engine lifecycle | Docker CLI build/pull/create/start/exec/remove; generation rollover and final inspection/recheck | `src/bosn/engine.py`, `src/bosn/converge.py`, `tests/test_engine_docker.py`, `tests/test_converge_docker.py` |
| Ownership/recovery | complete label set plus registry UUID is the only ownership proof; unknown/incomplete/foreign are protected; explicit adoption | `src/bosn/labels.py`, `src/bosn/recovery.py`, `tests/test_labels.py`, `tests/test_recovery_docker.py` |
| Accounting/retention | managed/foreign/uncertain accounting, warm expiry/supersession/pressure, shared consumers, pinned volumes | `src/bosn/accounting.py`, `src/bosn/retention.py`, `tests/test_accounting_docker.py`, `tests/test_retention.py` |
| IPC/daemon | loopback authenticated JSON requests, streaming heartbeat, bounded jobs and restart recovery | `src/bosn/ipc.py`, `src/bosn/daemon.py`, `tests/test_daemon.py`, `tests/test_daemon_jobs.py` |
| Git completion | derive finished workspaces from Git state conservatively | `src/bosn/gitstate.py`, `tests/test_gitstate.py` |
| Integration consumer | Soldr manifest/workflow guidance | `docs/soldr-integration.md`, `examples/soldr.toml`, `tests/test_shims.py` |

## Downstream migration inventory (read-only discovery, 2026-09-13)

These are identified consumers, not completed consumer migrations. Sibling
checkouts were inspected without modification; implementation stays in this
repository until a reviewed downstream change is ready.

| Consumer | Verified integration | Required release proof |
| --- | --- | --- |
| Soldr (`942f4acf`) | `bosn.toml` declares cook/seed/warm stacks, shared cache volumes, workspace mounts and tasks; `ci/bosn_workspace_test.py` handles bootstrap-to-source handoff | Load/migrate the real manifest, preserve cache scopes and readonly mounts, execute the workspace task; retain `tests/test_perf_local.py` handoff/cleanup tests |
| clud (`bce6aee2`) | `bosn.toml`, `bosn/Dockerfile`, bundled `clud-bosn` and `clud-preloop` skills, `crates/clud-bin/src/skills_tests.rs` | Update install/command examples and support/refusal claims, preserve manifest tasks and volume scopes, run bundled-skill tests and a representative task |
| kernal-api (`fcfc2ed`) | `bosn.toml` and `docker/bosn.Dockerfile` | Migrate manifest and prove its declared task through the installed Rust CLI |
| Hermes | New consumer, not an existing Bosn integration found in the inspected checkout | Pin client version; prove MCP initialization, tool discovery, setup, run, logs and cancellation |

The clud skill currently describes an older Compose subset and rejects `up -d`,
unlike the current Bosn implementation; migrate from the source-tested catalog
above rather than preserving those stale claims. This targeted inventory is not
a claim to have found every external user or to have validated downstream tasks.

## Python v4 registry contract

The registry is file-backed system/resource state, not a crawler index.  It opens WAL,
enables foreign keys, uses a five-second busy timeout, serializes its writer, and permits
independent read-only opens that never create or migrate state.  Lifecycle decisions use
`BEGIN IMMEDIATE` through the final engine mutation/recheck.

| Table | Key / relationships | Invariants to import |
| --- | --- | --- |
| `meta` | `schema_version`, stable `registry_id` UUID | accept v4 only for direct import; reject newer safely |
| `resources` | resource id; engine kind/name identity | retention is `warm` or `pinned`; engine identity reconciles in place |
| `resource_uses` | `(resource, workspace, stack, generation)` | every consumer matters before shared resource retirement |
| `leases` | resource FK, PID/start identity and heartbeat/TTL | live/uncertain owner blocks destructive actions; FK cascades with resource |
| `execution_sessions` | container, engine, client process, JSON lease IDs | durable execution ownership participates in restart recovery |
| `volume_creation_intents` | volume name, serialized complete labels and manifest identity | intent exists before creation and enables repair, never name-based guessing |
| `generations` | `(workspace, stack, digest)` | superseded timestamp is distinct from current generation |
| `events` | ordered autoincrement event log | preserve compatible history or explicitly map it during import |

Fixture: `tests/fixtures/migration/create_python_v4_registry.py DESTINATION` creates a
single checkpointed SQLite file using only stdlib `sqlite3`; it is synthetic and contains no
credentials. `tests/test_migration_fixture.py` proves all eight tables and includes a stable
registry UUID, complete Bosn labels, two consumers of one image, a pinned volume, active
lease/session, creation intent, generation rollover, and an event. Rust import tests should
copy/create this fixture then assert their import result, rather than importing Python code.

## Proposed Rust boundary (not implemented)

The supported Python distribution continues to import as `bosn`, but exposes a deliberate
typed client rather than current internals:

```text
Client::plan_config(SetupRequest) -> Plan
Client::apply_config(ApplyRequest) -> JobId | ApplyResult
Client::ensure(StackSelector) -> JobId | EnsureResult
Client::run(RunRequest) -> JobId
Client::status(StatusRequest) -> Status
Client::jobs(JobCursor) -> Page<Job>
Client::logs(JobId, LogCursor) -> Page<LogEvent>
Client::cancel(JobId) -> CancelResult
Client::done(WorkspaceSelector), gc(GcRequest), adopt(AdoptRequest),
reconcile_volume(ReconcileVolumeRequest), release_volume(ReleaseVolumeRequest)
```

`SetupDocumentV1` is a versioned local-path/HTTPS input with source provenance, explicit
workspace, inline Dockerfile or pinned image, optional bounded inline companion files,
stacks/tasks/mounts/env/workdir, and policy overrides limited to app scope.  Parsing and
planning are inert; apply validates again in the daemon.  Remote cache stores resolved URL,
content hash, schema version and selected workspace; path traversal, unpinned assets,
unsupported schemes and oversized inputs are rejected/redacted.

The replacement internal protocol is Bosn-owned versioned protobuf over kernal-api local
authenticated transport, not MCP JSON:

```proto
message Envelope { uint32 protocol_version = 1; string request_id = 2; oneof body {
  Request request = 3; Progress progress = 4; Result result = 5; Error error = 6;
}}
message Request { oneof operation { PlanConfig plan_config = 1; ApplyConfig apply_config = 2;
  Ensure ensure = 3; Run run = 4; Status status = 5; Jobs jobs = 6; Logs logs = 7;
  Cancel cancel = 8; Gc gc = 9; Adopt adopt = 10; ReconcileVolume reconcile_volume = 11;
  ReleaseVolume release_volume = 12; Done done = 13; }}
```

Frames, cursors, logs and deadlines are bounded; all results use stable machine error codes.
MCP remains its own JSON-RPC stdio boundary, maps semantic tools to this client, writes only
protocol to stdout, and does not make disconnect imply cancellation.

## Ownership mapping to `kernal-api`

Read-only inspection of sibling `../kernal-api` (current checkout, no edits) confirms the
architecture reserves generic process, filesystem/locking, hashing, HTTP, IPC and autostart
mechanisms for kernal-api, while applications own their policy/protocols.  No public SQLite
facility was found.  On 2026-09-13, GitHub's `releases/latest` endpoint returned 404 and its
tags listing was empty; `soldr cargo info kernal-api` also found no crates.io package.  Thus an
exact published kernal-api dependency remains a Phase 1 release gate, not an assumed version.
Bosn must not directly depend on kernel private backend crates.

The SQLite prerequisite subsequently landed in
[kernal-api PR #192](https://github.com/zackees/kernal-api/pull/192), merge
`10e558a9f2eb51c2989c89d05b13cf7636bd374e`. Its opt-in facade supplies
WAL/read-only connections, bounded prepared-query results, immediate transactions,
integrity/checkpoint and non-overwriting consistent backup through private bundled
SQLite. Thirteen SQLite tests, the facade-policy test, targeted Clippy and two
dependency-boundary unit tests passed locally after integration with upstream.
This is source readiness, not evidence of a published crate or native Windows/macOS
validation; those Phase 1/platform gates remain open.
`soldr cargo package --locked --features sqlite` also passed verification of the
extracted registry package at that merge (47.55 seconds on this host). This checks
the packaged SQLite feature graph, not every optional kernel feature or publication.

### Python-v4 offline-import quiescence gate (2026-09-13)

The v4 importer remains pending, but its cooperative Python bridge is implemented
in this checkout. A safe importer must prove that the legacy Python daemon cannot
write the source for the complete snapshot/import interval; a caller-supplied
`quiesced: bool`, PID file, or one-time ping is not such proof.

The Python daemon cannot be guarded by acquiring its SQLite file lock: it does not
participate in `kernal-api`'s advisory file-lock protocol.  Its actual singleton is
the deterministic loopback TCP endpoint calculated by `bosn.daemon.port_for` (with
an explicit `BOSN_PORT` override).  However, `Daemon.__init__` opens
`registry.sqlite3` before `serve_forever` binds that endpoint, while `shutdown()`
closes the server endpoint before it has necessarily closed the registry (background
work can defer registry close).  Consequently, either a free port or even a held
port alone is insufficient evidence that no Python writer remains.

The pinned public kernel API has SQLite backup/checkpoint and process-identity
facades, plus local-IPC listeners, but no public loopback-TCP endpoint ownership.
Bosn therefore uses a cooperative bridge rather than claiming to reserve the old
TCP singleton: every writable bridge-capable Python `Registry` holds a shared lock
on `registry.migration.lock` from before marker recheck through successful SQLite
close (including deferred daemon close); Rust's public kernel filesystem facade
takes the same lock exclusively. A real cross-language subprocess test proves the
POSIX `flock` interaction. On Windows the Python bridge calls `LockFileEx` on the
same single byte at offset `1 << 62` that kernal-api uses, rather than assuming a
default third-party lock convention is compatible.

The authenticated `migration-cutover` daemon verb closes mutation/stream admission,
refuses active requests/jobs/execution ownership, requests daemon shutdown, and
publishes the private create-new `rust-cutover-v1.json` marker with the source
registry UUID. Existing bridge holders must close before Rust can obtain exclusive
ownership; every later writable Python registry rechecks that marker under its
shared lock and refuses to open. A malformed, unreadable, dangling, or conflicting
marker fails closed and is never replaced. The marker is deliberate state-directory
metadata, not a write to the source SQLite database.

This only fences upgraded cooperative binaries. An older Python release cannot be
inferred absent from a missing port or state file; activation must authenticate its
shutdown and verify its exact process identity exited before starting the bridge
release, otherwise import is refused. That old-release activation/proof consumer is
not implemented yet, and neither is the importer; no current command represents a
validated import authorization. The eventual importer must validate the marker/proof,
retain exclusive ownership through SQLite's non-overwriting consistent backup and
destination transaction, reject live or unclassifiable lease/session ownership, and
leave the destination reconciliation-required.

| Concern | Owner | Required migration rule |
| --- | --- | --- |
| SQLite connection/transactions/read-only/WAL/busy/integrity backup | kernal-api facade (new opt-in capability) | Bosn owns SQL/schema/migration/import policy; facade owns backend dependency and lifecycle primitives |
| Docker/git/ssh/scp launch, process identity, cancellation, groups | kernal-api | Bosn supplies command semantics and lifecycle policy |
| Paths, atomic files, locks, hashing, tree walk | kernal-api | Bosn retains Dockerignore/COPY/context/workspace identity semantics |
| Authenticated local transport, frames, process singleton/autostart | kernal-api | Bosn owns protobuf messages, authorization, daemon/job policy |
| HTTP TLS/download/cache primitives | kernal-api | Bosn owns setup-document scheme/origin/digest/cache-update policy |
| registry, labels, manifests, Compose/guest, GC, adoption, API/MCP | Bosn | never move product policy into kernal-api |

## README discrepancies found

The README status says Compose is only `up/down/logs/ps`, but implementation additionally
accepts `build/run/exec/config` (`src/bosn/frontdoor.py`, `tests/test_frontdoor.py`).  It also
says the manifest has no `env` or `workdir` key, while `src/bosn/manifest.py` parses both and
tests cover them in `tests/test_manifest.py`.  The migration documentation treats source and
tests as authoritative until README is corrected in a separately reviewed documentation pass.

## Phase-0 evidence commands

### Python baseline (2026-09-13)

`uv run pytest -q -m 'not docker'` at the Python baseline completed with **1219
passed, 9 skipped, 37 deselected** in 125.30 seconds. The new portable-fixture test
passes separately. No Python production behavior changes in this milestone.

The broader baseline `uv run pytest -q -m docker` completed with **36 passed,
1 failed** in 1135.75 seconds on this host. The failing test is
`tests/test_compose_e2e_docker.py::test_compose_lifecycle_through_the_real_front_door`:
the Compose `run web echo ...` step returned 1 after reporting that the existing
volume did not match the configuration and attempting to recreate a network with
active endpoints. The current overlay stamps `created` with the current time on
every invocation (`src/bosn/docker_cli.py`); label stability across repeated Compose
verbs needs explicit characterization during the Rust port. This is an observed
Python-baseline failure, not a passed migration gate or a proven root-cause claim.

Run `uv run python ci/migration_baseline.py --docker --samples 7` to repeat the
measurements. The runner creates an isolated state directory, starts/stops its own
daemon, builds a synthetic Alpine stack, and removes only resources whose complete
labels prove ownership by that run's registry UUID. Docker mode mutates only these
synthetic resources. The base Alpine image and BuildKit cache are not pruned.

Observed on Linux 6.18.48 x86-64, glibc 2.42, Python 3.13.15, Docker 29.7.2:

| Measurement | Result |
| --- | --- |
| `python -m bosn --version` startup (7 samples) | median 169.03 ms; range 144.80–269.73 ms |
| CLI status, empty registry/running daemon (7 samples) | median 141.22 ms; range 127.22–149.52 ms |
| Daemon resident memory after status | 30,848 KiB |
| Initial synthetic ensure/build (1 sample) | 2,155.91 ms |
| Ensure reuse (7 samples) | median 708.71 ms; range 523.79–1,322.81 ms |
| `uv build --wheel` (1 sample, warm dependencies) | 388.16 ms |

This is a shared development host with concurrent workloads; caches were not
cleared. Repeat on the same host/workload and include raw samples for final Rust
comparisons. The wheel figure measures packaging, not a clean toolchain/dependency
build. Native target and packaged-artifact comparisons remain Phase 7 work.

### Fixture validation

The fixture was added RED first: `uv run pytest tests/test_migration_fixture.py -q` failed
because the fixture generator did not exist.  GREEN verification and lint commands/results
belong with this change's final review run:

```bash
uv run pytest tests/test_migration_fixture.py -q
uv run ruff check tests/test_migration_fixture.py tests/fixtures/migration/create_python_v4_registry.py
uv run pyright tests/test_migration_fixture.py tests/fixtures/migration/create_python_v4_registry.py
```
# Rust migration

See [Rust generation identity](rust-generation.md) for the bounded collector
and immutable generation API migration status.
