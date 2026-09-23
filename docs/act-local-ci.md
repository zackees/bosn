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
