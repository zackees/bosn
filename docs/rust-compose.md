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

## Pure Compose-to-setup translation

`bosn_core::translate_compose_to_setup` is a second, still-inert planning boundary. It accepts a
previously parsed `ComposeDocument` and returns `ComposeSetupPlan`, which contains Bosn's existing
typed `SetupDocument`, canonical JSON, and a `sha256:` translation receipt. The receipt is over
the translated setup semantics plus the selected service name; it is distinct from the broader
Compose plan digest and is not a build-generation, container, or ownership receipt.

This is deliberately a *lossless single-app adapter*, not a partial Compose runner. It accepts
exactly one service with all of the following properties:

| Compose shape | Typed setup result |
| --- | --- |
| `image: name@sha256:<64 lowercase hex>` | `SetupSource::PinnedImage` with the same immutable reference |
| explicit `environment` mapping/list values that satisfy setup's identifier, size, and NUL rules | the same ordered-map environment |
| bind mounts with safe relative sources and normalized absolute targets | the same `WorkspaceMount` values |
| `working_dir` covered by a bind target | workspace-relative setup workdir which resolves back to that exact target |
| no `command`, or `command: [sh, -lc, <nonempty bounded script>]` | no setup command, or the same script used by setup's fixed `sh -lc` command shape |

The adapter neither chooses a workspace nor verifies that bind sources exist. It only derives a
workspace-relative workdir lexically from an already-declared bind mount. This keeps path
containment, filesystem observation, config acquisition, asset materialization, image
preparation, registry writes, and Docker calls outside the boundary.

Everything else is refused with a structured `ComposeError` and source path. In particular this
includes more or fewer than one service; unpinned images; `build`; profiles; named volumes and
tmpfs; top-level or service networks; ports; dependencies; healthchecks; labels; restart policy;
container names; entrypoint overrides; arbitrary command vectors or strings; and workdirs not
covered by a bind mount. Those features need a later daemon-owned multi-service/lifecycle model;
they must never be smuggled through this adapter as raw Compose YAML or Docker arguments.

## Setup-source acquisition

The existing typed setup lifecycle can acquire this lossless subset without introducing a raw
Compose execution path. A selected local path or HTTPS URL ending in `.yaml` or `.yml` (case
insensitive, before an HTTPS query string) is parsed exactly once through
`parse_and_translate_compose_yaml`, and only the resulting `SetupDocument` reaches the existing
setup cache, plan, image-preparation, ensure, or task flows. The original YAML bytes remain the
content-hash/cache receipt; the translated typed document remains the only downstream input.

All other locator names, including extensionless legacy paths, remain TOML-only. Bosn does not
probe YAML and then TOML, or vice versa. Consequently a TOML body at a `.yaml` URL and a Compose
body at a `.toml` URL both fail closed. Offline mode revalidates the same source syntax from the
locator bound into its cache key, and never contacts the source or falls back to another cached
format. The existing one MiB document limit applies before either parser runs.

This makes `bosn setup plan`, the matching Python `Client.plan_setup`, and MCP setup tools accurate
for the narrow supported one-service YAML shape. It does not make `bosn compose plan` an apply
path, and it does not permit a multi-service Compose file to enter setup/engine execution.

## Read-only front doors

The pure plan is available for review before any future execution work:

```text
bosn compose plan --file compose.yaml --json
```

The CLI reads only the explicitly supplied local file, caps it at one MiB, and returns
`applied: false`. It never opens a Bosn state directory, contacts a daemon/Docker, or
uses a Compose executable. Python callers can use `bosn.plan_compose_yaml(source)`;
its immutable result exposes `version`, `digest`, `normalized_json`, `document_json`, and
`applied` (always false). The MCP tool `bosn_compose_plan` accepts the full YAML in its
bounded `document` string (32 KiB), specifically rather than accepting a path or URL. It
returns the same typed document, digest, and `applied: false` receipt.

## Next migration work

The next Compose work package must add daemon-owned planning/apply semantics, context-closure
digesting, multi-service/network/volume lifecycle, leases/reconciliation behavior, and only then
expose a typed client/CLI/Python/MCP operation. It must preserve the existing front-door refusal
catalog and never turn either pure planner into a generic YAML or raw-Docker pass-through.
