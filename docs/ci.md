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
bosn ci runners [list|drain|resume|set-limit N|prune-cache]
```

## Agents: MCP tools and `bosn.Client`

`bosn mcp` serves `bosn_ci_plan`, `bosn_ci_run`, `bosn_ci_status`,
`bosn_ci_list`, `bosn_ci_logs`, `bosn_ci_wait`, `bosn_ci_cancel`,
`bosn_ci_report` and `bosn_ci_runners`. Python gets the same contract through
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
for a single-use link and opens it in the browser. `--print` only prints it.

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
- **Isolation.** Each run gets one privileged `docker:dind` container on the
  host engine: the *engine*, pinned by digest and named `bosn-act-<run>`. It
  carries the registry's ownership labels.
  - act is downloaded inside the engine from the pinned release URL and checked
    against its pinned sha256. It runs there through `docker exec`, so it only
    sees the engine's private socket. The host socket is never mounted.
  - Every job container, network, volume and image act creates lives in the
    engine's own storage, which `docker rm -f -v <engine id>` removes with it.
  - **This isolates resource ownership and cleanup, not untrusted code.** The
    engine is privileged, and jobs can reach its Docker socket, so a hostile
    workflow can escape to the host. Run only workflows you would run on this
    machine directly.
- **Lifecycle.** Each step goes through the registry (`docs/rust-registry.md`):
  1. A durable intent is recorded.
  2. The engine is created.
  3. It is observed and registered.
  4. act runs.
  5. The outcome is recorded.
  6. Cleanup is requested and authorized.
  7. The engine is removed.
  8. Its absence is proven.
  9. The record becomes terminal.

  On daemon start, every non-terminal record is reconciled and marked
  `interrupted`. A container that holds an engine name without the exact
  ownership labels is never removed.
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
- `source/` is the frozen snapshot. It is kept for the newest 10 runs so they
  can be retried.

The newest 200 finished runs are kept; `runners prune-cache --older-than-secs N
--max-bytes N` prunes further.

## Not yet

These are tracked in #323:

- Cross-repository sharing of compiler caches (zccache/soldr) outside
  `actions/cache`.
- The desktop widget and a native full-view window (blocked on zackees/kernal-api#384).
- GitLab.
- Skipped *steps* (`if:` false) are not yet listed; skipped and unsupported
  *jobs* are.
