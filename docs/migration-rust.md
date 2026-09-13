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

`bosn` currently has no supported Python product API beyond package version export
(`src/bosn/__init__.py`).  Internal modules are not a compatibility contract.

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
| Manifest kind | `macos-x86-64`, explicit Apple-license acknowledgement, guest sizing/ports/readiness/payload fields; guest data volume can be pinned | `tests/test_guest.py`, `tests/test_manifest.py` |
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
