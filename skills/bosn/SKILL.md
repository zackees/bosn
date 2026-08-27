---
name: bosn
description: Use bosn to create and operate managed Linux Docker development and test stacks. Trigger when a user asks to run tests, lint, commands, or an interactive shell in a reproducible Docker/Linux environment with warm persistent caches; when creating or improving a bosn.toml workflow; or when diagnosing bosn container, daemon, lease, cache, timeout, or cleanup behavior.
---

# Bosn Docker Workflow

Use bosn as the container lifecycle owner. Prefer its tasks and managed persistent
containers over ad-hoc `docker run` commands when the project has a `bosn.toml`.

## Operate an existing stack

1. Read the nearest `bosn.toml` and its task names with `bosn tasks`.
2. Check the engine with `bosn doctor` before diagnosing a failed stack. Treat an
   unreachable engine or clock-skew warning as a real prerequisite failure.
3. Use the project task when available:

   ```text
   bosn ensure
   bosn test
   bosn lint
   ```

4. For one-off non-interactive commands, use `bosn run -- <command>`. Use `bosn shell`
   only for a terminal session.
5. Preserve bosn's exit code. Do not replace it with a raw Docker fallback: that bypasses
   its labels, registry, leases, and cache lifecycle.

## Create a test workflow

Keep the image small and stable. Put project source in a read-only bind mount and put
package/tool caches in named volumes. A typical stack has:

- a pinned Linux base image and only runtime packages required by tests;
- a stack-scoped virtual environment or build-output volume;
- machine-scoped dependency and analysis caches when sharing them is safe;
- tasks that use the project lockfile and disable tool-local caches when necessary.

Use explicit volume destinations that match the tool's environment variables. Put
integration tests that require the host Docker daemon outside a nested Docker test task
unless Docker access is intentionally configured.

## Safety and recovery

- Let `bosn run` and `bosn shell` own foreground execution. Their sessions serialize a
  persistent container and clean up a killed client on the next acquire or maintenance pass.
- Do not delete `bosn`-labeled resources manually while a task is active. Use `bosn gc` to
  inspect lifecycle decisions and `bosn done` when a workspace is finished.
- Treat foreign registry warnings as protected state. Bosn intentionally cannot delete
  resources owned by a different registry identity; use explicit adoption only after
  confirming ownership.
- For an incomplete legacy volume, inspect `bosn gc --dry-run --json` and use only
  `bosn reconcile-volume --stack <stack> --volume <logical-name>` to preview it. Apply needs
  `--apply --yes`; never substitute `adopt`, a raw engine name, or a force-delete.
- After changing bosn code during development, restart its daemon before dogfooding so the
  in-memory process runs the updated implementation.

## Validate a change

Run the repository's host lint and focused tests first. Then dogfood the stack with
`bosn lint` and `bosn test`. Confirm that command output is live, a command's exit code is
preserved, and `bosn status` has no active execution session after the command ends.
