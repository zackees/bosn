# Rust Compose planning boundary

`bosn_core::compose` is the first Rust implementation slice for the Compose behavior
currently implemented in `src/bosn/compose.py`. It is deliberately a pure parser and
planner. It accepts YAML text, produces Bosn-owned typed values plus canonical JSON and a
`sha256:` plan digest, and has no filesystem, environment, Docker, daemon, registry, or
process API.

This means it is not a replacement for `bosn-docker compose` yet. In particular, it does
not execute Compose, generate an overlay, acquire leases, reconcile resources, inspect build
contexts, interpolate variables, or treat its digest as a final build-generation digest.
Those effects remain future daemon-owned work; callers must not bridge this gap by forwarding
raw YAML or Docker arguments.

## Characterized source mapping

The parser is based on the actual Python model and tests rather than Docker Compose's much
larger schema:

| Python source behavior | Rust planning representation |
| --- | --- |
| top-level `name`, `version`, `services`, `volumes`, `networks`; `x-*` extensions and YAML `<<:` merge keys | typed `ComposeDocument`; extensions are ignored after merge resolution |
| image/build services, profiles, named/bind/tmpfs mounts, named networks | typed `ServiceSpec`, `BuildSpec`, `MountSpec`, normalized lexical relative paths |
| environment, ports, dependencies, healthchecks, labels, command, entrypoint, restart, container name | typed fields in `ServiceSpec`; list ordering remains meaningful |
| top-level volume/network driver, options, labels, external/name/internal fields | typed `ResourceSpec` |
| unknown keys are fail-closed with dotted paths | `ComposeError { code, path, message, remedy }` |
| Python's content digest includes an on-disk Dockerfile/context closure | not ported here: the Rust plan digest is only canonical represented YAML semantics |

The tests characterize Python's realistic multi-service fixture and merge-key behavior. Maps
are normalized with ordered maps; cosmetic mapping order and YAML formatting produce the same
plan/digest, while ordered command/mount/etc. lists remain ordered.

## Intentional initial refusals

Python's current parser admits a few shapes because its old Docker front door passes them
through without a typed model. Rust must not silently do that. This foundation therefore
rejects values that it cannot represent faithfully, including `deploy`, secrets/configs,
build args and other build-object options, anonymous mounts, untyped mount options, inherited
environment-list entries, non-name-only per-service network options, undeclared named
resources, and unsafe/escaping source or target paths. Errors are structured rather than
falling back to raw Docker/Compose execution.

The path rule is lexical and pure: build and bind sources must be non-absolute, use `/`, and
cannot contain `..`; container targets must be normalized absolute paths without `..`. A
future daemon supplies the selected roots and revalidates them before any materialization or
engine operation.

## Next migration work

The next Compose work package must add daemon-owned planning/apply semantics, context-closure
digesting, lifecycle/lease/reconciliation behavior, and only then expose a typed client/CLI/
Python/MCP operation. It must preserve the existing front-door refusal catalog and never turn
this parser into a generic YAML or raw-Docker pass-through.
