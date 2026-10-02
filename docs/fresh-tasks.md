# Fresh task containers (`fresh = true`)

A manifest task normally runs through `docker exec` in its stack's long-lived
container. That is what makes a build loop fast: the toolchain, `target/` and
everything else the last run left behind is still there.

It is also a trap when a task must start from the image every time. An
experiment that rewires the root filesystem (links `/lib64`, rewrites
`/usr/lib`, installs a tool) leaves that state for the next task, and one bad
run can poison every later one. In one Nix experiment, a task wrote a wrapper
through a symlink a previous task had left at `/sbin/ldconfig`. That replaced
the image's own `ldconfig` in `/nix/store` with a script that called itself,
and every later run hung.

## Declare it

```toml
[task.desktop]
cmd = "MODE=desktop bash /exp/corpus.sh"
fresh = true
```

`fresh` is a boolean task key, `false` by default. A fresh task runs in a new
`docker run --rm` container:

- **Same image and runtime shape as the stack.** Its binds, named volumes,
  tmpfs, environment and working directory are derived from the same plan as
  the stack's own container, byte for byte.
- **Named volumes persist.** They are the stack's declared state: caches,
  downloads, toolchains. Everything else the task writes is gone when it
  ends.
- **The stack's container is still ensured first.** It owns the named volumes,
  and keeps them leased while a fresh task uses them.
- **Same budgets, secrets and receipts as an exec task.** `secrets` and
  `github_api = "proxy"` work unchanged, and the run is recorded the same way.
- **Cancellation removes the container.** If the `docker run` client ends
  without the task's exit status (a cancel or a deadline), Bosn force-removes
  the container (`bosn-task-<token>`). A confirmed removal is reported as
  stopped.

Fresh tasks are refused for macOS guest stacks.

Use `fresh` when every run must be independent: experiments, installers,
tests of system layout. Leave it off for build loops that rely on warm state
outside a volume.

## Example: Nix and FHS experiments

[`examples/nix-fhs`](../examples/nix-fhs) runs unmodified foreign binaries on a
NixOS-shaped root:

- a Node.js tarball;
- uv's CPython and a numpy wheel;
- rustup and `cargo run`;
- an AppImage;
- Playwright Chromium.

It runs them once per FHS layer, each built from the real NixOS `nix-ld`
module.

- **The image** is `nixos/nix` pinned by digest. Like stock NixOS, it has
  only `/bin/sh` and `/usr/bin/env`.
- **The layers** are built into the image from one pinned nixpkgs revision
  (`layers.nix`), so they come from `cache.nixos.org` and are fixed for the
  image's lifetime.
- **Each mode is a fresh task.** Every mode links `/lib64`, `/usr/lib` and
  `/sbin/ldconfig` differently, and none sees another's links.
- **Downloads go in the machine-scoped `cache` volume** (tarballs, uv
  interpreters, rustup, browsers), so only the first run pays for them.

```bash
cd examples/nix-fhs
bosn run --task baseline   # every foreign binary fails: no loader
bosn run --task desktop    # FHS layer + desktop libraries
```

Each check prints one `RESULT <mode> <check> PASS|FAIL <detail>` line.
