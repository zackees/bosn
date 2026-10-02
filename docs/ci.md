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

## Exit codes

| Code | Meaning |
|------|---------|
| 0 | success |
| 1 | workflow failure, or an engine or cleanup error |
| 2 | cancelled or timed out (also `wait` reaching its deadline) |
| 3 | refused, or coverage incomplete (some jobs need a runner bosn cannot supervise) |

A run is never reported as `success` when any job was `unsupported`, when no
job ran, or when its engine could not be proven removed.

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
    engine's own storage, which `docker rm -f -v` removes with it.
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
    dirty digest, workflow, job, trigger, mode, provider and engine.
  - `drain` stops new runs from starting, and `resume` restarts them. Neither
    affects running jobs.
- **Actor.** Every run records who submitted it. `BOSN_CI_ACTOR` sets it
  explicitly. Under an agent session it is `agent:<session>`, and otherwise
  `human`.

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

- The persistent action and cache volumes and the runner-image cache (#302, #303).
  Each run currently pulls the runner image inside its engine, which takes
  about 40 s.
- Secrets and `GITHUB_TOKEN` (#308).
- MCP tools and `bosn.Client` methods.
- The daemon UI and the desktop widget.
- GitLab.
- Skipped *steps* (`if:` false) are not yet listed; skipped and unsupported
  *jobs* are.
