# bosn

[![CI (Linux)](https://github.com/zackees/bosn/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/zackees/bosn/actions/workflows/ci.yml)
[![Python 3.11+](https://img.shields.io/badge/python-3.11%2B-blue)](https://www.python.org/)
[![License: MIT](https://img.shields.io/badge/license-MIT-green)](LICENSE)

**A machine-wide lifecycle supervisor for container development resources.**

`bosn` (bo's'n — the ship's officer who keeps the deck in order) is a standalone CLI
plus a background daemon that owns the containers, volumes, images, and BuildKit
records your development workflow creates. It knows what each resource belongs to,
which ones are in active use, and when the rest may be deleted — so build caches
stay warm while total storage stays bounded, without anyone remembering to clean up.

> **Status: base functionality implemented and validated.** The manifest, generation
> digests, converge-then-run, the sqlite registry, the daemon singleton, leases, adoption,
> and tiered GC are all in place and covered end to end against a real Docker engine on
> Linux CI. The full v3 design lives in
> [issue #1](https://github.com/zackees/bosn/issues/1) and the bringup plan in
> [issue #3](https://github.com/zackees/bosn/issues/3). Issue #1's text predates the rename
> and refers to the project as *dockhand*; the design is unchanged, the name is not.
>
> Not yet built: the `bosn-docker` compatibility subset, `bosn init` from `compose.yaml`,
> daemon-owned background jobs (`bosn attach`), macOS/Windows CI, and podman.

## Why this exists

Docker is a daily dependency across [clud](https://github.com/zackees/clud),
[soldr](https://github.com/zackees/soldr), [zccache](https://github.com/zackees/zccache),
and agent-created git worktrees — but lifecycle policy was duplicated across a dozen
scripts, and no component could answer *"is this resource still in use?"*

That gap became a host-level failure in the summer of 2026. Every worktree path-hash
spawned its own five-volume Rust cache group (`target`, `CARGO_HOME`, `RUSTUP_HOME`,
cargo-chef, `.soldr`). Retained history showed **268 Docker invocations — and zero
cleanup calls.** Nothing had a TTL, so nothing was ever deleted:

| Date | State |
| --- | --- |
| Jul 25 | 60 volumes, 141 GB |
| Aug 7 | 101 volumes, 273 GB (+ 89 GB images, 38 GB writable layers) |
| Aug 9 | ~312 GB of volumes; host volume under 1 GB free |
| Aug 13 | 452.6 GiB VHDX, 804 KiB free — ext4 journal aborted, read-only remount, engine dead |

Recovery required elevated privileges, a full WSL shutdown, and deleting the Docker data
VHDX — restoring 452.6 GiB by destroying every image, container, and volume on the machine.
A fresh image was rebuilt within three hours of recovery. **Manual GC is not a lifecycle:
a dry-run command nobody invokes cannot bound growth.**

## The model

Six ideas carry the design:

- **Ownership is machine-readable.** Every managed resource carries a complete label
  contract including a `registry` UUID. A daemon collects only resources bearing *its own*
  registry id. Foreign resources are counted and reported, never deleted; unlabeled
  resources are invisible to every decision. Name prefixes are never used as ownership proof.
- **Active work holds leases, not age guesses.** A lease records the client PID, process
  start-time, and a heartbeat. Leased resources are untouchable. A lease expires only when
  its TTL elapses *and* a liveness probe fails — so a killed client releases in one TTL
  while a live 40-minute build is never collected out from under itself.
- **Identity is a content digest.** A *generation* is the hash over a stack's manifest
  section plus the byte content of every file it references. Two invocations are compatible
  iff their digests are byte-equal. Edit the Dockerfile and a new generation rolls forward;
  the old one keeps serving its live leases, then ages out as *superseded*.
- **Retention is tiered, because containers are not volumes.** A container is disposable
  (seconds to recreate from a warm image); a cache volume is the asset that turns a
  20-minute cold build into 30 seconds. They get separate clocks: containers idle-stop at
  1 h and are removed at 24 h; warm volumes live 72 h; superseded generations are capped at
  24 h; machine-shared caches age only under pressure.
- **Enforcement is layered.** Docker has no native TTL, so containers police themselves
  first — ephemeral runs are `--rm` with a wall-clock cap, and the persistent container's
  PID 1 is an idle watchdog. The daemon is the authority and the backstop, never the only
  mechanism. Cleanup keeps working with the daemon retired, asleep, or dead.
- **Commands converge.** Every stack verb makes registered state match the manifest —
  registering, rolling a generation, or reusing as-is — then runs. The same command is
  correct on the 1st and the 500th invocation. Errors are reserved for states that need a
  human decision; an error whose remedy is always the same mechanical command is just a
  forced retry loop.

Machine-scoped caches kill the incident's dominant multiplier: the cargo registry, rustup
toolchains, and soldr state become **one volume per machine** shared across every repo and
worktree, dropping the soldr stack from five per-worktree volumes to two.

## What it looks like

A checked-in `bosn.toml` is the spec sheet, the discovery surface, and the digest root:

```toml
[stack.test]
dockerfile = "docker/test.Dockerfile"     # referenced files fold into the digest
family = "rust"                           # shares machine-scoped caches with other "rust" stacks
default = true

[stack.test.volumes]
target    = { scope = "spec" }            # invalidated when the digest changes
chef      = { scope = "stack" }           # survives spec edits in this workspace
cargo-reg = { scope = "machine" }         # one per machine, shared everywhere
rustup    = { scope = "machine" }

[task.unit]
stack = "test"
cmd = "bash test"
```

The steady-state contract is one word:

```bash
bosn unit                    # run a manifest task (converges first, silently)
bosn run -- cargo test       # ad-hoc command in the default stack
bosn shell                   # interactive session in the persistent container
```

Everything else is lifecycle and introspection:

```bash
bosn tasks --json            # discovery: tasks, stacks, digests, registration state
bosn status                  # tiers, leases, managed bytes vs ceiling, foreign registries
bosn jobs / bosn attach <j>  # daemon-owned builds that survive a killed CLI
bosn cancel <j>              # stop a build you no longer want
bosn done                    # this workspace is finished; its caches become collectable
bosn gc --dry-run            # what would be reclaimed right now
bosn doctor                  # engine health, backing-volume free space, VHDX slack
```

## Build jobs and the concurrency policy

Cold builds belong to the daemon, not to the CLI that asked for one. `bosn run` submits a
converge, attaches, streams the build to stderr, and exits with the job's status — but if
that CLI is killed, the build keeps going. Re-run the same command and you reattach to it;
`bosn jobs` lists what is in flight and `bosn attach <id>` reconnects explicitly. This is
the whole point: a 20-minute build must not die because an agent's timeout fired.

Which creates the problem the policy exists to solve. The daemon now owns work that
outlives its requester, and bosn's primary consumer is an agent in an edit-and-rerun loop.
Each edit is a new digest against the same stack, so those requests neither join (not
identical) nor parallelize (same key). Left to serialize, they queue without bound — every
entry but the last obsolete on arrival, each pinning volumes against GC, and running jobs
block idle retirement, so the loop keeps the daemon resident and its resources
uncollectable at the same time.

**The rule: per `(workspace-id, stack)`, at most one running build and one pending
request.**

| Request arrives while... | What happens |
| --- | --- |
| nothing is in flight | it builds |
| the same digest is building | it **joins** — one build, many watchers |
| a different digest is building | it takes the **pending slot** |
| the pending slot holds the same digest | it joins that |
| the pending slot holds a different digest | it replaces the occupant, which ends as **superseded** |

The bound is structural rather than tuned, and the worst case is waiting out one obsolete
build — which warms BuildKit's layer cache for the one that follows. The alternative of
cancelling in-flight builds so the newest always wins is more responsive but can livelock:
under a fast edit loop every build is killed before finishing and the agent never gets
output. Rejecting the second request instead ("a build is already running") turns a queue
problem into a retry loop, which this project treats as a bug, not an error message.

The consequence worth knowing: superseding only ever drops a build that has **not
started**, so cancellation never happens behind your back. Stopping a running build is
always deliberate — `bosn cancel`, daemon shutdown, or the per-job TTL.

Other guarantees:

- **Distinct keys build in parallel.** Two worktrees never block each other. `workspace-id`
  is the resolved manifest root, never the cwd, so two agents in different subdirectories
  of one worktree correctly share a key.
- **Total concurrency is capped** at `max(2, cpus/2)` builds machine-wide (`BOSN_MAX_BUILDS`).
  Per-key serialization alone does not bound host load; N worktrees still can.
- **A cancelled build leaves nothing behind.** Registration happens only after `docker
  build` exits 0, so a killed build cannot leave a generation row implying a usable image —
  and cannot mark the previous, working generation superseded.
- **Nothing fails silently.** A superseded, dropped, or cancelled request exits non-zero
  with a message naming what happened: `4` for superseded, `5` for cancelled, `1` for a
  build that failed.
- **A hung build cannot pin the daemon.** Running jobs block idle retirement, so a per-job
  TTL (default 1 h, `BOSN_BUILD_TTL_SECONDS`) reaps anything that stops reporting.

There is deliberately no `--detach`. Blocking-with-attach is the only mode, because
backgrounding is the easiest way to recreate the pile-up above and fan-out is already
available by running from multiple worktrees.

## Existing Docker and Compose workloads

Most real projects already have a `compose.yaml` and scripts that shell out to `docker`.
Those keep working: bosn ships a second binary, **`bosn-docker`**, that is a drop-in
replacement for the `docker` and `docker compose` CLIs, with optional `docker` /
`docker-compose` shims so unmodified scripts need no edits at all. Everything it creates
is labeled, registered, leased, and collected like any other managed resource — the
Docker interface is a front door, not a bypass.

It implements a subset and grows as real workloads demand. Anything outside that subset
**fails with a specific error naming the flag or key it cannot honor**, never by silently
ignoring the option and never by quietly falling back to raw Docker — either of those
would reintroduce exactly the unlabeled resources bosn exists to eliminate.

The two surfaces are not peers. `bosn` is the better-managed environment and the
recommended way to use the product: manifest-declared stacks, digest-keyed generations,
converge-on-run, one-word tasks, and machine-shared caches. `bosn-docker` is the on-ramp
for what you already have, and `bosn init` can read an existing `compose.yaml` to generate
a starting `bosn.toml` when you want the stronger guarantees.

## Design commitments

- **Never `docker system prune`.** Never prune the default builder. Never delete a foreign
  or incompletely labeled resource. Automatic deletion requires complete ownership proof,
  which is also why there is no `gc --force`.
- **Fail closed, stay visible.** If the daemon is unreachable, mutating commands fail with a
  diagnosis rather than falling back to raw Docker — a fallback would recreate exactly the
  unregistered resources this project exists to eliminate. Read-only verbs always work: they
  open the sqlite registry directly (WAL readers never block), so a wedged daemon stops
  mutation without blinding you.
- **Dirty work is never destroyed on inference.** Derived done-signals (worktree missing,
  branch gone, merged PR) apply only when the worktree is absent, or clean with nothing
  unpushed. The strongest signal is first-party: clud tells bosn at worktree teardown.
- **Diagnose, don't grind.** When free-space pressure exceeds what GC can actually reclaim,
  bosn stops evicting and raises a compaction advisory instead of destroying warm caches
  against a signal it cannot move. Destructive VM-disk recovery stays manual and
  confirmation-gated.
- **Every failure is observable.** Cleanup errors land in the event log and in `status`
  counters — a discarded prune error is how you come to believe storage is bounded when it
  isn't.

## Architecture

A `bosn` CLI and a `bosn __daemon` singleton, following the soldr/zccache pattern: its own
repository, its own release cadence, its own failure domain. State is **sqlite in WAL mode** —
chosen over redb specifically for failure shape, since WAL readers never block behind a hung
writer. The daemon is the registry's only writer and the only executor of reap and GC; it is
lazily spawned, autostarted at login, and idle-retired with a scheduled catch-up tick so
maintenance still runs unattended.

Losing the database is survivable: **ownership lives in the labels, and the registry is
authoritative only for time and leases.** A lost registry rebuilds by rescanning Docker
labels, with adoption time as last-use and a 24 h quiet period so recovery is never followed
by a mass age-out.

**v1 platforms:** cmd.exe, PowerShell, and MSYS Git Bash on Windows; native macOS and Linux.
WSL is deferred to v2 — on Windows 10, `127.0.0.1` inside WSL does not reach Windows
loopback, so the v1 CLI detects WSL and fails closed with that explanation rather than
half-working.

Docker is the v1 engine. Nothing in the model is Docker-specific, and podman is the intended
second target — the name is a deliberate step away from `dock*`. Engine access goes through
the `docker` / `podman` CLI rather than a Docker-specific API binding, which is most of what
makes that second target cheap.

**bosn is a pure-Python application** — it follows soldr's and zccache's *shape* (own repo,
own daemon, own release cadence), not their implementation language. It ships as a single
`py3-none-any` wheel with no compiled artifact and no per-platform build matrix, which is the
whole reason for the choice: clud invokes bosn on every platform clud itself supports, and a
compiled bosn would owe each of those a cross-compiled binary. Install it into its own
isolated environment (`uv tool install bosn`) so a broken project venv can never take the
disk-space supervisor offline with it.

## Integration

clud ships a thin `clud bosn …` forwarder that passes argv verbatim to the executable, exactly
as it shells out to soldr, pinned to a bosn release. The second seam is the worktree-teardown
hook that calls `bosn done`. Both degrade safely when bosn is absent: teardown proceeds, and
the derived signals catch up later.

## Development

Three bash trampolines are the whole interface. `./lint` and `./test` hold no logic of
their own — they exec into `ci/lint.py` and `ci/test.py`, which do the real work under `uv`:

```bash
./install     # install uv if absent, then `uv sync --all-groups`
./lint        # ruff format --check, ruff check, pyright, KeyboardInterrupt checker
./test        # pytest; passthrough args work, e.g. ./test -k registry
```

Linting includes a dedicated **KeyboardInterrupt checker** (`ci/lint_kbi.py`). Ctrl-C and
Python do not mix well: a broad `except Exception` silently swallows the user's interrupt.
Ruff's BLE001 can flag the blind except, but the actual defect is the *missing*
`except KeyboardInterrupt` beside it, so an AST checker enforces that instead:

| Code | Rule |
| --- | --- |
| `KBI001` | `except Exception` / `BaseException` with no sibling `except KeyboardInterrupt` |
| `KBI002` | a `KeyboardInterrupt` handler that neither re-raises nor calls `_thread.interrupt_main()` |
| `KBI003` | a bare `except:` that never re-raises |

Suppress a line with `# noqa: KBI001` when a case is genuinely intentional.

CI runs on Linux only. Docker-backed tests carry a `docker` pytest marker: they skip when no
engine is reachable, and `ci/test.py` excludes them outright on non-Linux CI runners, so the
unit suite stands alone without an engine.

## License

MIT
