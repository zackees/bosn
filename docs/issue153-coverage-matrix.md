# Issue #153 migration coverage matrix

This is the Phase 0 inventory for [issue #153](https://github.com/zackees/bosn/issues/153). It records what is implemented in this repository, the evidence that exercises it, and the work that must not be mistaken for release completion. It is not a claim that every Phase 0 or migration exit criterion has passed.

## Boundary and ownership

| Concern | Implemented owner | Evidence / boundary |
| --- | --- | --- |
| Product rules: manifests, setup documents, identity/generation, ownership, retention, jobs, Docker argument shapes, and protocol payloads | Bosn Rust crates | `bosn-core`, `bosn-generation`, `bosn-setup`, `bosn-registry`, and `bosn-service`; [kernel-boundary verifier](../ci/verify_kernel_boundary.py) rejects direct systems backends. |
| OS effects: filesystem/private paths and locks, hashing, HTTPS, authenticated local IPC, random, SQLite, process execution and host checks | `kernal-api` | Every Bosn systems dependency is the reviewed upstream `kernal-api` revision `fc634e507024d63ccaaf75fa564818b9dcfbff36`; see the Cargo manifests and [kernel-boundary verifier](../ci/verify_kernel_boundary.py). Bosn supplies its schema and policy, not an alternate OS facade. |
| Durable state | Bosn schema over kernel SQLite/locks | Eight-table v5 registry: `meta`, `resources`, `resource_uses`, `leases`, `execution_sessions`, `volume_creation_intents`, `generations`, and `events`; [registry documentation](rust-registry.md) and `crates/bosn-registry/tests/foundation.rs`. |
| Front ends | One authenticated Bosn daemon | Native CLI, the thin PyO3 `bosn` package, and stdio MCP all submit typed requests to the same daemon; [migration boundary](migration-rust.md#native-operation-boundary). |

The normal development dependency is intentionally a Git revision, not `_vender/kernal-api`; a checkout there is for source inspection/bootstrap only. `cargo search kernal-api` on 2026-09-14 reports only `kernal-api = "0.0.0"`. Therefore a release must first publish the required real `0.1.0` and switch all manifests/lockfiles to exactly `kernal-api = "=0.1.0"`; [release-dependency checks](../ci/verify_release_dependencies.py) deliberately fail until then.

## Implemented Bosn surface and behavior fixtures

| Surface / verbs | Current implementation | Focused evidence |
| --- | --- | --- |
| Daemon and diagnostics | `daemon serve/status/stop`, bounded `doctor`, authenticated local IPC, bounded frames, one registry writer | `crates/bosn-service/tests/daemon_cli.rs`, `crates/bosn-service/src/lib.rs`, and `crates/bosn-service/src/jobs.rs` unit tests |
| State and safe cutover | SQLite v5 writer/read-only behavior, all eight v4 tables, guarded offline `registry import-v4`, gated `registry reconcile-v4 preview/apply` | `crates/bosn-registry/tests/foundation.rs`, `tests/test_migration_fixture.py`, [Python-v4 cutover](python-v4-cutover.md) |
| Ownership and resource lifecycle | Typed labels, shared consumers, PID/start-time leases, execution sessions, creation intents, generations, conservative GC preview/apply, adoption, retired-stop, setup reconciliation, manifest volume GC/release | `crates/bosn-core/tests/domain.rs`, `crates/bosn-registry/tests/foundation.rs`, `crates/bosn-service/tests/setup_*.rs` |
| Setup URL / one-file Docker Linux app | `setup plan/prepare/ensure/task/app-task/done`; local path or bounded verified HTTPS config; cached refresh/offline policy; pinned image or inline Dockerfile plus companion files, mounts/environment/workdir/tasks | `crates/bosn-setup/src/lib.rs`, `crates/bosn-setup/src/plan.rs`, `crates/bosn-setup/src/prepare.rs`, `crates/bosn-setup/src/ensure.rs`, `crates/bosn-service/tests/setup_remote_https.rs` |
| Manifest runtime | `manifest ensure/converge/app-task`; typed images/Dockerfile, named volumes, tmpfs, workspace mounts/workdir, constrained macOS guest | [manifest runtime matrix](rust-manifest-runtime.md), `crates/bosn-setup/src/ensure.rs`, and `crates/bosn-service/tests/setup_ensure_docker.rs` |
| Compose | Pure `compose plan` and lossless one-service Compose-to-setup translation for explicit YAML locators; not a generic Compose executor | [Compose boundary](rust-compose.md), `crates/bosn-core/tests/compose.rs` |
| Python API and packaging | `import bosn`, thin `Client`, local pure planning, packaged version-matched native executable/extension; no Python lifecycle writer | `crates/bosn-python/src/lib.rs`, `tests/test_python_native_boundary.py`, `tests/test_native.py`, `tests/test_native_wheel.py`, [migration boundary](migration-rust.md#python-lifecycle-retirement) |
| MCP / Hermes | `bosn mcp` stdio JSON-RPC/MCP tools for diagnostics, jobs, setup, manifest operations and explicit GC/adoption/reconciliation/volume operations | `crates/bosn-service/src/mcp.rs`, `crates/bosn-service/tests/mcp_*.rs`, `crates/bosn-service/tests/hermes_mcp.rs`, [MCP contract](rust-registry.md#native-mcp-stdio-surface) |

The setup document remains a closed product schema: callers provide a workspace and one local/HTTPS document locator, not arbitrary Docker CLI flags or additional user-authored build files. The exact accepted and refused configuration forms are documented in [the setup/manifest matrix](rust-manifest-runtime.md) and [the Compose boundary](rust-compose.md).

## Explicit removals and refusals

Backward compatibility is intentionally absent. The retired Python lifecycle modules and legacy Python CLI/API/daemon protocol are not packaged; the small remaining Python package is only a native binding and package-local launcher. The authoritative retired-module list and wheel assertion are in [migration-rust.md](migration-rust.md#python-lifecycle-retirement) and `ci/verify_installed_wheel.py`.

Bosn also deliberately refuses, rather than approximates:

- generic Docker arguments, arbitrary shell/run operations, unpinned images, and unsupported setup/manifest/Compose shapes;
- generic Compose application or multi-service lifecycle execution (the parser/translator is pure and narrow);
- a Dockerfile selected symlink until `kernal-api` exposes a safe private-root symlink-creation facade;
- unsafe guest hosts, unpinned/unknown macOS guest images, or arbitrary privileged guest shapes; and
- daemon/Python/MCP access to offline v4 import/reconciliation, which must run while the destination daemon is stopped.

## Consumers and release inventory

| Consumer | Status in this repository | Required remaining evidence |
| --- | --- | --- |
| Bosn CLI, Python package, and MCP | Implemented as three front ends over the Rust daemon; wheel verifier checks an installed artifact outside the checkout | Re-run installed artifacts on every supported release target. |
| Bosn legacy Python lifecycle | Removed; no compatibility shim | Real v4 state cutover/reconciliation against a protected Docker fixture remains required before release. |
| `zackees/Soldr` Bosn workflow | External repository; not modified or verified here | AI-assisted migration plus its focused tests in Soldr. |
| `zackees/clud` scripts/skills | External repository; not modified or verified here | Inventory callers, migrate them, and run focused tests there. |
| `zackees/kernal-api` Bosn manifest/CI and publishing | External repository; no release change made here | Publish the required crate, change Bosn to the exact published dependency, and verify the downstream manifest/CI. |

## Bounded gaps and proof gates

| Gap / gate | Current status | What would close it |
| --- | --- | --- |
| Exact published kernel dependency | Blocked externally: crates.io has the `0.0.0` reservation, not `0.1.0` | Publish the reviewed kernel capability set, pin `=0.1.0`, regenerate the lockfile, and pass release-dependency verification. |
| Dockerfile symlink context | Safely refused | A public, portable `kernal-api` private-root symlink-create facade with non-following destination/parent checks, then Bosn tests. |
| Live Docker evidence | Opt-in/ignored, not ordinary CI | Run the documented ignored Docker tests against a pinned image and preserve their scoped cleanup proof. |
| Hermes evidence | Opt-in/ignored, pinned to Hermes Agent 0.21.0 | Run `hermes_mcp` with `BOSN_HERMES_ACCEPTANCE=1`; it requires the installed pinned client. |
| Live KVM macOS guest | No live KVM guest acceptance proof | Run a separately provisioned Linux host with `/dev/kvm` and `/dev/net/tun`, licensed guest bootstrap, and the documented guest lifecycle/task checks. |
| Consumer migration | Downstream work remains external | Close the Soldr, clud, and kernal-api inventory rows above with reviewed changes and focused tests. |
| Platform release proof | Native-wheel lanes are defined for Linux, macOS, and Windows | Require successful current GitHub Actions wheel/install smoke evidence before release; the Linux Docker lane does not prove macOS/Windows Docker or guest behavior. |

## Baseline record

[`performance-baseline.json`](performance-baseline.json) is the historical Phase 0 comparison record. Its source commit reported medians/ranges, not raw samples; its `raw_samples_ms` values are intentionally `null`. The native collector and its non-Docker/Docker opt-in modes are documented in [migration-rust.md](migration-rust.md#performance-comparison-baseline). Build/package cost still needs a comparable recorded measurement, and no performance regression conclusion follows from the historical record alone.
