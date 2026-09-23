# CI runner capacity and queue SLO

Issue #257. Bosn's platform matrix runs on GitHub-hosted runners, and the
hosted pool is shared by **every** repository under the account, not by this
repository alone. A Bosn run therefore waits behind whatever the rest of the
fleet has queued, and from inside the run that looks like an unexplained
stall. This document records what caused the observed delay, what was changed,
the SLO the matrix is held to, and who to call when it is breached.

## What happened (before-change trace)

Run [34865549628](https://github.com/zackees/bosn/actions/runs/34865549628)
(push to `main`, 2026-09-14 15:57:33 UTC) declared seven independent jobs. All
seven were created at the same instant; they started over the next 26 minutes.
Queue time is job start minus job creation; execution time is job completion
minus job start.

| Job | Runner | Queued | Executed |
|---|---|---:|---:|
| Native wheel (windows-latest) | windows-latest | 6.5 min | 13.0 min |
| Darwin wheel (aarch64-apple-darwin, Linux-hosted Soldr) | ubuntu-latest | 9.6 min | 4.3 min |
| CI policy (no hosted macOS runners) — gate | ubuntu-latest | 17.5 min | 0.1 min |
| Rust workspace (locked tests) — gate | ubuntu-latest | 19.8 min | 1.2 min |
| Native wheel (ubuntu-latest) | ubuntu-latest | 21.9 min | 3.0 min |
| Linux (lint + unit + docker) | ubuntu-latest | 23.0 min | 5.8 min |
| Darwin wheel (x86_64-apple-darwin, Linux-hosted Soldr) | ubuntu-latest | 26.4 min | 4.2 min |

The Windows wheel's 13-minute execution included the 7m47s OpenSSL bootstrap
tracked separately in soldr#3231 (since removed by #259). That is execution
cost, not queue delay, and the report keeps the two columns apart so neither
can mask the other.

This trace is kept as `tests/fixtures/ci_queue_timing/run_34865549628.json`
and asserted by `tests/test_ci_queue_timing.py`.

## Root cause

An account-wide snapshot taken the same afternoon showed ~36 jobs running and
120 queued across 24 repositories — consistent with the hosted-runner
concurrency ceiling for the account plan (40 total jobs, 5 macOS), with the
macOS pool pinned at exactly 5. Nothing about Bosn's own workflow set the wait;
the pool was full of other repositories' work, most of it dead weight:

- runs already superseded by a newer push to the same branch or PR;
- runs for pull requests that had been merged a day earlier (kernal-api's
  73-job matrix, twice over);
- runs stuck in `queued` for weeks that nothing had cancelled.

Only `cancel-in-progress` on Bosn's own ref could not fix that, because the
competing load lived in other repositories.

## What changed

**Capacity decision: stay on hosted runners; reclaim the pool instead of
buying more of it.** No self-hosted or reserved runner group is introduced —
that would add an operational surface for a problem that was waste, not
demand. Darwin wheels remain Linux-hosted Soldr cross builds on
`ubuntu-latest`; hosted Mac execution is reserved for full CI and release,
as `ci/lint_no_macos_runners.py` enforces.

1. **Fleet-wide concurrency policy** (19 repositories, 224 workflow files,
   2026-09-14). Every workflow now carries:

   ```yaml
   concurrency:
     group: ${{ github.workflow }}-${{ github.ref == 'refs/heads/main' && github.run_id || github.ref }}
     cancel-in-progress: ${{ github.ref != 'refs/heads/main' }}
   ```

   A new push to a branch or PR cancels the run it supersedes. Runs on `main`
   are keyed on `run_id`, so they are never cancelled and never coalesced —
   the only release verification is never lost to a later push, which is the
   sense in which "keep cancellation, but never cancel the only verification"
   is satisfied: the replacement semantics apply to feature refs only.
   Release/publish workflows are never cancelled at all.
2. **Queue visibility in every run.** The final `CI queue timing` job in
   `.github/workflows/ci.yml` runs `ci/ci_queue_timing.py report` against the
   run's own job list and writes the table above to the step summary. An SLO
   breach adds a `::warning::` annotation per delayed job naming the runner
   label, the wait, the limit, and the escalation owner. It does not fail the
   run: capacity is an account-level condition, and turning a green matrix red
   would only hide the result behind the same queue.
3. **Operational probe.** `python ci/ci_queue_timing.py probe --branch main
   --count 3` prints the last three main-branch runs with maximum queue and
   execution times side by side and exits non-zero if any breached.

## CI tiers (soldr#3345)

Ordinary pull requests and main pushes run the minimal gate: the stable CI
policy and Rust workspace status jobs, plus tier selection and queue timing.
The `Rust workspace (locked tests)` job continues to run its locked tests, so
minimal still checks executable code. A PR with `ci-test` also runs the Linux
lint, unit, and Docker job. A PR with `ci-full` runs that tier plus both native
wheel jobs, both Linux-hosted Darwin cross-wheel jobs, and both hosted macOS
wheel smokes. `ci-full` takes precedence when both labels are present. Label
additions and removals trigger a new run and reselect from the current labels.

Manual dispatch accepts `minimal`, `test`, or `full`. A full dispatch requires
`commit_sha` matching the workflow run's exact `github.sha`; the checkout and
all wheel jobs use that same commit. For example, dispatch on a branch whose
head is the desired commit and supply its 40-character SHA. A mismatch fails
selection before the matrix starts. Release still uses its separate workflow.

The target for ordinary CI is at most 12.5% of a matched full event's total
runner minutes. The lane selection is in place, but that ratio needs live
minimal and full runs at the same revision before it can be reported.

## SLO

Measured per job as start minus creation, on hosted runners:

| Class | Jobs | Must start within |
|---|---|---:|
| Gates | `CI policy (no hosted macOS runners)`, `Rust workspace (locked tests)` | 5 min |
| Release-relevant matrix | every other lane: Linux, both native wheels, both Darwin cross wheels | 15 min |

The CI policy check keeps its legacy name because branch protection requires
that exact status. Its current guard allows hosted macOS runners only for
`ci-full` and release smoke jobs.

Execution time has no SLO here; it is reported so regressions are visible, and
tracked by their own issues (soldr#3231 for the Windows bootstrap).

Evidence after the change — main-branch runs, queue and execution reported
separately (run the probe to refresh):

| Run | Created (UTC) | Max queue | Max exec | Breaches |
|---|---|---:|---:|---|
| [34876130390](https://github.com/zackees/bosn/actions/runs/34876130390) | 2026-09-14 17:40 | 0.1 min | 5.3 min | none |

The fixture for that run is `tests/fixtures/ci_queue_timing/run_34876130390.json`.
Further rows are appended from the probe as main-branch runs accumulate; the
acceptance bar is three consecutive main runs inside both SLOs.

## Escalation

Owner: **@zackees** (account owner; the concurrency ceiling and any plan
change are account-level settings no repository can alter).

When a run's summary reports a breach:

1. Confirm it is the pool, not the run: the report names which jobs waited and
   for how long. A single late Windows lane with everything else on time is
   Windows capacity; every lane late is the shared pool.
2. Look at the global queue, not just this repository. Superseded or orphaned
   runs elsewhere are the usual cause; cancel them (`gh run cancel`) — runs
   that answer "has not been queued yet" are inert and can be ignored.
3. Check that the offending repository still carries the concurrency block
   above; a workflow added without it reintroduces the backlog.
4. Re-run only a lane that never started. A lane that started late but passed
   has already produced its evidence.
5. If the pool is legitimately full of live work for more than a day, the
   options are, in order: trim matrices that fan out per push (kernal-api's
   `each-feature`), raise the account plan's concurrency, or add a reserved
   runner group — each a decision for the owner, recorded here when taken.
