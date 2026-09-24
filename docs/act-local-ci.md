# Local GitHub Actions planning

`bosn act plan` is a read-only first step for mapping a fleet CI request to a
specific checkout and workflow file. It accepts `--event pull_request|push|release`,
`--mode minimal|test|full`, and an exact 40-hex `--sha`. A release requires full
mode. A push can request only minimal mode. `--workspace` must name the Git
checkout root. The SHA must equal the workspace's
Git `HEAD`; the selected workflow must match its committed bytes, and the
workspace must have no tracked or untracked changes reported by `git status`.
Tracked files marked assume-unchanged or skip-worktree are rejected. Bosn
checks `HEAD` and worktree status again after querying Act. Ignored files and
changes made after the final check are not covered, so the receipt is not an
execution snapshot.
This prevents a receipt from attributing edited workflow content to a commit.
`--workflow` must resolve to a file inside the workspace. `--act-version`
must match the installed `act --version` exactly. This is a caller-selected
version, not a verified fleet pin. `--act-bin` can select an
explicit executable; the default is `act` on `PATH`. Relative executable paths
are resolved from the workspace for both queries. Planning invokes only
`act --version` and `act -l -W <workflow>` with bounded time and output; it
does not create jobs or Docker resources. Timeout terminates the direct Act
process; a separately spawned descendant may outlive it.

```sh
bosn act plan --workspace . --workflow .github/workflows/ci.yml \
  --event pull_request --mode test --sha 0123456789abcdef0123456789abcdef01234567 \
  --act-version 0.2.88 --json
```

The JSON output is a versioned receipt with the workflow's declared `jobs`
and `selected_jobs` (filtered by `--job` when supplied), plus
`executable: false` and `docker_resources_tracked: false`. Act's list view
does not resolve event filters, job `if` conditions, reusable workflow internals,
or expanded matrices; `selection_scope` says so explicitly. The receipt has
`event_payload_resolved: false` and `fleet_pin_verified: false`. Each repository
still needs an adapter that constructs and checks the exact event payload and maps `test` to
the literal `ci-test` PR label, full PR to `ci-full`, and release to exact-SHA
dispatch. Local simulation cannot prove GitHub's native Windows/macOS/embedded
execution or the required real PR label-triggered runs.

`bosn act run` and `bosn act report` fail closed. Act launched with the host
Docker socket creates job containers and images outside Bosn's registry, even
when the outer Act process runs in a Bosn-managed container. Execution needs
an isolated engine whose entire lifecycle and storage are owned by Bosn, plus
bounded serial logs and a complete coverage report, before these commands can
be enabled. Do not use a host-socket task as evidence that nested resources
are supervised.

## Execution milestones

1. Define a repository adapter with a closed list of supported workflow/job
   IDs and exact event inputs. A PR adapter must build the event payload with
   the reviewed `ci-test` or `ci-full` label; a release adapter must require
   the requested commit SHA and full mode. Check out that SHA into a private,
   immutable source snapshot before either workflow inspection or execution.
   Refuse jobs whose event, `if`, matrix, reusable workflow, runner, or required
   secret behavior cannot be resolved locally. A successful `act -l` alone is
   not a coverage decision.
2. Add a Linux-only, fixed-shape nested Docker engine to Bosn's daemon-owned
   resource lifecycle. Pin its image by digest, record an intent before
   creation, and durably register the engine container and its storage before
   handing its private socket to Act. Never mount the host Docker socket into
   Act. On timeout, cancellation, client loss, and daemon restart, reconcile
   the exact registered engine identity and retire it only after ownership
   checks. The nested engine's storage is the cleanup boundary for Act-created
   containers, networks, images, and volumes.
3. Run the pinned Act binary against only that private socket and the frozen
   source snapshot. Stream bounded output into a durable run record with an
   exact SHA, event payload digest, workflow/job IDs, Act version, engine
   identity, start/end times, exit status, and explicit cleanup outcome. A
   timeout or incomplete cleanup is a failed run, never a successful report.
4. Make `bosn act report` require every declared supported job to have a
   completed result. Report unsupported/skipped jobs separately and never
   summarize them as a full pass. Exercise RED-to-GREEN tests with a synthetic
   engine for resource creation failure, Act failure, timeout, client death,
   daemon restart, and cleanup; then run one live isolated-engine test on
   Linux. GitHub-only runners and services remain explicitly uncovered.

The current setup engine cannot implement milestone 2 by configuration: its
only privileged container shape is the fixed macOS guest, and the Docker
command endpoint is not a nested-engine ownership boundary. That capability
needs a typed Bosn daemon operation and registry recovery before `act run`
can be enabled.
