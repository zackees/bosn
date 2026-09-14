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
with the bounded `bosn-generation` collector, prepares/pulls the declared
immutable image or materializes an accepted Dockerfile context into
owner-private Bosn state, then uses the existing finite typed ensure primitive to
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
| `dockerfile = 'Dockerfile'` build context | Supported for the selected workspace-root Dockerfile. The daemon collects the finite Docker-selected `COPY`/`ADD` context through `kernal-api`, refuses selected symlinks, special files, empty selected directories, traversal, unbounded assets, alternate Dockerfile locations, and a simultaneous `image`. It copies exact observed regular-file bytes to an owner-private content-addressed setup asset tree before the typed build primitive invokes Docker; Docker never receives the workspace as its build context. Every external Dockerfile image must itself be digest-pinned. |
| `kind = 'macos-x64-guest'` with explicit license acknowledgement | Supported only for `dockurr/macos` and Docker Hub aliases (`docker.io`, `index.docker.io`, `registry-1.docker.io`) pinned by a lowercase 64-hex `sha256` digest, with exactly `[stack.NAME.volumes.storage]` declared as `scope = 'machine'`, `destination = '/storage'`, and `retention = 'pinned'`. This prevents an ephemeral VM disk and prevents the fixed KVM/tun create shape from executing a manifest-selected privileged image. On Linux, kernal-api must report both `/dev/kvm` and `/dev/net/tun`. Bosn emits only the fixed dockurr KVM/tun/NET_ADMIN, loopback SSH/web-port, 120-second stop timeout, and sizing shape; it never accepts raw privilege/device/port arguments. The durable container resource uses the `manifest-guest:` namespace and rolls over conservatively like a manifest container. |
| named `[stack.NAME.volumes]` | Supported for typed Bosn-managed named volumes. The daemon derives the engine name from the declared logical name, scope, canonical workspace, and (for `spec`) generation; callers cannot supply a Docker volume name or mount string. It writes a durable creation intent before `docker volume create`, requires exact ownership labels before reuse, then atomically records the resource and consumes the intent after container ensure. `spec` rolls with generation; `stack` survives generations within its workspace; `machine` follows the declared `family` or stack. Normal rollover and GC never delete volume data in this slice; retention is recorded for later explicit lifecycle work. |
| `tmpfs` | Supported only as an array of normalized legacy strings: `/target`, `/target:ro`, `/target:rw`, with an optional one `size=POSITIVE{b,k,m,g}` option (for example `/run/cache:rw,size=64m`). The daemon parses those into typed target/mode/size values before its engine seam, incorporates the declaration into the runtime generation, and emits only its own finite `--tmpfs` form. Repeated modes/sizes and all other options (`noexec`, `mode`, `uid`, etc.) are refused rather than passed through. tmpfs is disposable container state; a generation rollover creates a new empty tmpfs. |
| `[stack.NAME.mounts]` workspace bind mounts | Supported for existing paths that canonicalize beneath the selected workspace. Sources may be legacy absolute paths only when they resolve beneath that workspace; traversal, source symlinks, escapes, duplicate targets, reserved targets, and unrepresentable Docker paths are refused. `readonly` is retained. |
| `workdir` | Supported only when its normalized absolute container path is covered by a declared workspace bind. It is translated to the typed workspace-relative form, becomes the persistent container workdir, and is therefore inherited by declared `manifest app-task` exec. Image-only workdirs are refused. |
| image tags or unpinned image references | Refused |
| generic `run`, shell, arbitrary Docker arguments | Not exposed |
| multi-stack orchestration | Refused; submit one named stack only |
| replacement/rollover | Supported for this accepted subset; a new immutable generation is ensured first, then only the same-workspace/stack prior manifest container is registry-retired |

`manifest app-task` deliberately refuses a macOS guest. Legacy guest work is
transported through SSH/SCP rather than `docker exec`; native Bosn does not yet
have a typed SSH/SCP task primitive with equivalent cancellation and durable
uncertainty semantics. Likewise guest `workdir` is refused rather than being
mistaken for the Linux-container workdir. This keeps the implemented guest
slice real (the actual KVM VM lifecycle and durable accounting), without
pretending the Linux app-task transport reaches inside the VM.

The deterministic engine container is still checked against its exact derived
image and labels before reuse. An occupied mismatched candidate fails closed.
The runtime generation incorporates the accepted effective bind/workdir, named
volume, and tmpfs shape
in addition to the historical manifest generation, so a bind target/source/
readonly or workdir change cannot reuse a container configured for the prior
lifecycle declaration. Before each ensure and manifest app-task operation the
daemon re-reads the manifest; the typed setup primitive then canonicalizes the
same mount sources again immediately before its engine operation.

## Remaining work

This does not claim completion of manifest migration. Future slices must add
explicit volume release/GC application, broader Dockerfile forms (alternate
Dockerfile paths plus selected symlink and empty-directory representation),
multi-stack dependency ordering, guest SSH/SCP task lifecycle, declarative tasks,
autostart/recovery, and generation reconciliation. Each must
remain daemon-owned and registry-backed rather than reintroducing raw Docker or
Python business logic.
