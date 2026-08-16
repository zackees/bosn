# soldr integration (issue #75)

This records what `examples/soldr.toml` proves, what it does not, and the one part of #75
that was deliberately left unvalidated.

All destinations, scopes, ENV, and `WORKDIR` claims below were read directly from soldr's
own source on this machine (`ci/perf_local.py`, `docker/cook-shared-cache/Dockerfile`) and
bosn's own (`src/bosn/manifest.py`, `src/bosn/converge.py`), not inferred or guessed.

## Which option this manifest represents

Issue #75 originally framed the choice as three options for governing soldr's `ci/perf_local.py`
workload, two of them blocked at the time:

- **(a)** rewrite soldr's Dockerfile to `COPY` the source instead of bind-mounting it —
  rejected in the issue itself: that would force a rebuild on every edit, defeating the
  point of the edit-in-place loop.
- **(b)** bosn grows a declared workspace bind mount, and governs the whole stack —
  blocked at the time by two conflicts (see below).
- **(c)** bosn governs only soldr's cache *volumes*; `perf_local.py` keeps owning the
  container and the bind mount.

Both blockers on (b) were resolved before this work started (`MountSpec`/`bosn.toml`
`[stack.X.mounts]` for binds, `VolumeSpec.destination` for fixed image paths — see
`src/bosn/manifest.py`). `examples/soldr.toml` is option **(b)**: a single bosn stack with
`[stack.perf.mounts]` and `[stack.perf.volumes]` sections that together express all six of
`perf_local.py`'s mounts, not just the volumes.

## What it does govern

Loaded through the real loader (`bosn.manifest.load`), `examples/soldr.toml`'s `perf`
stack expresses exactly the six *mounts* `Runner.volumes` / `create_command()` wire in
`ci/perf_local.py`, plus, since #105, the two container-create-time knobs that used to
have no bosn.toml equivalent at all — `CARGO_TARGET_DIR` via `[stack.perf.env]` and the
working directory via `workdir = "/repo"` (see "What it does not govern" below for what is
still left over, which is narrower than it used to be).

| Host (soldr) | Container | soldr wires it by | bosn expresses it as |
| --- | --- | --- | --- |
| `{source_root}` | `/repo` | `-v {source_root}:/repo` | `MountSpec` (bind) |
| `{target}` | `/target` | `-v {target}:/target` + `-e CARGO_TARGET_DIR=/target` | `VolumeSpec`, `scope = "stack"` |
| `{cargo_home}` | `/root/.cargo` | `-v {cargo_home}:/root/.cargo`; `ENV CARGO_HOME` baked into the image | `VolumeSpec`, `scope = "machine"` |
| `{soldr_home}` | `/root/.soldr` | `-v {soldr_home}:/root/.soldr` | `VolumeSpec`, `scope = "machine"` |
| `{uv_cache}` | `/root/.cache/uv` | `-v {uv_cache}:/root/.cache/uv`; `ENV UV_CACHE_DIR` baked into the image | `VolumeSpec`, `scope = "machine"` |
| `{venv}` | `/venv` | `-v {venv}:/venv`; `ENV UV_PROJECT_ENVIRONMENT` baked into the image | `VolumeSpec`, `scope = "stack"` |

`tests/test_manifest.py` (the `# -- examples/soldr.toml expresses soldr's actual
perf-local mount table (#75) --` section) loads this file and pins every destination and
scope in that table, plus: the bind is a `MountSpec` (never registered, labeled, or GC'd —
see `MountSpec`'s docstring) and never a `VolumeSpec`; none of the six destinations fall
inside bosn's reserved `/bosn/*` / heartbeat namespace; and the reserved-namespace guard
still refuses a destination that does.

### The scope split, and the README's number

This repo's `README.md` ("How the disk savings actually work" → "1. Sharing") claims that
applying bosn's scopes to soldr's own five volumes drops five per-checkout volumes to two
— `cargo-home`, `soldr-home`, `uv-cache` machine-scoped; `target`, `venv` stack-scoped —
and that ten checkouts go from 50 volumes to 23. `examples/soldr.toml` **agrees with**
that split, and it is not a coincidence: `cargo-home`/`soldr-home`/`uv-cache` hold
content that does not depend on which repo asked for it (a cargo registry, a uv wheel
cache, soldr's own state dir), while `target`/`venv` are built from one checkout's source
and would corrupt a sibling checkout's build if shared. `tests/test_manifest.py` pins
this scope assignment against the checked-in file, so an edit to either the manifest or
the claim that breaks the correspondence fails a test.

One nuance worth stating plainly, because it's easy to read the README's number as "bosn
discovered a place soldr was wasting volumes" when the real story is narrower: soldr's own
`shared_source_root()` (in `ci/perf_local.py`) already collapses every *linked* `git
worktree` under one checkout root onto a single `Runner` and its five volumes — sibling
worktrees of the *same* repo never get their own copies today (`RUNNER_SCHEMA` "7"; the
docstring is explicit that this replaced an older, worse scheme). The 50→23 scenario is
ten separate *checkout roots* (ten clones, or ten repos with their own `bosn.toml`), each
of which is its own `Runner` under soldr's current code and its own `workspace` under
bosn's (`workspace_of(manifest)`, keyed off the manifest's root — see
`src/bosn/converge.py`). Machine-scoping three of the five volumes is a real win *bosn's*
scoping model adds on top of soldr's own per-root sharing, not a bug bosn is fixing in
soldr — soldr never claimed to share `cargo-home` across sibling clones, and it still
doesn't on its own.

## What it does not govern

**Historical note (resolved by #105): `bosn.toml` used to be unable to declare container
environment or a working directory at all.** `src/bosn/manifest.py` had no `env` table or
`workdir` key, and neither `converge.py`'s `docker create` nor its `docker exec` passed
`-e`/`-w`. Everything a container got on either axis came from what its *image* baked in
via `ENV` / `WORKDIR` — which is exactly why bosn's own dogfood image
(`docker/test.Dockerfile`) sets `WORKDIR /repo` explicitly, line 17. soldr's image does
not need to, because `perf_local.py` supplies both axes itself at container-create time
(`create_command()` in `ci/perf_local.py`): `-w /repo`, plus eight `-e` flags —
`CARGO_TARGET_DIR=/target`, `TMPDIR=/target/tmp`, `CARGO_BUILD_JOBS=2`, `SOLDR_JOBS=2`,
`UV_CACHE_DIR=/root/.cache/uv`, `UV_PROJECT_ENVIRONMENT=/venv`, `NEXTEST_TEST_THREADS=2`
(the last three duplicate what the Dockerfile already bakes in as `ENV`, harmlessly).
Checked against `docker/cook-shared-cache/Dockerfile` line by line (not just grepped —
the file's `ENV` blocks span backslash-continued lines that a naive grep for `ENV` would
miss part of):

| create-time flag | baked into the image's own `ENV`? | consequence if bosn supplies neither | bosn's #105 fix |
| --- | --- | --- | --- |
| `CARGO_HOME` | yes (line 86) | none — the volume above lands correctly regardless | not needed |
| `UV_CACHE_DIR`, `UV_PROJECT_ENVIRONMENT` | yes (lines 133–134) | none, same reason | not needed |
| `CARGO_BUILD_JOBS`, `SOLDR_JOBS` | yes (lines 91–92) | none, same reason | not needed |
| `CARGO_TARGET_DIR` | **no** | **breaks cache correctness**: Cargo falls back to `<cwd>/target`, not the `/target` volume | closed — `[stack.perf.env]` in `examples/soldr.toml` |
| `TMPDIR` | no | smoke-suite temp binaries fall back to the container overlay instead of the target volume (perf, not correctness) | still open (see below) |
| `NEXTEST_TEST_THREADS` | no | nextest picks its own default thread count instead of 2 (perf, not correctness) | still open (see below) |
| working directory (`-w /repo`) | no (image `WORKDIR` is `/work`, an empty dir) | any command that assumes it starts inside the repo fails outright | closed — `workdir = "/repo"` in `examples/soldr.toml` |

`src/bosn/manifest.py` now has a `[stack.X.env]` table (validated: an empty key or a key
containing `=` is a manifest error) and a `workdir` key on the stack (validated: must be an
absolute path, sharing its validator with `MountSpec`/`VolumeSpec`'s destination check).
`converge.py`'s `docker create` now emits `--env KEY=VALUE` for every declared entry, and
its `docker exec` (both the task path and the interactive shell path) now emits `--workdir`
when one is declared. `examples/soldr.toml` uses both: `[stack.perf.env]` declares
`CARGO_TARGET_DIR = "/target"`, and `workdir = "/repo"` replaced `[task.check]`'s old
`cd /repo &&` workaround with a real working directory. That workaround was never enough
on its own — `cd` gets `cargo check` running in the right place but cannot set
`CARGO_TARGET_DIR`, so the task used to run successfully while still quietly building into
`/repo/target` on the bind mount instead of the `/target` volume. Both halves of that gap
are closed now.

Two things remain genuinely open, and are unrelated to #105's fix:

- `TMPDIR`, `CARGO_BUILD_JOBS`, `SOLDR_JOBS`, `NEXTEST_TEST_THREADS` are not declared in
  `examples/soldr.toml`'s `env` table. All four are perf-only (see table above), not
  correctness gaps like `CARGO_TARGET_DIR` was, so leaving them out is a choice, not an
  oversight — the reference manifest declares only what changes correctness.
- The issue's own "suggested direction" also raised a speculative idea: bosn could warn
  when a well-known cache-path env var (`CARGO_TARGET_DIR` and friends) points somewhere no
  declared mount destination covers, which is really what would have caught soldr's
  original gap structurally instead of by manual line-by-line audit. That is explicitly out
  of scope for #105 (needs its own design for what "well-known" means and how false
  positives are avoided) and is not implemented here.

**Adoption was not exercised against real soldr volumes.** #75 also asked to run
`bosn adopt --legacy soldr --yes` against real soldr cache volumes and measure the result.
That mutates real, potentially large volumes in place (Docker labels are immutable, so
adoption is a staged copy — recreate under new labels, needing roughly double the volume's
size in free space; see `src/bosn/legacy.py`'s module docstring and README's "Coming from
Docker" section) and needs the user's explicit go-ahead, which was not given for this
pass. What *was* exercised instead: `tests/test_legacy.py` already builds synthetic
volumes carrying soldr's exact real label shape — `soldr_volume_labels()` stamps only
`io.soldr.perf-local.managed` and `.source-root`, matching `ensure_runner`'s actual
`docker volume create --label ... --label ...` call in `ci/perf_local.py` byte-for-byte —
and adopts them through `legacy.plan_adoption`/`apply_plan` against a fake engine
(`test_all_five_real_soldr_volume_names_from_one_root_adopt_to_one_workspace`,
`test_two_checkout_roots_map_to_two_distinct_workspaces_not_one`, and the `apply_plan`
tests below them). That is real coverage of the *mapping* logic — label→workspace, one
root's five volumes landing in one workspace, two roots never colliding — without touching
a real Docker daemon or a real cache. It is not a substitute for the real-volume run;
nothing here claims otherwise.

## The issue's open item about `RUSTUP_HOME`/cargo-chef — already resolved

Issue #75's last open item claimed the README's soldr paragraph names `RUSTUP_HOME` and
cargo-chef, which do not appear in `Runner.volumes`. Re-checked directly against the
README text at the paragraph that actually makes the soldr claim ("How the disk savings
actually work" → "1. Sharing," the "Applied to soldr's own five..." paragraph): it lists
exactly `target`, `cargo-home`, `soldr-home`, `uv-cache`, `venv` — a byte-for-byte match
to `Runner.volumes` in `ci/perf_local.py`, with no mention of `RUSTUP_HOME` or cargo-chef
anywhere near it. `RUSTUP_HOME` and a `chef` volume appear only in the README's *generic*
"First run" Rust example (a made-up `family = "rust"` stack, unrelated to soldr) and its
`rustup toolchain is byte-identical...` sentence a few lines above the soldr paragraph,
which is about rustup toolchains in general, not soldr's specific five. So this item is
already resolved — no further edit was needed here.

## `SOLDR.recognized_schema_versions` — re-checked, still correct

`src/bosn/legacy.py`'s `SOLDR` family accepts only schema `"2"` even though soldr's
`RUNNER_SCHEMA` constant now reads `"7"` (`ci/perf_local.py`, checked on this machine).
Read against the actual producer code, that mismatch is inert, exactly as the surrounding
comment in `legacy.py` claims:

- `ensure_runner()`'s `docker volume create` call passes only two labels —
  `io.soldr.perf-local.managed=true` and `io.soldr.perf-local.source-root=...` — never
  `.schema`. `.schema` (`expected_labels()`) is stamped only on the *runner container*.
- `legacy.py` only ever adopts **volumes** (containers/images are reported and skipped —
  Docker labels are immutable, so relabeling either would mean destroy-and-rebuild).
- So `SOLDR.map_labels()`'s schema check never actually runs against a real soldr object:
  the label it validates is never present on the only kind it is ever asked to validate.
  `recognized_schema_versions=frozenset({"2"})` is accurate about a boundary that soldr's
  *current* code never crosses, not stale.

This still holds. If soldr ever starts stamping `.schema` on its volumes, adoption starts
refusing them with "unrecognized schema" until someone audits what changed between "2" and
"7" and widens the set — which `legacy.py`'s own comment already documents as the intended
failure mode. Nothing here contradicts that; no code change was needed.

## Files

- `examples/soldr.toml` — the reference manifest itself, meant to be copied to
  `<soldr-checkout>/bosn.toml`.
- `tests/test_manifest.py` — loads it through the real loader and pins destinations,
  scopes, and the bind/volume distinction (see the `#75` section near the end of the file).
- `tests/test_legacy.py` — pre-existing synthetic-volume adoption coverage for soldr's
  exact label shape (not modified for this pass; see "What it does not govern" above).
