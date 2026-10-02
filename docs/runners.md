# Concurrent runners, accounting, and act job caches

Issue [#358](https://github.com/zackees/bosn/issues/358). The daemon used to run
one job at a time on the whole machine (`Jobs::new(1)`), so one session's
`bosn run --task act-ci-*` queued every other session behind it, including
their seconds-long stack ensures. This document describes what replaced that:

- runner slots that run task jobs in parallel, each with a CPU limit;
- one daemon-owned record of everything running, with stall teardown and
  reaping after a daemon restart;
- a per-job Docker proxy, so the sibling containers a task starts (act's job
  containers) are labelled, limited, isolated from other runs, and removed
  afterwards;
- cache volumes that follow act's jobs, with defined concurrency semantics.

## Scheduling

#12's per-key policy is unchanged: per `(workspace, stack)` at most one job
runs and one waits, an identical digest joins the running job, and a
different digest supersedes the waiting one.

Distinct keys run in parallel, in two lanes:

| Lane | Jobs | Default size |
|---|---|---|
| runner | task jobs: `bosn run --task`, manifest/setup app tasks, setup tasks | `max(1, ncpu * 2)` slots |
| control | ensure, prepare, converge | `max(2, ncpu / 2)` (#12's build cap) |

`ncpu` is `std::thread::available_parallelism()`, which already honours cgroup
CPU quotas and affinity masks. If it cannot be read, `ncpu` is 1. On a
16-CPU host that gives 32 runner slots and 8 control slots.

- **Separate lanes.** A stack ensure never waits behind long tasks: with every
  runner slot busy, another checkout's ensure still finishes at once, and only
  its task queues, with `bosn run` saying so.
- **Fair admission.** Each lane is round-robin across workspaces, not FIFO,
  so a checkout that queues ten jobs cannot starve another checkout's one.
- **Slots.** A running task holds the lowest free slot index until it settles.
- **Dead followers.** A queued job whose follower has gone (#357 lease
  expired) is cancelled, never started. A follower that has not polled for
  half its lease holds its job back, and the slot goes to the next job,
  until the follower polls again. A live `bosn run` polls every 200 ms
  against a 30 s lease.

### CPU per slot, and oversubscription

Each runner slot has a CPU limit, 4 by default. It is applied as a CFS quota
(`docker update --cpus`) to the task's setup container, and through the
Docker proxy (below) to every container the task creates. Memory is unlimited
unless configured.

Slots and CPUs per slot are independent, and their product may exceed the
host. The default does: 32 slots × 4 CPUs is 128 CPUs of quota on 16 CPUs.
**That oversubscription is intended.** A quota is a ceiling, not a
reservation: idle slots cost nothing, and a busy machine shares its CPUs
between running jobs instead of queueing them. The per-slot quota is what
stops one job (one `cargo build -j16` inside act) from taking every core.
To get strict partitioning instead, choose `slots * cpus <= ncpu`, for example
`slots = 4` and `cpus = 4` on 16 CPUs.

Tasks see their allocation as `BOSN_RUNNER_CPUS`, `BOSN_RUNNER_SLOT`,
`BOSN_JOB_ID` and `BOSN_RUN` (for example, to size `cargo -j`).

### Configuration

Highest precedence first:

1. `bosn daemon serve` flags: `--runner-slots N`, `--runner-cpus CPUS`
   (0 = no limit), `--runner-memory SIZE` (`8g`, `512m`; 0 = no limit),
   `--control-slots N`, `--stall-seconds S` (0 = off), and
   `--docker-proxy true|false`.
2. Environment: `BOSN_RUNNER_SLOTS`, `BOSN_RUNNER_CPUS`, `BOSN_RUNNER_MEMORY`,
   `BOSN_CONTROL_SLOTS`, `BOSN_STALL_SECONDS`, `BOSN_DOCKER_PROXY`.
3. `<state-dir>/runners.toml`, the same keys without the prefix:

   ```toml
   slots = 8
   cpus = 4
   memory = "8g"
   stall_seconds = 1800
   ```

   The file is the reliable choice for an autostarted daemon, which inherits
   no flags.

The daemon logs its effective capacity at start
(`bosn runners: 32 runner slots x 4 CPUs, ...`). `bosn jobs` shows it too.

## Accounting: `bosn jobs`

```
$ bosn jobs
runner slots: 32 x 4 CPUs (host 16 CPUs), control slots: 8, stall teardown: 1800s, docker proxy: on
 runner: 2 running, 0 queued
control: 0 running, 0 queued
   ID  STATE       LANE     SLOT      AGE    IDLE  KEY                           WORKSPACE / CONTAINERS
    5  running     runner      1      13s      0s  manifest-app-task:ci          /home/me/dev/repo-a
       task act-ci in bosn-setup-v2-60…; run e65…-5; docker: 64 requests, 1 creates; live containers: [9c6296abbf42]; caches: bosn-cache-m-act-toolcache-1 …
```

- **Commands.** `bosn jobs --json` prints the same document; the MCP tool
  `bosn_jobs` returns it to agents.
- **Contents.** Every queued and running job, plus the 20 most recently
  finished, each with its lane, slot, age and idle time (since its last log
  line or Docker request). A running task also shows its setup container, run
  label, CPU allocation, Docker proxy traffic, leased cache volumes, and the
  containers that carry its run label right now.

The runs are also written to `<state-dir>/runners/active.json` on every change.

### Daemon restart

A daemon that dies takes its jobs' output streams and client leases with it,
so a new daemon cannot re-adopt them. Before it accepts any work, it **reaps**
each run in the ledger:

1. It stops the task's processes inside the recorded setup container. These
   are every process except the container's idle PID 1, which is safe because
   one task job runs per setup container at a time and nothing runs yet.
2. It removes every container, network and volume labelled with that run.

It then sweeps objects carrying this daemon's label (`com.zackees.bosn.daemon`,
derived from the state directory) whose run is not live. Nothing labelled by
another daemon or another run is ever touched. Each reap is one stderr line:

```
bosn runners: reaped orphaned job 8 (ci stall in /…/act-c): signalled 2 task process(es), 0 still running; removed 1 container(s), 0 network(s), 0 volume(s)
```

### Stall detection

A running task with **no output and no Docker activity** for `stall_seconds`
(default 1800) is cancelled. "No Docker activity" means no byte in either
direction through its proxy: act streaming a step's output counts as activity.

Cancellation stops the in-container process tree (#363), and teardown
(below) removes what the task created. The job log, and the daemon's
stderr, say why:

```
[bosn] stalled: no output and no Docker activity for 15s; tearing down this job (stall_seconds)
```

This is separate from the #357 follow lease, which cancels a job, queued or
running, whose `bosn run` client stopped polling.

## The Docker proxy

A stack that mounts the host Docker socket (act's `clud_act`, for example)
lets its task create sibling containers outside Bosn's registry. For such a
task, on Linux, the daemon starts a per-job proxy on a private Unix socket. It
hands the task only `DOCKER_HOST=unix://<socket>`.

The socket lives in a directory that Bosn binds into the setup container at
its own host path: `<state-dir>/dp`, or `/tmp/bosn-<uid>/dp-<hash>` when that
path would not fit `sun_path`. When act bind-mounts "its" Docker socket into
a job container, that container therefore talks to the same proxy.

The proxy forwards bytes untouched, hijacked `attach`/`exec` streams included,
except for these rewrites:

| Request | Rewrite |
|---|---|
| `POST /containers/create` | Adds `com.zackees.bosn.run` / `.daemon` / `.job` / `.slot` labels. Caps `NanoCpus` (and `Memory`, if set) at the slot's limits; a smaller request, or an explicit CFS period/quota, is kept. Maps named volumes through the job's cache policy and adds its injected cache mounts. Appends the run's suffix to `?name=`. |
| `POST /networks/create`, `POST /volumes/create` | Adds the labels (and maps a volume's name). |
| `GET /containers/json` | Filters to the run's own containers. |

**Why the last two rewrites.** act names job containers, and its per-job
volumes, after the workflow and job only. Before creating a job container it
lists *all* containers and force-removes any with its name. Two checkouts
running the same workflow at once therefore removed each other's job
containers and shared each other's `GITHUB_ENV` volume. Observed under
concurrency, before the fix:

```
Error response from daemon: remove act-ci-build-b1b7…: volume is in use - [79fb9a48…]
failed to copy content to container: … RWLayer of container 79fb9a48… is unexpectedly nil
```

With name suffixes, private per-run volumes and scoped listings, four
concurrent runs of one workflow from four checkouts all pass.

**Failure handling.**
- A create the proxy cannot account for (malformed body, or a cache volume it
  cannot prepare) is answered with a Docker-shaped 500 and never forwarded.
- If the proxy socket is not visible in the container (an image whose user
  is not root, or a proxy directory removed under a running container), the
  task runs without accounting and the job log says so.

**Teardown.** After every task, whatever its outcome (success, failure,
cancel, stall, lost client), the daemon removes every container, network and
volume labelled with the run, through the Docker API (milliseconds, no CLI
processes). Cache volumes never carry a run label and survive:

```
[bosn] teardown of run e65…-4 in 470 ms: removed 1 container(s), 1 network(s), 1 volume(s)
```

**Limitations.**
- Network names are not made unique, so a job with `services:` from two
  concurrent runs of one workflow still shares act's per-job network name.
- A tool that ignores `DOCKER_HOST` and opens `/var/run/docker.sock` directly
  bypasses the proxy.
- Linux only: Docker Desktop cannot pass a host Unix socket into a container.

## Cache volumes that follow act jobs

GitHub jobs share state between jobs in two ways: `actions/cache` (save in job
A, restore in job B), and toolchains under `/opt/hostedtoolcache`. Under Bosn
both work across the jobs of one run, and across runs.

### `actions/cache`: act's cache server

act embeds the Actions cache service (`ACTIONS_CACHE_URL`). Point
`--cache-server-path` at a machine-scoped stack volume and every run shares
it. Concurrent writers do not corrupt it. Every save reserves a new entry
(copy-on-write), and a restore returns the newest complete entry for the key.

Three rounds of three concurrent runs saving one key, 32 MiB each, all restored
whole. GitHub instead rejects a second save of an existing key. Under act the
later save wins.

### Mapped cache volumes: `[stack.<name>.job_caches.<cache>]`

```toml
# Every act job container gets this volume at /bosn-cache: what job "build"
# writes there, job "test" (needs: build) reads, in this run and the next.
[stack.ci.job_caches.shared-out]
destination = "/bosn-cache"
scope = "repo"            # machine | repo (default) | workspace

# Map a named volume wherever a job container references it.
[stack.ci.job_caches.cargo]
volume = "cargo-registry"
mode = "shared"           # exclusive (default) | shared
```

**Keys.** A cache key is the scope plus the cache name. `repo` hashes the
checkout's `origin` URL, so every clone and worktree shares, and falls back to
the checkout path. `workspace` hashes the checkout path. `machine` is
one key for everything.

**Concurrency** is chosen per cache:

- **`exclusive`** (default): per-key locking over a pool of `replicas`
  (default 4). A run leases the lowest-numbered free replica for its whole
  duration. Every workflow job of the run sees the same volume, the next
  sequential run gets the same warm replica, and concurrent runs get different
  replicas. Two runs never write one volume at once. The lock is an OS file
  lock (`flock`) on `/tmp/bosn-<uid>/cache-locks/<volume>.lock`, so every
  bosn daemon of the user honours it, and a crashed daemon's leases are
  released by the kernel. If every replica is busy,
  the run gets a private cold volume, removed with it, like a cache miss on
  GitHub.
- **`shared`**: one volume mounted read-write into every concurrent run, for
  tools that do their own file locking (cargo's registry, uv's cache).

**Default rule.** A stack that mounts the host Docker socket also maps act's
`act-toolcache` volume (`/opt/hostedtoolcache`, where setup-python/node/uv
install) as an exclusive machine-wide cache, unless the manifest maps it
itself. Concurrent `setup-*` installs into one shared toolcache would
otherwise race.

`job_caches` never changes the stack's own container. Cache volumes are
labelled `com.zackees.bosn.cache=<name>`. List them with:

```sh
docker volume ls -f label=com.zackees.bosn.cache
```

Remove one with `docker volume rm` when no run holds it.

### act's own action cache

The directory behind `--action-cache-path` holds act's checkouts of
`uses:` actions. With act's default cache it is a working tree that each run
re-checks-out. Concurrent runs sharing it fail intermittently
(`lstat …/actions-cache-restore@v4/jest.config.ts: no such file or directory`).
Pass `--use-new-action-cache` with a shared action cache: act then fetches into
a bare repository under a random temporary branch and reads by commit SHA,
which is safe concurrently.

## Measurements

These were taken on a 16-CPU host with Docker 29.7.2, shared with other agent
sessions (load average 20 to 30). The fixture runs a two-job `pull_request`
workflow under act: `build` hashes 4 × 12 GB on four threads and saves its
output, and `test` restores it.

| | Wall time, 4 runs from 4 checkouts |
|---|---|
| 1 slot (before #358) | 91.3 s: runs queued, act started at +2.5 / +24 / +48 / +71 s |
| 2 slots | 42.8 s |
| default slots | 62.6 s: every run started at +8.7 s, CPU-saturated (each run's hashing took 24 s instead of 15 s) |

The fixture is the worst case for concurrency, because each run alone fills its
4-CPU quota. Real CI jobs mix CPU with network, container setup and serial
steps.

clud's real `act-ci-linux` (zackees/clud `f2b26ff`; a static job, a Rust
build, then a three-way test matrix with pytest), from three clones:

| | Wall time, 3 runs |
|---|---|
| 1 slot, no CPU cap (the old scheduler) | 976.5 s: act started at +4 / +315 / +640 s |
| default slots, 4 CPUs each | 567.6 s, 1.72× faster: every act started within 12.6 s |
| one run alone, 4-CPU cap | 374.9 s (about 312 s uncapped) |

Three concurrent runs took about 1.5× one run alone, not the sum: they share
16 CPUs with each other and with other sessions (load average 23 at the start).
For these runs, clud's `act_ci.sh` passed `--use-new-action-cache` (see
above and zackees/clud#1724).

Other measurements:

| What | Result |
|---|---|
| Warm cache | First (cold) run 128 s; next sequential run 14.8 s (toolcache replica, action cache and `actions/cache` warm) |
| Setup overhead | Submit to the task's first line: 1.4 to 2.0 s with a warm image and container |
| Proxy overhead | About 0.5 s per two-job act run |
| Teardown | 1 to 470 ms |
| Log throughput | 5 concurrent tasks writing 8 MB each: 34.2 s before, 8.6 s after |

**Why log throughput changed.** The job actor ran a zero-length
`timeout(join_next)` on every command. A zero timeout still waits a timer tick,
which capped the whole daemon near 1000 commands per second. It now reaps
finished tasks every 500 ms and batches each job's log lines.
