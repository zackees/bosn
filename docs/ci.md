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

**Quitting.** Closing the bubble is a deliberate quit. The bubble has no
title bar, so close it from the window manager (Alt+F4 while it has focus);
a panel Quit button is #409. Quitting suppresses
auto-launch until the next login (a new graphical session) or an explicit
`bosn widget`. A crash is not a quit: systemd restarts it, backing off after
five failures in a minute.

**Notifications.** The widget sends a desktop notification for every failed
run, and for the completion of runs a human started. Agent successes stay
silent.

**Windows.** Every window carries the app id `dev.bosn.widget`. Each is
opened once, at its final size, and then reused:

- The bubble is undecorated and transparent, and asks to stay above other
  windows and out of the taskbar. On X11, Windows and macOS it also asks for a
  fixed spot near the top-left corner, because kernal-api cannot report the
  work area yet (zackees/kernal-api#393). On macOS, leaving the taskbar puts the
  whole widget process in the accessory policy, with no Dock icon or menu bar.
- The panel is shown and hidden; it is not closed and reopened.
- The full view navigates to the new page and takes focus, so there is only
  ever one.

**KDE Plasma on Wayland.** The compositor decides stacking, the taskbar and
placement, so the bubble's requests do nothing there. A KWin rule supplies
them. It matches the app id and the bubble's title, `bosn bubble`; the panel
(`bosn panel`) and the full view (`bosn`) stay ordinary windows. Set
`position` to the bottom-right corner of your work area, minus the 72 px bubble
and a margin. Then apply the rule, or declare the same keys in the desktop
configuration (zackees/nixos, plasma-manager `window-rules`):

```sh
g=bosn-widget-bubble
k() { kwriteconfig6 --file kwinrulesrc --group "$g" --key "$1" "$2"; }
k Description "bosn widget bubble"
k wmclass dev.bosn.widget; k wmclassmatch 1        # 1 = exact match
k title "bosn bubble";     k titlematch 1
k above true;              k aboverule 2           # 2 = force
k noborder true;           k noborderrule 2
k skiptaskbar true;        k skiptaskbarrule 2
k skippager true;          k skippagerrule 2
k skipswitcher true;       k skipswitcherrule 2
k position "3887,1399";    k positionrule 2
# With other rules already present, list them all here and count them.
kwriteconfig6 --file kwinrulesrc --group General --key count 1
kwriteconfig6 --file kwinrulesrc --group General --key rules "$g"
qdbus org.kde.KWin /KWin reconfigure
```

On Wayland, size every window when it opens. A later resize is not applied on
some hosts (zackees/kernal-api#390). Compositor-anchored placement without a
rule is zackees/kernal-api#389.

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

A matrix leg is keyed by its job name plus the matrix values that name does
not show (`CI/Native wheel (ubuntu-latest) (3.11)`), never by act's leg
number, and sits in its job's stage. A job whose `runs-on` is a matrix
expression is decided per leg by bosn, not act (act 0.2.88 lets the first leg
overwrite every leg's `runs-on`): a leg whose own runner is not a local Linux
label is `unsupported`, every time (#404).

## Jobs only GitHub can run (`remote_only`)

Some jobs cannot run under act at all (zackees/ci.yml GATE-012, #400). A job
that asks the GitHub API about its own run gets a 404, because act's
`github.run_id` is a local ID. A job that needs an OIDC token, or a
GitHub-side service, cannot run either. `bosn ci` treats a job as remote-only
when:

- its job-level `env:` declares `CI_REMOTE_ONLY: <reason>`. This is the
  general declaration; on GitHub it is an unused variable, so the job and any
  required check built on it are unchanged;
- it uses an action from GATE-012's act-impossible registry
  (`crates/bosn-service/src/ci/remote_only.rs`); or
- it requests `id-token: write`.

In the run's copy of the workflow such a job keeps its `if:`, `needs:`, runner
and matrix, so it is skipped exactly when GitHub would skip it, and the jobs
that need it still run. Only its steps are replaced, by one step that prints
the reason. The run log notes each such job. The job is then reported
`remote_only` with its reason (`ci report` lists it, `ci show` marks it
`[remote-only]`). It is never a failure and never a coverage gap: by policy it
is not local evidence. Declare a job this way rather than guarding it with
`if: ${{ !env.ACT }}`, which hides the reason and skips its dependents.

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
  - The job sees uncommitted work as a **synthetic commit** on top of `sha`
    (by `bosn`, dated like `sha`, so the same tree always gives the same
    commit and identical runs still coalesce). Its checkout is clean, and
    `HEAD`, `github.sha` and `pull_request.head.sha` all name that commit, so
    a workflow that cleans its tree (`git restore`, `git reset --hard`) still
    builds the work under test (#394). The record keeps the real `sha` and
    names the synthetic one in `commit`. A clean tree is checked out at
    exactly `sha`.
  - bosn's own rewrites of `actions/checkout` steps are marked skip-worktree,
    so they neither show in `git status` nor are reverted by a restore.
  - A detached `HEAD` is checked out detached at the same commit (#393).
  - Editing the checkout during a run does not change what the run sees.
- **Isolation.** Each run gets one owned Act engine on the host engine
  (`crates/bosn-service/src/act_engine`, #349): a privileged container of the
  pinned `docker:29.7.2` publisher manifest, named `bosn-act-<run>`, with a
  read-only root, a private cgroup namespace, bounded memory, CPUs and
  processes, and its Docker storage on a private tmpfs. It carries the
  registry's ownership labels and a frozen creation profile.
  - Its limits are sized from the host engine's machine (`docker info`, plus
    `/proc/meminfo` when that is the same machine): memory is half the total,
    at most three quarters of what is available, held between 4 and 48 GiB;
    the storage tmpfs, which is RAM and counts against that memory, is three
    quarters of it, always leaving 2 GiB; CPUs are min(cores, 8); 4096
    processes. Neither limit reserves RAM until it is written; both only
    bound a runaway job. Any of them can be pinned in `<state>/config.toml`
    (pinning only `storage_gib` grows the sized memory to fit it):

    ```toml
    [engine]
    memory_gib = 16
    storage_gib = 10
    cpus = 4
    pids = 4096
    spares = 0   # no prepared spare engine (default 1)
    ```

    The chosen limits are frozen into the creation profile; creation and every
    observation verify the engine against it exactly, so a later host or
    config change never alters an existing run's engine.
  - While act runs, the engine's storage is sampled (`df` inside the engine)
    every few seconds. The log notes its peak; when under 5 GiB is left on a
    mostly used engine it warns at once, and a run that then fails says so
    in its `reason` (`bosn ci report`), naming `storage_gib`. A step that a
    free-space guard refused (soldr will not build under 5 GiB) or that hit
    ENOSPC is explained rather than failing silently (#392).
  - **One prepared spare engine (#410).** Once a daemon has taken its first
    `bosn ci` run, whenever nothing is queued and a run slot is free, it
    creates one engine with no run and prepares it
    as far as bosn's own fixed scripts go: ready, act installed and verified,
    runner image loaded and proven. The next run claims it instead of
    creating and preparing its own (about 15 s), then seeds the tool cache
    and streams in its source as usual; a new spare is prepared behind it.
    - It is an ordinary owned engine (`bosn-act-<spare>`, labelled
      `com.zackees.bosn.act.spare=true`): its intent and frozen creation
      profile are durable before it exists, and the daemon holds it under its
      own execution claim. A run takes it over in one registry transaction
      that replaces that claim with the run's and records the run it serves;
      only one claim can. From then on it is that run's engine, removed with
      proof when the run ends.
    - A run claims a spare only when its intent would create exactly the same
      engine (pins and creation profile); a spare made for other limits or
      pins is retired, never claimed. Startup recovery retires a spare a dead
      daemon left, prepared or half-created, like any other engine, and
      `bosn daemon stop` removes it before the daemon stops answering.
    - At most one is kept. It counts against the run concurrency limit (it is
      only prepared while a slot is free) and is a registered owned resource.
      It is kept only when the host has 16 GiB of memory available, since an
      idle spare holds its runner image (about 2 GiB) in its RAM-backed
      storage; `bosn ci runners --json` shows it with that usage. Clearing
      the cache volume retires it first. Opt out with `[engine] spares = 0`.
  - Its one named mount is the machine-wide cache volume (below), frozen into
    the creation profile, verified before creation and on every observation.
    No host path or socket is mounted.
  - Every artifact is pinned in one place (`crates/bosn-service/src/ci/pins.rs`).
    act is downloaded into the cache volume from the pinned release URL; the
    tarball and the binary inside are checked against their pinned sha256s.
    The runner image (`catthehacker/ubuntu:act-24.04`, linux/amd64, pinned by
    manifest digest) is loaded from the cache volume's image tar, or pulled by
    digest once and saved there; either way `docker image inspect` must then
    show exactly the pinned manifest, config, execution config and rootfs,
    proven against the publisher bytes the daemon ships. A cached tar that
    fails the proof is discarded and pulled again. act runs inside the engine through
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
  included, in a Git repository holding only the `HEAD` commit (depth 1, plus
  the synthetic commit of a dirty tree), so a workflow's `git rev-parse HEAD`,
  `git diff` and `git status` behave as in a real checkout.
  `refs/bosn/base` names the commit the snapshot was taken from. A `--trigger
  pr` run also holds its base branch as `origin/<base>` (origin's default
  branch, else `main`, at `origin/<base>` or the local branch), with the
  history of both tips down to their merge base, so `git merge-base
  origin/main HEAD` works as in a `fetch-depth: 0` PR checkout; the payload's
  `pull_request.base.ref` and `.sha` name the same commit (#403). It is kept
  for the newest 10 runs so they can be retried.

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
- Compositor-anchored widget placement without a KWin rule (zackees/kernal-api#389), and a bottom-right default placement elsewhere (zackees/kernal-api#393).
- GitLab.
