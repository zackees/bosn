# Local CI: `bosn ci`

`bosn ci` runs a repository's CI workflow on this machine, on an engine the
bosn daemon creates for that run and removes afterwards. It serves agents
(bounded, cursor-based output and stable exit codes) and humans. Tracking issue:
#323.

```sh
bosn ci run --wait                      # push trigger, minimal mode, ci.yml
bosn ci run --trigger pr --mode test    # pull_request + ci-test label
bosn ci list
bosn ci show RUN                        # Run -> stage -> job -> step tree
bosn ci logs RUN --follow               # or --since-seq N --limit N
bosn ci report RUN --json               # agent contract: first failure + tail
bosn ci cancel RUN | retry RUN [--job K] | wait RUN --deadline-ms N
bosn ci runners [list|drain|resume|set-limit N|prune-cache|cache|clear-cache]
```

`--timeout-secs N` (default 2 hours) bounds the whole run from the moment it
starts: resolving and pulling images, preparing the engine and running act. A
run that exceeds it ends `timed_out`, and its engine is still removed.

## Agents: MCP tools and `bosn.Client`

`bosn mcp` serves `bosn_ci_plan`, `bosn_ci_run`, `bosn_ci_status`,
`bosn_ci_list`, `bosn_ci_logs`, `bosn_ci_wait`, `bosn_ci_cancel`,
`bosn_ci_retry`, `bosn_ci_report` and `bosn_ci_runners`. Python gets the same contract through
`bosn.Client(state_dir).ci("<tool>", **arguments)` (for example
`client.ci("run", workspace="/repo", trigger="pr", mode="test")`), which
dispatches through the same code.

- Arguments are parsed eagerly into typed structs; an unknown field is
  refused.
- Every reply is a typed document of at most 64 KiB.
- `bosn_ci_run` returns a durable run ID at once. Waiting goes through
  `bosn_ci_wait`, which accepts a deadline of at most 10 minutes per call.
- Logs are cursor pages (`since_seq` to `next_seq`); each record comes back
  exactly once.
- `bosn_ci_report` gives the first failing job and step with only that step's
  tail (at most 20 lines).

## Dashboard (`bosn ui`)

The dashboard is opt-in. Enable it in the daemon state directory's
`config.toml`, then restart the daemon:

```toml
[ui]
enabled = true   # default false: no port is bound
port = 0         # 0 = an ephemeral port
```

`bosn ui [--path /ci/runs/RUN]` asks the daemon (over its owner-only socket)
for a single-use link, then picks where to show the page:

- The bosn widget's full-view window, when the widget is running. If it is
  installed but not running, it is started first; an explicit `bosn ui`
  overrides an earlier quit.
- The browser, with `--browser`, with `auto_launch = "never"`, when
  `bosn-widget` is not installed, or when the widget does not register within
  5 seconds.
- Printed only, with `--print` or when there is no desktop.

The dashboard and the widget panel both offer the run actions: cancel or
retry a run, drain/resume, set the limit, prune runs older than 7 days, and
measure or clear the cache volume. A destructive action takes a second click
within 4 seconds to confirm.

- The listener binds `127.0.0.1` only.
- The link is redeemed once for an `HttpOnly; SameSite=Strict` session cookie.
  A replayed link is refused.
- Every request's `Host` must name the listener, which defends against DNS
  rebinding.
- Every write must carry the listener's `Origin`, which defends against CSRF.
  A write with no `Origin` is refused.
- Every `/v1` route is one of the typed CI operations above; the listener can
  do nothing the daemon socket cannot.
- The page, script and stylesheet are compiled into the binary. They load no
  CDN, fonts or analytics, so the dashboard works with no network beyond
  loopback.

The live feed (`/v1/events`, server-sent events) is lossy: a reader that falls
behind is sent `resync` and refetches. A slow browser never delays the daemon
or other readers.

## Schema

[`docs/ci.schema.json`](ci.schema.json) is the published JSON Schema of the
contract:

- every request;
- each operation's reply (`--json` output and MCP structured content);
- the live event;
- the error document.

It is derived from the typed Rust definitions, and a unit test fails when the
committed copy is stale. After an intentional change, regenerate it with:

```sh
BOSN_UPDATE_SCHEMA=1 cargo test -p bosn-service published_schema
```

## Desktop widget (`bosn widget`)

`bosn-widget` is a separate binary. It links the webview toolkit, so the
`bosn` CLI, the daemon and headless installs never do. It runs in the user
session and opens three kinds of window onto daemon pages, each signed in
with its own single-use grant:

- a small **bubble** (`/widget/bubble`): running, queued and failed counts,
  coloured by the worst state;
- a **panel** (`/widget/panel`), toggled from the bubble: runs across
  workspaces with actor, branch, SHA (`+dirty`) and progress, plus runner
  controls;
- one **full view** (the dashboard).

The pages never call native code. A click POSTs to the daemon
(`/v1/widget/toggle`, `/open` and `/open-external`), and the widget picks the
command up on its next one-second poll. External links open in the OS
browser only for `https` URLs on `github.com`, `gitlab.com` or a host listed
in `[widget] external_hosts`.

```sh
bosn widget              # run it (a second start shows the running bubble)
bosn widget --detach
bosn widget install      # systemd user unit, started with the graphical session
```

**When it appears.** `[widget] auto_launch` is `always` (the default),
`on-activity` or `never`.

- The daemon tries `systemctl --user start bosn-widget.service`:
  - with `always`, when the daemon starts;
  - with `always` or `on-activity`, when a run is submitted.
  It tries at most once every 10 seconds, and only while no widget is
  connected and the dashboard listener is enabled.
- `bosn ci run` and `bosn ui` start it detached themselves when the daemon
  reports none and the terminal has a desktop.

**Quitting.** Closing the bubble is a deliberate quit. It suppresses
auto-launch until the next login (a new graphical session) or an explicit
`bosn widget`. A crash is not a quit: systemd restarts it, backing off after
five failures in a minute.

**Notifications.** The widget sends a desktop notification for every failed
run, and for the completion of runs a human started. Agent successes stay
silent.

**KDE Plasma on Wayland.** The window app id is `bosn-widget`. Until
kernal-api#384 adds keep-above and undecorated windows, a KWin rule does the
placement. Declare it in the desktop configuration (zackees/nixos):

```ini
[bosn widget bubble]
Description=bosn widget bubble
wmclass=bosn-widget
wmclassmatch=1
title=bosn
titlematch=1
above=true
aboverule=2
noborder=true
noborderrule=2
skiptaskbar=true
skiptaskbarrule=2
skippager=true
skippagerrule=2
```

## Fleet adapter plan (`bosn ci plan --adapter`)

`bosn ci plan --adapter RELATIVE_JSON` prints the fleet adapter V1 plan
(zackees/soldr#3345) as one JSON object on stdout (`"action":
"act_adapter_plan"`). It is read-only: it observes the checkout's HEAD and
cleanliness, reads the committed adapter declaration and the workflows it names,
and resolves the requested event into its declared cells and event payload. It
executes nothing and starts no daemon.

```sh
bosn ci plan --adapter RELATIVE_JSON --workspace . \
  --event pull_request|push|release --mode minimal|test|full --sha 40_HEX \
  --repo-owner OWNER --repo-name NAME [--base-sha 40_HEX] \
  [--pr-number N --head-owner O --head-name N --head-ref R --base-ref R --author-login L] [--json]
```

`--workspace` must be the clean checkout root at exactly `--sha`. Pull requests
need `--base-sha` and every PR identity option; other events refuse them.
Repository and PR metadata are operator-supplied and reported as unverified.
A refusal prints `bosn ci: <reason>` on stderr and exits 3.

`bosn act plan --adapter` is the deprecated spelling of the same
implementation: identical options, the same JSON byte for byte on stdout, and
the same refusal reasons (`bosn act: <reason>`, exit 2). It prints a
deprecation notice on stderr and is removed after one release.

## Exit codes

| Code | Meaning |
|------|---------|
| 0 | success |
| 1 | workflow failure, an engine or cleanup error, a daemon/transport error, or `cancel` on a run that was already finished |
| 2 | cancelled or timed out, or not finished yet (`wait` reached its deadline, or `report` on a running run) |
| 3 | refused (invalid arguments, release from a dirty tree, ...) or incomplete coverage |

A run is never reported as `success` when any job was `unsupported`, when no
job succeeded, or when its engine could not be proven removed.

## What runs, and where

- **Provider and engine are separate axes.** The provider is auto-detected:
  `.github/workflows/` means GitHub, and `.gitlab-ci.yml` (planned) means GitLab.
  If a checkout has both, pass `--provider`. The only engine so far is `act`.
  Both are recorded on every run.
- **Triggers:**
  - `pr` maps to `pull_request`, labelled `ci-test` (in `--mode test`) or
    `ci-full` (in `--mode full`).
  - `push` maps to `push` on the current branch.
  - `release` maps to `workflow_dispatch` with `commit_sha`. It requires
    `--mode full` and a clean tree.
- **Uncommitted work runs.** The client snapshots the working tree as-is: tracked
  and untracked files, honouring `.gitignore`. Deleted files are left out,
  symlinks are copied as links (never followed), executable bits are kept, and
  submodule checkouts are included. The snapshot goes into the daemon's staging
  area.
  - The run records `sha` plus `dirty: <tree digest>`, never only the bare SHA.
  - Editing the checkout during a run does not change what the run sees.
- **Isolation.** Each run gets one owned Act engine on the host engine
  (`crates/bosn-service/src/act_engine`, #349): a privileged container of the
  pinned `docker:29.7.2` publisher manifest, named `bosn-act-<run>`, with a
  read-only root, a private cgroup namespace, bounded memory, CPUs and
  processes, and its Docker storage on a private tmpfs. It carries the
  registry's ownership labels and a frozen creation profile.
  - Its one named mount is the machine-wide cache volume (below), frozen into
    the creation profile, verified before creation and on every observation.
    No host path or socket is mounted.
  - act is downloaded into the cache volume from the pinned release URL and
    checked against its pinned sha256. It runs inside the engine through
    `docker exec`, so it only sees the engine's private socket. The source
    snapshot and event payload are streamed in on `docker exec`'s stdin.
  - Every job container, network, volume and image act creates lives in the
    engine's private storage, which goes with the engine.
  - **This isolates resource ownership and cleanup, not untrusted code.** The
    engine is privileged, and jobs can reach its Docker socket, so a hostile
    workflow can escape to the host. Run only workflows you would run on this
    machine directly.
- **Lifecycle.** Each step goes through the registry (`docs/rust-registry.md`):
  1. A durable intent, with its frozen creation profile, is recorded.
  2. The engine is created, observed and registered, then started.
  3. The run takes an exclusive execution claim; every in-engine step
     re-verifies it.
  4. act runs.
  5. The outcome is recorded under the claim.
  6. The claim's owner requests cleanup; removal is authorized.
  7. The engine is removed.
  8. Its absence is proven.
  9. The record becomes terminal.

  A run whose cleanup fails stays `cleanup_required`. On daemon start, before
  any request is admitted, every non-terminal record is marked `interrupted`
  and its engine retired (`act_runtime::recover_startup_act_engines`); the
  window then seals. A container that holds an engine name without the exact
  ownership and isolation identity is never removed.
- **Scheduling.** There is one machine-wide FIFO queue with a live concurrency
  limit, which defaults to cores / 4 and is changed with
  `bosn ci runners set-limit N`.
  - Identical submissions share one run ID. "Identical" means the same SHA,
    dirty digest, workflow, job, trigger, mode, provider, engine, event payload
    (branch, PR number, repository) and timeout.
  - `drain` stops new runs from starting, and `resume` restarts them. Neither
    affects running jobs.
- **Actor.** Every run records who submitted it. `BOSN_CI_ACTOR` sets it
  explicitly. Under an agent session it is `agent:<session>`, and otherwise
  `human`.

## Caches (machine-wide)

Every engine mounts one bosn-labelled named volume, `bosn-ci-cache-v1`, at
`/bosn/cache`. It is a volume rather than a host directory because the
privileged engine writes as root, and Docker Desktop shares no host paths
with its VM. It holds:

- `tools/`: the pinned act release, verified by sha256 on every use.
- `images/`: the pinned runner image, saved once as a tar and loaded into each
  engine with `docker load` (a warm run skips the ~30 s pull).
- `actions/`: act's action checkouts (`--action-cache-path`). These are keyed
  by repository and ref, so they are safe to share.
- `actcache/<namespace>/`: act's cache server store (`--cache-server-path`),
  one per repository identity. The namespace is a hash of the `origin`
  repository, or of the checkout path when there is no origin. So two
  repositories using the same `actions/cache` key never restore each other's
  entries, while a repository's second run restores its own.

Artifacts use a per-engine store, so concurrent runs never share an artifact
server or its port.

## GitHub token (opt-in)

`bosn ci run --github-token` passes the daemon-owned secret
(`bosn secret set github_token`; see [task-secrets.md](task-secrets.md)) to
act as `-s GITHUB_TOKEN`.

- The value travels only in the Docker client's environment
  (`docker exec --env GITHUB_TOKEN`). It never appears in argv, records or logs.
- Run output is masked before it is parsed or stored.
- A missing secret runs anonymously, which GitHub limits to 60 API requests
  per hour. A refused secret (wrong permissions, or a symlink) fails the run.

## State

All runtime state is under the daemon state directory (`ci/`):

- `runs/<id>/run.json` is the record.
- `log.jsonl` holds seq-ordered log records. Each has `stream`, `job` and
  `section`.
- `event.json` is the event payload.
- `source/` is the frozen snapshot: the working tree, uncommitted work
  included, in a Git repository holding only the `HEAD` commit (depth 1), so
  a workflow's `git rev-parse HEAD`, `git diff` and `git status` behave as in
  a real checkout. It is kept for the newest 10 runs so they
  can be retried.

`runners cache` reports the size of the machine-wide cache volume, and
`runners clear-cache` removes it. Removal is refused while a run executes,
and the next run recreates the volume cold.

The newest 200 finished runs are kept; `runners prune-cache --older-than-secs N
--max-bytes N` prunes further.

## Live tests

The Docker-backed tests are opt-in (`#[ignore]`):

```sh
cargo test -p bosn-service --test ci_live --test ci_agent_live -- --ignored --test-threads 1
```

They check four things:

- The host engine's containers, networks, volumes and images are unchanged
  after success, failure, timeout, a killed client and a killed daemon.
- The engine has no bind mount and its socket belongs to the nested daemon.
- The recorded matrix/`needs:` fixture yields its tree, exit 1 and the
  failing step's tail.
- A second run restores `actions/cache`.

The leak check compares everything bosn owns on the host engine (its
ownership label and `bosn-act-` engines), so other tools using Docker at the
same time do not disturb it.

## Not yet

These are tracked in #323:

- Cross-repository sharing of compiler caches (zccache/soldr) outside
  `actions/cache`.
- Transparent, compositor-anchored widget windows (zackees/kernal-api#384; KWin rules cover keep-above today).
- GitLab.
