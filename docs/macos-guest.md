# The macOS x86-64 guest stack

A guest stack runs a [`dockurr/macos`](https://github.com/dockur/macos) container whose real
workload executes in a QEMU/KVM virtual machine *inside* it. It lets a repo declare a macOS
x86-64 target the same way it declares a Linux one, and puts that guest under the same
lifecycle bosn already gives every other container resource.

```toml
[stack.macos-x64]
kind = "macos-x64-guest"
acknowledge_macos_license = true          # see Licensing, below — no default
image = "ghcr.io/<org>/<repo>/macos-x64-guest:ventura"
family = "macos-x64"
workdir = "/Users/runner"

[stack.macos-x64.guest]
ssh_port = 2222
ssh_user = "runner"
ready_timeout = 1800
payload = "kernal-x64.tar.zst"            # scp'd into the guest before every task
payload_destination = "~/kernal-x64.tar.zst"

[stack.macos-x64.volumes]
storage = { scope = "machine", destination = "/storage", retention = "pinned" }

[task.test-macos-x64]
stack = "macos-x64"
cmd = "cargo-nextest nextest run --archive-file ~/kernal-x64.tar.zst"
```

## Why this is a stack *kind* and not a stack with unusual options

Four things a guest stack needs that no Linux stack does. Each one is a place bosn's normal
behavior would be silently wrong rather than merely absent.

**Device passthrough at create time.** `--device /dev/kvm --device /dev/net/tun --cap-add
NET_ADMIN`. Without KVM the guest emulates and is unusably slow; without the tun device it
has no network, so no sshd, so no way in. Docker cannot add a device or a capability to a
container after creation, which is why every one of these — and everything in
`[stack.X.guest]` — is part of the generation digest: changing one has to roll a new
container, exactly like `env` does.

**Readiness is sshd, not "container running".** The container is up in a second. macOS is
booted minutes later, and on one core, later than that. bosn polls the guest's sshd against
a bounded deadline (`ready_timeout`, default 1800s) and, if it passes, attaches the last 60
lines of the guest's own logs to the error. A fixed sleep is wrong in both directions: too
short and the task fails against a guest that was about to be ready, too long and every run
pays for the worst case.

**The execution transport is ssh.** `docker exec` lands in the container, *beside* the VM,
and would quietly run a macOS task against a Linux userland. Tasks are shipped over ssh and
the guest's exit code comes back unchanged, so CI can gate on it. One ambiguity is worth
knowing: ssh uses exit 255 for its own connection failures, so a task that genuinely exits
255 is indistinguishable from a lost connection. bosn logs a `guest.ambiguous_exit` event
when it sees one.

**A bind mount is invisible to the guest.** A Linux stack binds `.` read-only into `/repo`.
The VM has no view of the container's filesystem, so bosn *refuses* `[stack.X.mounts]` on a
guest stack rather than accepting a declaration that would silently not be there. Instead,
declare a `payload` in `[stack.X.guest]`: one repo-relative file, `scp`'d to
`payload_destination` over the same ssh channel before every task. Before *every* task, not
once at container creation — a payload is a build output that changes between runs, and a
guest quietly serving last week's archive while reporting success is the worst failure this
kind can produce. A missing payload file is refused before the task starts.

## Pinned volumes

The guest disk is tens of gigabytes and its only creation path is a human sitting through a
30–60 minute interactive installer. It is not a cache that rebuilds on demand, and a GC
sweep that reclaims it costs an hour of someone's day.

```toml
storage = { scope = "machine", destination = "/storage", retention = "pinned" }
```

A pinned volume is exempt from **every** automatic rule: age, supersession, `bosn done`, and
storage pressure. Only an active lease outranks it, and that is not a reclaim decision. The
tier is recorded on the volume's registry row rather than looked up in the manifest, because
GC runs from the registry alone — it has no manifest in hand, and the worktree that declared
the volume may be long gone. It is *also* written as a Docker label, because bosn's registry
is explicitly disposable ("ownership lives in the Docker labels"): without the label, a
`bosn adopt` after a lost database would rebuild the guest disk's row as warm and the next
sweep would take it.

Releasing one is deliberate and manual:

```bash
bosn release-volume --stack macos-x64 --volume storage            # preview
bosn release-volume --stack macos-x64 --volume storage --apply --yes
```

The release re-proves ownership from the engine's labels, refuses an active lease, and
refuses a volume still attached to a container — the same three proofs GC requires.

## Recommended shape: build outside, execute inside

This removes almost all of the guest's setup burden. Cross-compile on Linux; ship only
prebuilt binaries into the guest.

```bash
# Linux stack — soldr carries its own macOS SDK and LLVM, so no Mac is involved
soldr cargo nextest archive --target x86_64-apple-darwin --all-features \
  --archive-file kernal-x64.tar.zst        # measured: 25 binaries, 103 files, ~112 MB, ~85 s

# macOS guest stack
cargo-nextest nextest run --archive-file kernal-x64.tar.zst
```

The guest then needs **no Rust toolchain, no Xcode CLT, no Homebrew** — only the
`cargo-nextest` binary.

Two portability traps, both invisible until you actually run cross-host:

- `cargo-nextest` invoked directly needs `cargo-nextest nextest run`. As `cargo-nextest run`
  it parses `run` as a cargo subcommand and exits 2.
- `env!("CARGO_MANIFEST_DIR")` and `env!("CARGO_BIN_EXE_<name>")` are **compile-time**
  constants, so they carry the *builder's* paths. nextest's `--workspace-remap` cannot
  rewrite them; `NEXTEST_BIN_EXE_<name>` is the runtime replacement for the binary case.

## Constraints bosn surfaces rather than hides

**AMD hosts get one core.** [dockur/macos#268](https://github.com/dockur/macos/issues/268):
a PCID mismatch makes a multi-core guest unstable on AMD. bosn reads `/proc/cpuinfo` at
create time and forces `CPU_CORES=1` on an AMD host, and also on a host whose vendor it
cannot read — guessing wrong toward AMD costs speed, guessing wrong toward Intel costs
stability. An explicit `cpu_cores` in `[stack.X.guest]` is honored as written.

**Bootstrap is manual and one-time.** `dockurr/macos` has no unattended install path, and
Docker-OSX's prebuilt `:auto` tag no longer exists on Docker Hub. bosn supervises the
*result*; it cannot automate the install. The realistic flow is: install once by hand → bake
the prepared disk into an image → publish it → every consumer pulls. See "One-time
bootstrap" below.

**One guest per machine.** The container name is per-workspace, but `ssh_port` is a fixed
host port, so a second workspace declaring the same guest stack will fail to start on
port-in-use. That is the intended shape rather than a limitation to route around: the guest
disk is `scope = "machine"`, and two VMs writing one disk would corrupt it. Give a second
guest its own `ssh_port` and `web_port` — and its own storage volume — if you really need
two.

**Linux hosts only.** A guest stack is refused before anything is pulled or built if the
host is not Linux or is missing `/dev/kvm` or `/dev/net/tun`. Failing at converge time,
rather than several layers down inside QEMU after a multi-gigabyte pull, is the point.

## Licensing

Apple's EULA conditions macOS on the hardware it runs on, and building an image from a
prepared disk does not change that analysis. Running macOS under QEMU on non-Apple hardware
is a decision for the person doing it, not something to back into by copying somebody's
`bosn.toml`. So a guest stack is inert until the manifest says so explicitly:

```toml
acknowledge_macos_license = true
```

Without it, loading the manifest fails with an explanation. There is no default, no
environment variable, and no flag.

## One-time bootstrap

bosn does not automate this; it is here so the manifest above has something to point at.

1. **Install macOS (~30–60 min, one core).** Start a `dockurr/macos` container with
   `-p 8006:8006 -p 2222:22`, the two devices and `NET_ADMIN`, and a `/storage` volume. Open
   `http://localhost:8006`: Disk Utility → erase the QEMU disk as APFS → Reinstall macOS →
   walk the setup screens. Skip Apple ID; create a local account named `runner` (or set
   `ssh_user` to whatever you chose).
2. **Enable ssh in the guest.** System Settings → General → Sharing → Remote Login: on.
3. **Install whatever the task needs.** For the recommended shape above that is one binary:
   `scp -P 2222 cargo-nextest runner@localhost:~/` then move it to `/usr/local/bin`.
4. **Bake the prepared disk into an image** with a Dockerfile that is `FROM
   dockurr/macos:latest` plus `COPY storage/ /storage/`, and push it. Stop the container
   first so the disk is quiescent — a baked image of a live filesystem is a torn one.
5. Point the stack's `image` at the published tag. Every consumer now pulls a guest that
   boots straight to a login-capable system.

Working reference scripts for all five steps live in `zackees/kernal-api` under
`ci/macos-x64/`.
