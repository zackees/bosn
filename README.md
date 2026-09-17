# Bosn

Bosn is a Rust daemon for safely managing Docker development applications.
It keeps ownership, generation, volume, lease, execution-session, and event
state in a durable SQLite registry. The daemon is the only component that
mutates Docker or that writes that registry.

The distribution includes three intentionally thin front ends:

- `bosn`, a bundled native CLI;
- `import bosn`, a PyO3 client binding; and
- `bosn mcp`, a stdio MCP server for Hermes and other MCP clients.

The former Python daemon, Docker front doors, manifest parser, and SQLite
registry are deliberately not packaged. Python callers use `bosn.Client`; they
cannot import or invoke a second lifecycle implementation.

## Install

```bash
pip install bosn
bosn --version
```

Platform wheels contain the native `bosn` CLI and the `bosn._native` Python
extension. Installing the wheel puts the `bosn` binary itself into the
environment's scripts directory, so the `bosn` on `PATH` is the native CLI,
with no Python wrapper in between.
The extension uses PyO3's `abi3-py310` ABI, so Bosn supports CPython 3.10 and
newer with one platform wheel rather than publishing cp311-only artifacts.

## One-file setup

Bosn accepts a local file or HTTPS URL for a versioned setup document. The
document declares a digest-pinned image or inline Dockerfile, optional inline
files, environment, bind mounts, named volumes, tmpfs mounts, and named tasks.
Planning is inert; ensure and task submission are daemon-owned jobs.

```toml
version = 1

[app]
image = "registry.example/team/app@sha256:REPLACE_WITH_64_LOWERCASE_HEX_DIGEST"
command = "exec sleep 120"

[task.check]
command = "cargo check"
```

```bash
bosn setup plan --state-dir "$HOME/.local/state/bosn" \
  --workspace "$PWD" --config https://example.invalid/bosn-setup.toml \
  --refresh
bosn setup ensure --state-dir "$HOME/.local/state/bosn" \
  --workspace "$PWD" --config https://example.invalid/bosn-setup.toml \
  --refresh --deadline-ms 300000 --output-limit 8388608
```

`online_refresh` is explicit; `offline_cache_only` uses only a previously
validated local cache record. URLs with credentials, unsupported schemes,
path traversal, unpinned images, and oversized documents are refused.

## Python

```python
from pathlib import Path
import bosn

client = bosn.Client(Path.home() / ".local/state/bosn")
plan = client.plan_setup(
    Path.cwd(), "https://example.invalid/bosn-setup.toml", policy="online_refresh"
)
job_id = client.submit_setup_ensure(
    Path.cwd(),
    "https://example.invalid/bosn-setup.toml",
    policy="online_refresh",
    deadline_ms=300_000,
    output_limit=8 * 1024 * 1024,
)
```

The binding exposes typed request and observation operations only. It does not
accept Docker command arguments, container identifiers, mounts, or arbitrary
commands from callers.

## MCP / Hermes

Run `bosn mcp` over stdio. A typical Hermes registration is:

```yaml
mcp_servers:
  bosn:
    command: bosn
    args: [mcp]
```

Set `BOSN_STATE_DIR` if the default state directory is not appropriate. MCP
tools use the same typed daemon operations as the CLI and Python client; long
operations return durable job IDs.

## Development

```bash
./install
./lint
./test
```

The Rust migration status, native manifest lifecycle boundary, and registry
details are documented in [docs/migration-rust.md](docs/migration-rust.md),
[docs/rust-manifest-runtime.md](docs/rust-manifest-runtime.md), and
[docs/rust-registry.md](docs/rust-registry.md).
