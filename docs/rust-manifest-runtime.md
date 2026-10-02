# First native manifest runtime slice

`bosn manifest ensure` is the first daemon-owned runtime bridge for the
existing typed Rust `bosn.toml` manifest. It is intentionally smaller than the
legacy Python converge engine: it proves one safe lifecycle path without
silently treating unsupported declarations as Docker arguments.

```text
bosn manifest ensure --state-dir STATE --workspace WORKSPACE \
  --manifest bosn.toml --stack app --deadline-ms 300000 --output-limit 8388608
```

## `bosn run`

`bosn run` is the everyday front door. It adds no authority of its own: it
composes `manifest ensure` and `manifest app-task` with defaults taken from
the working tree.

```text
bosn run --task NAME [--stack NAME] [--manifest PATH] [--state-dir STATE_DIR]
         [--deadline-ms 1..=14400000] [--output-limit 1..=67108864] [--no-ensure]
bosn run --stack NAME [--manifest PATH] [--state-dir STATE_DIR] [...]
```

| Input | Default |
| --- | --- |
| `--manifest` | the nearest `bosn.toml` in the current directory or a parent; the workspace is the manifest's own directory |
| `--stack` | the `stack` the named task declares (a contradicting `--stack` is refused) |
| `--state-dir` | `$BOSN_STATE_DIR`, else `$XDG_STATE_HOME/bosn`, else `~/.local/state/bosn` (`%LOCALAPPDATA%\bosn` on Windows) |
| `--deadline-ms` | 1 hour for the ensure, 2 hours for the task |
| `--output-limit` | 64 MiB (the manifest maximum) |

It pings the daemon for that state directory and, when none answers, starts
`bosn daemon serve --state-dir STATE_DIR` detached (its own process group, so
Ctrl-C on the client does not reach it). It then submits the stack's ensure
(skipped with `--no-ensure`), which reuses the exact running container when
the generation is unchanged, and the declared task. Each job's logs are
streamed as they arrive: the task's stdout goes to stdout, its stderr and all
daemon progress to stderr, and every ensure record to stderr. Before
following the task it prints `bosn run: job N` with the exact `bosn job
cancel --state-dir … --job-id N` command, and it says so while the job is
queued behind other jobs on the daemon. Ctrl-C cancels the running job and
exits 130.

The task is submitted with a 30-second follow lease (#357). The client polls
every 200 ms. If no poll arrives within the lease, the daemon cancels the job,
whether it is queued or running. This covers a client killed by SIGTERM,
SIGHUP or SIGKILL, or a parent session that died; without the lease the job
would run on to its deadline. Jobs submitted without a lease (`manifest
app-task`, MCP, the Python client) are unchanged and outlive their caller. A
cancelled, timed-out or output-limited task exec is followed by a stop in the
same container. Every exec carries a random `BOSN_TASK_TOKEN`, which all of its
processes inherit. The stop sends SIGINT, then SIGTERM, then SIGKILL, each
after a 10-second grace, to exactly the processes carrying that marker, so an
`act` run tears down its job containers rather than running on. Only a
confirmed stop clears the job's execution session; otherwise it stays
`uncertain`. The exit status is the task's own exit status
(0 on success); a Bosn-side refusal or failure exits 1 with a `bosn run:`
message, and bad arguments exit 2. `bosn run --stack NAME` only ensures.

Manifest operations (`manifest ensure`, `manifest converge`, `manifest
app-task`, their MCP tools, and `bosn run`) accept a deadline of up to four
hours and an output budget of up to 64 MiB, because a declared stack builds
and runs whole CI workloads. Setup-document operations keep their five-minute
and 8 MiB bounds. Task output is held in daemon memory until the exec ends, so
the output ceiling remains finite.

The same bounded semantic operation is available as
`Client.submit_manifest_ensure(...)` in the Python wheel and the
`bosn_manifest_ensure` Hermes MCP tool. The public request can select only the
workspace, a safe workspace-relative manifest path, declared stack name,
deadline, and output budget. It cannot supply an image, container identifier,
labels, Docker arguments, mounts, environment, command, or state path.

`bosn manifest converge` is the corresponding whole-document operation:

```text
bosn manifest converge --state-dir STATE --workspace WORKSPACE \
  --manifest bosn.toml --deadline-ms 300000 --output-limit 8388608
```

It is also available as `Client.submit_manifest_converge(...)` and the
`bosn_manifest_converge` MCP tool. The existing TOML manifest schema has no
dependency/root relation: `family` is a volume identity hint, and unknown
`depends_on`/`dependencies` fields are parse errors. Therefore this operation
accepts no root, dependency, or ordering selector. It validates the document,
snapshots its declared stack names, then ensures them in deterministic lexical
order through one daemon job. The daemon is globally single-flight, so a
batch's volume and guest setup cannot race another engine lifecycle job. Every
member uses the same typed image/Dockerfile, bind/workdir, volume, tmpfs,
guest, ownership, and registry path as `manifest ensure`. A member is recorded
and its own generation rollover committed before the next member starts. If a
later member fails or the batch is cancelled, the job stops and prior proven
member records remain durable; Bosn does not claim an atomic multi-container
rollback it cannot safely prove.

`bosn manifest app-task` now runs one named `[task.NAME]` declaration inside
an already ensured instance of that same supported stack:

```text
bosn manifest app-task --state-dir STATE --workspace WORKSPACE \
  --manifest bosn.toml --stack app --task check --deadline-ms 300000 --output-limit 8388608
```

It is also exposed as `Client.submit_manifest_app_task(...)` and the
`bosn_manifest_app_task` MCP tool. The task must belong to the selected stack.
The daemon re-reads and validates the manifest, verifies the immutable image,
and inspect-proves the exact deterministic managed container is running. Linux
stacks then use only `docker container exec NAME sh -lc DECLARED_COMMAND`.
Cancellation, deadline, and transport uncertainty retain a durable
execution-session row and `manifest.app-task.uncertain` event so GC and
recovery protect that container.

**Login shell and the image's `ENV PATH`.** Every declared command (a setup
task's `docker run`, the long-running app command, and each app task's
`docker container exec`) runs in a login shell, so `/etc/profile`,
`/etc/profile.d/*`, and `~/.profile` apply. Debian- and Alpine-based images'
`/etc/profile` *assigns* `PATH`, which would drop every `ENV PATH` entry the
image declares (`/usr/local/cargo/bin` in `rust:*`, `/opt/<tool>/bin`). Bosn
therefore runs `sh -c LAUNCHER sh SCRIPT`: the non-login launcher records the
container's pre-profile `PATH` in `BOSN_IMAGE_PATH` and `exec`s
`sh -lc SCRIPT`, whose first line, after the profile ran, sets
`PATH="$BOSN_IMAGE_PATH:$PATH"` and unsets the carrier. The image's entries
lead, the profile's follow, and the declared command text is unchanged. An
image no longer needs a `/etc/profile.d` shim to keep its `ENV PATH`
(`crates/bosn-setup/src/shell.rs`).

For an accepted macOS guest, the same operation first proves the dockurr
container exact/running, then performs a bounded SSH `true` readiness probe
and runs only the re-derived `[task.NAME].cmd` over SSH. The SSH adapter fixes
the host to `127.0.0.1`, uses only the manifest-derived published SSH port and
validated username, ignores ambient SSH configuration (`-F /dev/null`), and
uses batch, key-only authentication. It cannot accept a caller host, port,
username, command, arguments, SSH options, credential path, or SCP input.

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
| `dockerfile = 'relative/Dockerfile'` build context | Supported for a safe relative Dockerfile label, selected regular files, and selected empty directories. The daemon collects finite Docker-selected `COPY`/`ADD` entries through `kernal-api`, copies their typed representation into an owner-private content-addressed setup asset tree, and Docker never receives the workspace as build context. A selected symlink is currently refused after safe target validation: `kernal-api` 0.1.14 exposes non-following observation/read but no public private-root symlink-creation facade. The smallest required upstream addition is a facade operation that creates a supplied relative link below a caller-owned private directory only after non-following destination/parent checks, with a portable directory/file target policy. Every external Dockerfile image must itself be digest-pinned; the refusal names the tag-only reference and the `FROM image:tag@sha256:<digest>` remedy. A nested Dockerfile (for example `dockerfile = 'bosn/act.Dockerfile'`) is materialized at the same relative path and built with `--file` that path and the workspace root as its context. |
| `kind = 'macos-x64-guest'` with explicit license acknowledgement | Supported only for `dockurr/macos` and Docker Hub aliases (`docker.io`, `index.docker.io`, `registry-1.docker.io`) pinned by a lowercase 64-hex `sha256` digest, with exactly `[stack.NAME.volumes.storage]` declared as `scope = 'machine'`, `destination = '/storage'`, and `retention = 'pinned'`. A tag-only reference and an image baked from a prepared disk and published elsewhere (for example ghcr.io) are refused; the prepared install lives on the pinned `/storage` volume, not the image. This prevents an ephemeral VM disk and prevents the fixed KVM/tun create shape from executing a manifest-selected privileged image. On Linux, kernal-api must report both `/dev/kvm` and `/dev/net/tun`. Bosn emits only the fixed dockurr KVM/tun/NET_ADMIN, loopback SSH/web-port, 120-second stop timeout, and sizing shape; it never accepts raw privilege/device/port arguments. The durable container resource uses the `manifest-guest:` namespace and rolls over conservatively like a manifest container. Guest SSH is fixed to `127.0.0.1`; non-loopback `guest.ssh_host` declarations are refused. See `docs/macos-guest.md`, whose example manifest is test-checked against this rule. |
| named `[stack.NAME.volumes]` | Supported for typed Bosn-managed named volumes. The daemon derives the engine name from the declared logical name, scope, canonical workspace, and (for `spec`) generation; callers cannot supply a Docker volume name or mount string. It writes a durable creation intent before `docker volume create`, requires exact ownership labels before reuse, then atomically records the resource and consumes the intent after container ensure. `spec` rolls with generation; `stack` survives generations within its workspace; `machine` follows the declared `family` or stack. Normal rollover never removes data. `manifest volume-gc` can collect only a retired native `spec` + `warm` volume. Durable `stack`/`machine` or `pinned` data remains outside GC and is removable only through confirmation-gated `manifest volume-release`: preview an opaque candidate token, then apply exactly that token. Apply repeats exact registry/use/lease/session/intent checks, two exact Docker-label-and-attachment inspections, and a fixed exact-name remove. |
| `tmpfs` | Supported only as an array of normalized legacy strings: `/target`, `/target:ro`, `/target:rw`, with at most one each of `size=POSITIVE{b,k,m,g}`, `exec` or `noexec`, and `mode=OCTAL` (1-4 octal digits, at most `7777`), for example `/mount-probe:rw,exec,mode=1777`. The daemon parses those into typed target/readonly/size/exec/mode values before its engine seam, incorporates the declaration into the runtime generation (exec/mode only when declared), and emits only its own finite `--tmpfs` form. Repeated options and all other options (`uid`, `nosuid`, etc.) are refused rather than passed through. tmpfs is disposable container state; a generation rollover creates a new empty tmpfs. |
| `[stack.NAME.mounts]` workspace bind mounts | Supported for existing paths that canonicalize beneath the selected workspace. Sources may be legacy absolute paths only when they resolve beneath that workspace; traversal, source symlinks, escapes, duplicate targets, reserved targets, and unrepresentable Docker paths are refused, and the refusal names the remedy (move the data under the workspace or declare a Bosn-managed volume). `readonly` is retained. |
| `[stack.NAME.mounts]` host Docker socket | A mount whose `source` is exactly `/var/run/docker.sock` or `/run/docker.sock` is accepted as a typed host-Docker-socket bind (at most one per stack, Linux stacks only, and only when that host path is currently a Unix socket). This is the manifest author's explicit choice to let the container drive the host engine, for example to run `act`, whose job containers are siblings. **Everything created through the socket is outside Bosn supervision**: it carries no Bosn labels, is not in the registry, and is never garbage-collected or protected by Bosn. Bosn supervises only the stack's own container. No other host path is accepted. |
| Linux container PID 1 | A fixed daemon-owned idle process (`sleep` loop that exits on `docker stop`), not the image's default command: declared tasks run through `docker exec`, and a base image such as `debian` defaults to a shell that would exit at once. This matches the legacy runtime. The command is part of the runtime generation. |
| `workdir` | Linux: supported only when its normalized absolute container path is covered by a declared workspace bind; it is translated to the typed workspace-relative form and inherited by declared `manifest app-task` exec. Guest: a normalized absolute VM path is supported only for typed SSH app tasks and is safely shell-quoted before the declared command. |
| `[task.NAME] secrets` | Supported for the one name `github_token`, injected per exec as `GITHUB_TOKEN` from daemon state; see [task-secrets.md](task-secrets.md). |
| `[task.NAME] github_api` | Only `"proxy"`: a per-run, loopback, read-only GitHub API proxy is injected as `GITHUB_API_URL`; the credential stays in the daemon. See [github-api-proxy.md](github-api-proxy.md). |
| image tags or unpinned image references | Refused |
| generic `run`, shell, arbitrary Docker arguments | Not exposed |
| all-stack orchestration | Supported by `manifest converge`: every declared stack in lexical order, one at a time, with per-stack durable records and partial-success semantics. The TOML model has no dependencies/root selector; dependency spellings fail closed rather than being guessed. |
| replacement/rollover | Supported for this accepted subset; a new immutable generation is ensured first, then only the same-workspace/stack prior manifest container is registry-retired |

Guest task authentication is daemon-owned but not auto-provisioned: before the
first guest task, install one private key at `STATE_DIR/guest-ssh/id_ed25519`
with no group/world permissions, and install its public key for the declared
guest user during the guest's one-time manual bootstrap. The daemon refuses
missing, non-regular, symlinked, or overly permissive identity files. This
avoids passwords, SSH agents, user config, and manifest-selected secret paths.
For a declared `guest.payload`, native guest tasks re-prove one regular,
non-symlinked workspace-relative file immediately before the task and copy it
through the fixed loopback OpenSSH SCP channel. The source is limited to 4 GiB;
the destination must be a normalized absolute guest path or a normalized `~/`
path. SCP receives the same daemon-state identity, manifest-derived user and
published port, and config/agent isolation as SSH. SCP failure, cancellation,
deadline, or output-limit failure stops before the task session begins, so Bosn
never executes a task against a stale declared payload.

SSH exit status 255 and local client cancellation/deadline/output failures
after task launch are recorded as uncertain because they cannot prove whether
the VM command completed. Ordinary nonzero SSH exit statuses are recorded as
known task failures. A failed readiness probe happens before a task session is
created, because the declared task has not started.

The deterministic engine container is still checked against its exact derived
image and labels before reuse. An occupied mismatched candidate fails closed.
The runtime generation incorporates the accepted effective bind/workdir, named
volume, and tmpfs shape
in addition to the historical manifest generation, so a bind target/source/
readonly or workdir change cannot reuse a container configured for the prior
lifecycle declaration. Before each ensure and manifest app-task operation the
daemon re-reads the manifest; the typed setup primitive then canonicalizes the
same mount sources again immediately before its engine operation.

## Daemon-start recovery

After a successful native `manifest ensure` or `manifest converge` member, the
same registry transaction records a bounded daemon-written recovery contract:
the canonical workspace, safe relative manifest spelling, stack, runtime
generation, deterministic container name, observed image identity, guest kind,
and immutable successful-ensure intent ID.

The legacy TOML schema has no `autostart` field. Native Bosn therefore does not
invent a client flag or reinterpret an arbitrary stack as background work. Its
only startup selection is the existing document-level `default` rule: the one
`default = true` stack, or the implicit sole stack in a one-stack document.
Other stacks can be explicitly ensured and have tasks run, but never become
daemon-start candidates. The selected/default decision is captured only after
that stack's ensure succeeds. A later successful selected ensure gets a new
intent ID even if its generation is unchanged.

On its next start, before accepting requests, the daemon reads only those
contracts (never a Docker listing), deduplicates them by resource ID, and for a
selected contract:

1. re-reads and re-derives the current manifest stack, including Dockerfile
   context, mounts, volumes, tmpfs, and guest checks;
2. requires the generation and guest kind to remain exact, then requires one
   active native-manifest registry resource/use with no execution session and
   no unresolved volume-creation intent for that workspace/stack;
3. inspects only the deterministic name and requires exact managed/content/
   container labels and the recorded image ID; and
4. starts only a stopped matching container, then re-inspects it to prove it
   is running.

Missing manifests, changed sources, unavailable Dockerfile context, a stack
that is no longer the default, stale or retired registry facts, uncertain
app-task sessions, pending volume intents, missing containers, malformed
contracts, label/image/name drift, engine errors, and the fixed 20-second
startup recovery deadline all fail closed. Source drift/removal or policy-off
records a durable exact `manifest.autostart.disabled` veto for that successful
intent; later daemon starts consult it before even reopening the old source. It
is never cleared implicitly: a new successful selected ensure is the deliberate
re-enable path. They neither adopt, create, delete, stop, or replace an engine
object. Recovery outcomes and intent/veto facts are durably appended as bounded
`manifest.recovery.*` / `manifest.autostart.*` events and are observable via the
existing authenticated `bosn registry setup-ensure-events` CLI/Python/MCP
diagnostic page. Dockerfile materialization during source proof remains inside
owner-private Bosn state; a missing source is never replaced from the cache.

## Remaining work

This does not claim completion of manifest migration. Future slices must add
selected symlink materialization
once kernal-api supplies its safe private-root link-creation facade,
dependency syntax/ordering (if the legacy TOML schema gains an explicit relation), guest SSH/SCP task lifecycle, declarative tasks,
an explicit manifest autostart field if a policy beyond default-stack selection
is required, and generation reconciliation. Each must
remain daemon-owned and registry-backed rather than reintroducing raw Docker or
Python business logic.
