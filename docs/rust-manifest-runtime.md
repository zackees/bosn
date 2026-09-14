# First native manifest runtime slice

`bosn manifest ensure` is the first daemon-owned runtime bridge for the
existing typed Rust `bosn.toml` manifest. It is intentionally smaller than the
legacy Python converge engine: it proves one safe lifecycle path without
silently treating unsupported declarations as Docker arguments.

```text
bosn manifest ensure --state-dir STATE --workspace WORKSPACE \
  --manifest bosn.toml --stack app --deadline-ms 300000 --output-limit 8388608
```

The same bounded semantic operation is available as
`Client.submit_manifest_ensure(...)` in the Python wheel and the
`bosn_manifest_ensure` Hermes MCP tool. The public request can select only the
workspace, a safe workspace-relative manifest path, declared stack name,
deadline, and output budget. It cannot supply an image, container identifier,
labels, Docker arguments, mounts, environment, command, or state path.

`bosn manifest app-task` now runs one named `[task.NAME]` declaration inside
an already ensured instance of that same supported stack:

```text
bosn manifest app-task --state-dir STATE --workspace WORKSPACE \
  --manifest bosn.toml --stack app --task check --deadline-ms 300000 --output-limit 8388608
```

It is also exposed as `Client.submit_manifest_app_task(...)` and the
`bosn_manifest_app_task` MCP tool. The task must belong to the selected stack.
The daemon re-reads and validates the manifest, verifies the immutable image,
inspect-proves the exact deterministic managed container is running, then uses
only `docker container exec NAME sh -lc DECLARED_COMMAND`. Cancellation,
deadline, and transport uncertainty retain a durable execution-session row and
`manifest.app-task.uncertain` event so GC and recovery protect that container.

## Implemented behavior

The daemon canonicalizes the workspace and manifest, requires the latter to
remain beneath the workspace, reads a bounded regular UTF-8 file through
`kernal-api`, and parses the existing `bosn-core` manifest model. It requires
the caller's exact stack name. For the supported shape it derives a generation
with `bosn-generation::stack_generation_async`, prepares/pulls the declared
immutable image, then uses the existing finite typed ensure primitive to
inspect/create/start only the deterministic Bosn-owned container. The daemon
atomically records its container, inspected image, resource uses, and a
`manifest.ensure.succeeded` event through the sole registry actor. A successful
new generation records its facts before retiring the prior manifest-container
resource/use for that same canonical workspace and stack in the same SQLite
transaction. Retirement never stops or removes Docker containers, images, or
volumes; conservative GC still requires its exact ownership, stopped-state,
lease, and execution-session checks.

Cancelling or exceeding the fixed budget cancels/reaps the direct Docker client
and leaves no success record. The durable job ID is observed with the ordinary
job status/log/cancel interfaces. Existing setup-document behavior is not
changed.

## Support and refusal matrix

| Manifest stack field | First native runtime slice |
| --- | --- |
| Standard Linux stack with `image = name@sha256:<64 lowercase hex>` | Supported |
| `[stack.NAME.env]` scalar environment | Supported after bounded validation |
| One explicitly named stack | Supported |
| Named `[task.NAME]` for that stack in an already ensured container | Supported; fixed daemon-owned exec only |
| Dockerfile/build context | Refused |
| macOS guest / guest fields | Refused |
| volumes, tmpfs | Refused |
| bind mounts | Refused in this first slice |
| workdir | Refused in this first slice |
| image tags or unpinned image references | Refused |
| generic `run`, shell, arbitrary Docker arguments | Not exposed |
| multi-stack orchestration | Refused; submit one named stack only |
| replacement/rollover | Supported for this accepted subset; a new immutable generation is ensured first, then only the same-workspace/stack prior manifest container is registry-retired |

The deterministic engine container is still checked against its exact derived
image and labels before reuse. An occupied mismatched candidate fails closed.

## Remaining work

This does not claim completion of manifest migration. Future slices must add
explicit lifecycle designs for bind mounts/workdir, managed volumes, build
materialization, multi-stack dependency ordering, guest lifecycle, declarative
tasks, autostart/recovery, and generation reconciliation. Each must
remain daemon-owned and registry-backed rather than reintroducing raw Docker or
Python business logic.
