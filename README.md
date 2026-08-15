# bosn — let your agents use Docker all day without filling the disk

[![CI](https://github.com/zackees/bosn/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/zackees/bosn/actions/workflows/ci.yml)
[![Python 3.11+](https://img.shields.io/badge/python-3.11%2B-blue)](https://www.python.org/)
[![License: MIT](https://img.shields.io/badge/license-MIT-green)](LICENSE)

An AI agent working a long-horizon task runs the same containerized build hundreds of
times. Docker keeps every image, volume, and container it is told to make, and has no
notion of "this one is finished." So the agent's disk usage only goes up — until the day
the engine dies with the machine full.

**`bosn` is the thing that knows when a container resource is still in use and when it may
be deleted, so a build cache stays warm across hundreds of runs while total storage stays
bounded — with nobody remembering to clean up.**

It is a CLI plus a background daemon that owns the containers, volumes, and images your
workflow creates. Every resource it makes is labeled with who owns it, what content it was
built from, and which workspace it belongs to; whether anything is using it right now is a
lease in bosn's own database. Storage is bounded by three ceilings — 1,000 resources,
100 GiB managed, 10 GiB free — and eviction re-checks after every delete, so crossing the
byte ceiling trims back under it instead of clearing the cache.

Also useful if you are not an agent: CI runners with persistent workers hit the same
rebuild-forever shape, monorepo teams get the machine-scoped cache sharing described below,
and anyone who has ever run `docker system prune` and destroyed a cache they wanted gets a
policy instead of a blunt instrument.

> ### Status
>
> **Working:** the manifest, generation digests, converge-then-run, the sqlite registry, the
> daemon singleton, leases, adoption (including `--legacy` import), tiered GC, and
> daemon-owned build jobs with `bosn attach`. All covered end to end against a real Docker
> engine on Linux CI.
>
> **Partial:** the Docker front door — `bosn init` (alias: `bosn-docker init`) and `bosn-docker compose
> {up,down,logs,ps}` only, over a small compose subset. See
> [Coming from Docker](#coming-from-docker).
>
> **Not built:** podman; `docker` / `docker-compose` shims; and **BuildKit's own layer
> cache**, which is outside the label contract — bosn manages containers, volumes, and
> images, and never prunes the builder.
>
> **⚠ WSL is refused outright.** `bosn` exits non-zero inside WSL — its Windows loopback
> daemon is unreachable from there. Use a native Windows shell, macOS, or Linux. (The check
> lives in `bosn`; `bosn-docker` does not yet refuse it. The incident below happened *to* a
> WSL VHDX; bosn runs on the Windows side of that boundary, not inside it.)
>
> Full v3 design: [issue #1](https://github.com/zackees/bosn/issues/1); bringup plan:
> [issue #3](https://github.com/zackees/bosn/issues/3). Issue #1 predates the rename and
> calls the project *dockhand*; the design is unchanged.

---

## The failure this prevents

This is not hypothetical. It happened, on one developer machine, in the summer of 2026.

Every agent-created git worktree spawned its own five-volume Rust cache group (`target`,
`CARGO_HOME`, `RUSTUP_HOME`, cargo-chef, `.soldr`). Retained history showed **268 Docker
invocations and zero cleanup calls.** Nothing had a TTL, so nothing was ever deleted:

| Date | State |
| --- | --- |
| Jul 25 | 60 volumes, 141 GB |
| Aug 7 | 101 volumes, 273 GB (+ 89 GB images, 38 GB writable layers) |
| Aug 9 | ~312 GB of volumes; host volume under 1 GB free |
| Aug 13 | 452.6 GiB VHDX, 804 KiB free — ext4 journal aborted, read-only remount, **engine dead** |

Recovery took elevated privileges, a full WSL shutdown, and deleting the Docker data VHDX:
452.6 GiB restored by destroying every image, container, and volume on the machine.

The lesson is narrow and specific: **a dry-run cleanup command that nobody invokes is not a
lifecycle.** `docker system prune` is not a policy, because something has to decide *when*
to run it and *what is still needed* — and neither an agent nor a busy human ever does.

---

## First run

```bash
uv tool install git+https://github.com/zackees/bosn    # puts `bosn` and `bosn-docker` on PATH
```

Write a `bosn.toml` at your repo root:

```toml
[stack.test]
dockerfile = "docker/test.Dockerfile"   # this file, and everything it COPYs, form the digest
family = "rust"                         # the machine-cache sharing key — see below
default = true

[stack.test.volumes]
target    = { scope = "stack" }         # per (workspace, stack) — survives digest changes
cargo-reg = { scope = "machine" }       # per (family, volume) — shared across every repo
rustup    = { scope = "machine" }

[task.unit]
stack = "test"
cmd = "sh test"                         # run as `sh -c`; the image needs whatever you name
```

Then:

```bash
bosn ensure                    # converge and register without running anything (pre-warm)
bosn unit                      # run the task
bosn status                    # what exists, what holds it, bytes vs ceiling
```

Nothing needs starting — the first command lazily spawns the daemon.

**For unattended cleanup, install the login launcher once:**

```bash
bosn __daemon --autostart      # systemd user timer / launchd plist / Windows Startup entry
```

This matters more than it looks. GC runs inside the daemon every 5 minutes, but the daemon
idle-retires after 15 minutes and is only respawned by the next command. Without the login
launcher, cleanup happens only while you are actively using bosn. (On macOS the plist takes
effect at next login; only Linux enables the timer immediately. On Windows the launcher is a
Startup `.cmd` loop, so it leaves a console window open.)

### The execution model — build time vs run time

This is the part most likely to surprise you, and it decides how you write the Dockerfile.

**Mounts exist only when a command runs. `docker build` never sees them.**

| Declared as | Lands at | bosn's relationship to it |
| --- | --- | --- |
| `[stack.X.volumes]` | `/bosn/<name>`, or an explicit `destination` | owns it — labels, tracks, and may delete it |
| `[stack.X.mounts]` | the `destination` you give | only references it — never labeled, never deleted |
| *(automatic)* | `/bosn-daemon/heartbeat`, read-only | the daemon's liveness file |

That distinction is the whole reason binds are a separate table: bosn deletes Docker objects
it owns, and a host path is not one.

**Bind-mount your source tree if you want an edit-in-place loop:**

```toml
[stack.test.mounts]
repo = { source = ".", destination = "/repo" }
conf = { source = "./config", destination = "/etc/app", readonly = true }
```

`source` is relative to the manifest root and must exist — a missing source is an error, not
an empty directory the engine creates for you. A destination inside `/bosn/*`, a relative
destination, or two mounts sharing one destination are all refused at parse time.

**The declaration is digested; a bind source's contents are not.** Moving a mount rolls the
generation. Editing a file that is only visible through a bind does not — that is exactly
what a bind is for, and hashing a live working tree would rebuild on every keystroke. Files
your Dockerfile `COPY`s are still content-digested as usual, so you choose per path whether
something participates in identity.

**Anything you want cached during the *build* needs a BuildKit cache mount**
(`RUN --mount=type=cache,...`), not a bosn volume — mounts are run-time only. bosn does not
manage the builder cache (see Status), so it is outside bosn's accounting, and also the one
thing bosn will never delete out from under you.

**Declaring a volume wires it to nothing.** The manifest has no `env` or `workdir` key, so
either point the toolchain at the mount from the Dockerfile:

```dockerfile
ENV CARGO_HOME=/bosn/cargo-reg \
    RUSTUP_HOME=/bosn/rustup \
    CARGO_TARGET_DIR=/bosn/target
```

or give the volume the destination the image already expects, which is the easier path when
adopting an existing image:

```toml
[stack.test.volumes]
cargo-reg = { scope = "machine", destination = "/root/.cargo" }
```

The image also needs a POSIX shell with `date`, `stat`, and `sleep`: the persistent
container's PID 1 is a shell loop, and `bosn shell` execs `sh`. Distroless and scratch
images will not work.

---

## Coming from Docker

### What maps to what

| You are used to | bosn | What changes |
| --- | --- | --- |
| `docker build` + `docker run` | `bosn run -- <cmd>` | rebuilds only if the *content digest* changed |
| `docker compose up` | `bosn-docker compose up` | service **containers** get labeled and tracked; volumes do not (see below) |
| `docker system prune` | *(nothing — automatic)* | never needed, and bosn never runs it |
| `docker volume ls` + guesswork | `bosn status` | tiers, leases, managed bytes vs ceiling |
| "is this cache still needed?" | `bosn gc` | dry-run by default; shows exactly what would be reclaimed |
| deleting a worktree and hoping | `bosn done` | marks it finished; its caches become collectable |

### If you already have a compose.yaml

Set expectations before you run anything: the front door is a narrow subset, not a
compatibility layer.

`init` is a line parser, not a YAML parser. It wants a top-level `services:`, service names
at two spaces, and an `image:` key. **A `ports:`, `environment:`, `volumes:`, or
`depends_on:` key is a hard error** — which is most real compose files. Treat it as a
skeleton generator for image-only services, not a migration tool.

```bash
bosn init --compose compose.yaml          # also: --output; alias: bosn-docker init
bosn-docker compose -f other.yaml up      # also: down, logs, ps
```

Flags belong *before* the verb. Anything after it is rejected outright rather than silently
ignored: `bosn-docker compose up -d` fails with `unsupported compose flag or argument '-d'`.
There is therefore no detached mode — `compose up` runs in the foreground.
That refusal is deliberate — silently dropping a flag, or quietly falling back to raw
Docker, would recreate the unlabeled resources bosn exists to eliminate.

**Compose volumes and networks are not yet labeled.** The overlay labels service containers
only, so volumes declared in your compose file stay outside bosn's accounting entirely —
counted as unlabeled, never given a TTL. Compose volumes are exactly the class of object the
incident was made of, so declare caches in `bosn.toml` if you want them bounded.

### If you already have a pile of unmanaged volumes

That is the 101-volume situation from the incident table. `adopt --legacy` brings qualifying
volumes under bosn's label contract:

```bash
bosn adopt --legacy soldr          # plan and report (exits non-zero); also: clud, zccache
bosn adopt --legacy soldr --yes    # apply
```

**This buys accounting, not warmth.** bosn generates its own volume names and stamps every
adopted volume with a sentinel generation that the first ordinary converge supersedes — so
your builds will not reuse them. What changes is that volumes which were invisible junk
become owned, counted, and collected on a clock, instead of sitting on the disk forever.

Eligibility is **label-gated, never name-gated**: a volume qualifies only if it carries its
producer's own `managed=true` label. Unlabeled pre-bosn volumes — much of that 101-volume
population — cannot be adopted and have to be removed by hand. Only volumes are adopted;
matching containers and images are reported and skipped. (`zccache` is implemented from the
design contract; no producer emitting those labels was found.)

Docker labels are immutable, so this is a **staged copy, not a rename**: bosn copies the
volume into scratch storage via `alpine:3.20`, recreates it under the new labels, and copies
it back. Budget roughly **double the volume's size in free space**, a reachable engine, and
stopped containers. If the disk is already full, free space first.

Adopted resources get a 24-hour quiet period before normal aging resumes.

---

## How the disk savings actually work

Five mechanisms, in descending order of how much they save.

### 1. Sharing: one cache per machine instead of one per worktree

The incident's dominant multiplier was per-worktree duplication. A cargo registry cache or a
rustup toolchain is byte-identical across every checkout — but Docker gives you one volume
per name, so ten worktrees meant ten copies.

Every volume declares a **scope**, which decides what it is keyed on:

| Scope | One per | Use for |
| --- | --- | --- |
| `spec` | (workspace, stack, **digest**) | output that *must* be discarded on a spec edit |
| `stack` | (workspace, stack) | anything you want warm across edits |
| `machine` | (**`family`**, volume name) | content-addressed, identical everywhere |

**Pick `spec` carefully.** A new digest means a new, *empty* volume — so a `spec`-scoped
`target/` is a cold build after every source edit, which is the opposite of what you want
from a cache. Use `stack` for anything whose value is being warm.

That drops the soldr stack from **five per-worktree volumes to two**. With ten worktrees,
50 volumes become 23 — and the shared ones are the big ones.

**`family` is the sharing key, and it is easy to get wrong.** A `machine` volume is named
from the stack's `family`, falling back to the stack *name* when no family is set. So two
repos both declaring `family = "rust"` share one cargo registry; two repos that omit
`family` and happen to both name their stack `test` will *also* silently share; and two
repos with differently-named stacks and no family will silently not share. Set `family`
deliberately.

### 2. Expiry: things actually get deleted, on separate clocks

Docker has no TTL. bosn gives every resource one, tiered by **what it costs to recreate**:

| Resource | Lifetime | Why |
| --- | --- | --- |
| Container | idle-stop 1 h, removed 24 h | disposable — seconds to recreate from a warm image |
| Warm volume | 72 h | this is the asset: a 20-minute cold build becomes 30 seconds |
| Superseded generation | capped 24 h | the old image after you edited the Dockerfile |
| Machine-shared cache | under pressure, and only once past the warm TTL | most expensive to refill, most widely reused |

Containers are cheap and die fast; caches are expensive and live long. A single global TTL
would either delete your caches or keep your junk.

### 3. Pressure: hard ceilings, and eviction that stops when satisfied

TTLs bound age, not total size. Three ceilings bound size — **1,000 resources**, **100 GiB
managed bytes**, or **under 10 GiB free**. Any one puts the machine under pressure, and
eviction then proceeds superseded → done → idle → pressure, taking non-machine resources
before machine-scoped ones. (Only the byte ceiling is configurable, as
`shared_cache_ceiling`; the other two are constants.)

The part that makes this safe to leave on: **after every delete the collector re-measures
and re-assesses.** Once the machine drops back under the resource-count or byte ceiling, the
remaining candidates flip back to "keep" and survive — crossing 100 GiB trims you back under
it rather than clearing your cache. It also refuses to declare byte pressure resolved while
any resource is still unmeasured, so it cannot talk itself into stopping early.

Two honest limits. Free space is probed once per pass and not re-read between deletes, so a
pass triggered by the **10 GiB free-space** ceiling cannot de-escalate and will evict every
non-machine resource it is allowed to. And machine-scoped caches under pressure are still
subject to the warm TTL — if the excess is young machine-scoped cache, GC will clear
everything else and remain over the ceiling.

If the backing store's slack were larger than everything GC could reclaim, evicting warm
caches would be pure damage, so the design calls for a compaction advisory instead of
grinding. **That measurement is not implemented:** Docker Desktop exposes no reliable
guest-used/host-allocated pair through its CLI, so slack is reported as unknown and the
advisory is currently unreachable. Reclaiming VHDX slack is a manual, confirmation-gated
operation today.

### 4. Supersession: edits do not accumulate

A **generation** is a content digest, and it is computed from more than the Dockerfile's
bytes: bosn parses the Dockerfile's logical lines — continuations, heredocs, `# escape=`,
JSON-form `COPY` — resolves every `COPY`/`ADD` source and `RUN --mount=type=bind` closure,
applies `.dockerignore`, and hashes that whole file set. It then folds in the resolved
engine IDs of every external base image, so a moved `:latest` tag rolls a new generation.

Two consequences worth knowing up front:

- Editing a source file the Dockerfile never copies does **not** rebuild. That is the point.
- Non-deterministic inputs are refused: a git `ADD` needs a full commit SHA and a remote
  `ADD` needs `--checksum`, or the command fails. A digest that cannot be reproduced is not
  an identity.

Edit the Dockerfile and a new generation rolls forward; the old one keeps serving its live
leases, then ages out as *superseded* under the 24-hour cap. Without this, an agent's
edit-and-rerun loop leaves one image per edit, forever.

### 5. Leases: never delete something in use

A lease records the client PID and its process start-time, with a 15-minute TTL. It expires
only when the TTL elapses **and** a liveness probe says the holder is gone — and the probe
compares PID *and* process start time, so a recycled PID cannot keep a dead client's lease
alive. It reads `/proc/<pid>/stat` on Linux, `Get-CimInstance Win32_Process` on Windows, and
`ps -o lstart=` under a pinned `LC_ALL=C` on macOS. If the probe cannot answer, it fails
open and the resource survives.

This is what makes aggressive expiry safe. Without leases the only safe TTL is one longer
than the longest build you might ever run, which is no TTL at all.

Before every delete, ownership is re-proven **from the engine's own labels**, not from the
registry — the registry is a hint, the labels are the authority. Anything that fails that
check is skipped and logged. Three buckets exist and only one is ever touched:

| Bucket | Meaning | Treatment |
| --- | --- | --- |
| owned | complete label contract, **our** registry id | eligible for collection |
| foreign | complete contract, someone else's registry id | counted and reported, never deleted |
| unlabeled | incomplete or absent labels | invisible to every decision |

Name prefixes are never ownership proof. This is also why there is no `gc --force`.

---

## Why an agent needs this specifically

A human runs a build, looks at it, and moves on. An agent runs the same build after every
edit, for hours, unattended.

**A build must outlive the process that asked for it.** `bosn run` submits the build to the
daemon and streams *build* output to stderr as it happens. If the agent's timeout fires and
kills the CLI, the build keeps going — re-run the same command and you reattach. A 20-minute
build must not die because a harness gave up at 10.

**Failures are machine-readable.** Every verb takes `--json` and returns a stable envelope on
error — `{"ok": false, "code": ..., "message": ..., "next": ...}` — where `next` names the
remedy. `tasks`, `gc`, and `adopt` return specific codes (`manifest.invalid`,
`registry.unreadable`, `daemon.unreachable`, `policy.invalid`); the remaining verbs currently
collapse to a generic `command.failed` with the diagnostic in `message`, and are being
migrated one at a time. Argparse errors come back as `parse.invalid`. An agent gets a
branchable result instead of prose to regex.

**Nothing fails silently.** A superseded, dropped, or cancelled request exits non-zero with a
message naming what happened: `4` superseded, `5` cancelled, `1` build failed. Note that
`bosn run` returns *your command's* exit code, so a test suite that itself exits 4 or 5 is
indistinguishable from these.

**Known rough edge:** your command's own output is buffered and printed after it exits, and
stdout wins over stderr when both are non-empty — so a long test run shows nothing until it
finishes. Build output streams; command output does not.

**Rapid edits must not pile up** — which is the concurrency policy below.

---

## Build jobs and the concurrency policy

Each edit is a new digest against the same stack, so requests neither join (not identical)
nor parallelize (same key). Left alone they queue without bound, every entry but the last
obsolete on arrival, each pinning volumes against GC.

**The rule: per `(workspace, stack)`, at most one running build and one pending request.**

| Request arrives while... | What happens |
| --- | --- |
| nothing is in flight | it builds |
| the same digest is building | it **joins** — one build, many watchers |
| a different digest is building | it takes the **pending slot** |
| the pending slot holds the same digest | it joins that |
| the pending slot holds a different digest | it replaces the occupant, which ends as **superseded** |

The bound is structural rather than tuned, and the worst case is waiting out one obsolete
build — which warms BuildKit's layer cache for the one that follows. Cancelling in-flight
builds so the newest always wins is more responsive but can livelock: under a fast edit loop
every build is killed before finishing and the agent never gets output. Rejecting the second
request turns a queue problem into a retry loop, which this project treats as a bug.

Superseding only ever drops a build that has **not started**, so cancellation never happens
behind your back. Stopping a running build is always deliberate — `bosn cancel`, daemon
shutdown, or the per-job TTL.

- **Distinct keys build in parallel.** Two worktrees never block each other. `workspace-id`
  is the resolved manifest root, never the cwd, so two agents in different subdirectories of
  one worktree correctly share a key.
- **Total concurrency is capped** at `max(2, cpus/2)` builds machine-wide (`BOSN_MAX_BUILDS`).
- **A cancelled build leaves nothing behind.** Registration happens only after `docker build`
  exits 0, so a killed build cannot leave a generation row implying a usable image.
- **A hung build cannot pin the daemon.** A per-job TTL (default 1 h,
  `BOSN_BUILD_TTL_SECONDS`) reaps anything that stops reporting builder progress.
- **Job history is bounded too.** The 50 most recent finished jobs stay listable; older ids
  report `no such job`. Build logs keep the last 5,000 lines, so a late `attach` sees the tail.

There is deliberately no `--detach`. Blocking-with-attach is the only mode, because
backgrounding is the easiest way to recreate the pile-up above.

---

## Command reference

```bash
bosn unit                    # run a manifest task by name (converges first)
bosn run -- cargo test       # ad-hoc command in the default stack
bosn shell                   # interactive session in the persistent container
bosn ensure                  # converge and register without running anything

bosn tasks --json            # tasks, stacks, content digests, registration state
bosn status                  # tiers, leases, bytes vs ceiling, foreign registries
bosn jobs / bosn attach <j>  # daemon-owned builds that survive a killed CLI
bosn cancel <j>              # stop a build you no longer want
bosn done                    # this workspace is finished; its caches become collectable
bosn gc                      # dry-run by default; --apply to reclaim (there is no --force)
bosn doctor                  # engine reachability, registry integrity, recovery commands
bosn adopt --legacy <family> # import pre-bosn volumes; --yes to apply

bosn __daemon --autostart    # install the login launcher (also --no-autostart)
bosn __daemon --stop         # stop the daemon — needed after upgrading bosn
```

`status`, `tasks`, and `doctor` read the sqlite registry directly, so they keep working when
the daemon is wedged. `gc` and `jobs` are read-only but go through the daemon and fail closed
if it is unreachable.

**After you upgrade bosn, stop the daemon.** A running daemon refuses every mutating verb
whose client reports a different version, so the next `bosn run` fails until you restart it.

### What `bosn done` actually deletes

It marks the workspace finished. At the next GC, every non-machine resource whose generations
are all current is collected **immediately**. Anything still carrying a *superseded*
generation — which after any Dockerfile edit means the persistent container, the
`stack`-scoped volume, and the previous generation's `target/` — falls to the conservative
superseded path instead and goes at the 24-hour cap. Machine-scoped caches survive either
way, and live leases outrank all of it, so a running build is never cut off.

bosn accepts only this first-party signal. Deriving "finished" from git state — worktree
missing, branch gone, PR merged — is the integrator's job; clud calls `bosn done` at worktree
teardown. Dirty work is never destroyed on inference.

---

## Architecture

A `bosn` CLI and a `bosn __daemon` singleton, following the soldr/zccache pattern: its own
repository, release cadence, and failure domain. State is **sqlite in WAL mode** — chosen
over redb for failure shape, since WAL readers never block behind a hung writer. The daemon
is the registry's only writer and the only executor of reap and GC; it is lazily spawned,
autostarted at login, and idle-retired with a scheduled catch-up tick. While the engine is
unreachable it backs maintenance off from 30 s to an hour rather than spinning against a
dead Docker.

**The daemon is a loopback TCP listener**, authenticated by a 32-byte token written to a
`0600` file and compared in constant time; unauthenticated attempts are logged. (The mode bit
is effectively a no-op on Windows.)

**One user per machine, in practice.** State is already per-user; what collides is the port.
The default state directory always maps to the fixed port 47764, and only `BOSN_PORT` — or an
explicit `--state-dir` pointing somewhere other than the resolved default — moves it. Setting
`BOSN_STATE_DIR` alone does not, because the port lookup resolves its default through that
same variable. More importantly for the disk story: `machine`-scoped volumes are Docker-global,
but each registry sees the other's as *foreign* — counted, never collected. bosn cannot bound
another user's growth.

## Configuration

Machine policy lives in `~/.config/bosn/config.toml` (or `$XDG_CONFIG_HOME/bosn/config.toml`;
set `BOSN_CONFIG` to select another file). Values use `default < file < environment < CLI flag`
precedence, and invalid values stop the command while naming the bad key. The `[policy]` table
accepts `container_idle_stop`, `container_remove`, `warm_volume_ttl`, `superseded_cap`,
`shared_cache_ceiling` (really the total managed-bytes ceiling), `run_max_duration` (default
8 h, the wall-clock cap on command execution), `idle_retire_seconds`, `build_ttl_seconds`, and
`max_builds`; environment overrides are their uppercase `BOSN_` equivalents. `bosn status`
reports each effective value and its origin.

Enforcement is layered: the daemon is the authority but never the only mechanism. The
persistent container's PID 1 watches the bind-mounted daemon heartbeat and exits once it has
been stale for 10 minutes, so containers drain even if the daemon dies.

## Registry recovery

Run `bosn doctor` after an unclean shutdown. It reports engine reachability, whether the
autostart launcher is installed, the next maintenance deadline, and registry integrity — and
when the database is damaged it prints the exact non-destructive backup-then-recover commands
for your path, deliberately leaving the original untouched. If it finds complete labels from a
prior registry it prints a source-specific `bosn adopt --from-registry <uuid>` command. That is
lost-database recovery only: it restores an identity into an empty registry and never replaces
the identity of a registry that already has rows.

## Integration

clud ships a thin `clud bosn …` forwarder that passes argv verbatim to the executable, exactly
as it shells out to soldr, pinned to a bosn release. The second seam is the worktree-teardown
hook that calls `bosn done`. Both degrade safely when bosn is absent.

---

## Development

```bash
git clone https://github.com/zackees/bosn && cd bosn
./install     # install uv if absent, then `uv sync --all-groups`
./lint        # ruff format --check, ruff check, pyright, KeyboardInterrupt checker
./test        # pytest; passthrough args work, e.g. ./test -k registry
```

`./install` sets up the development environment; run the CLI from the clone with
`uv run bosn`. Linting includes an AST checker (`ci/lint_kbi.py`) that enforces a sibling
`except KeyboardInterrupt` beside every broad `except Exception`, because a blind except
otherwise swallows Ctrl-C.

CI runs Linux, Windows, and macOS lanes on every pull request. Docker-backed tests carry a
`docker` pytest marker and run on Linux only — hosted macOS runners have no Docker daemon and
hosted Windows runners cannot run Linux containers, so real Docker Desktop coverage there
would need a self-hosted runner. All three lanes are required status checks on `main`, so a
red lane blocks the merge.

## License

MIT
