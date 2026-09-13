"""Measure the Python migration baseline against an isolated, disposable registry.

Run with ``uv run python ci/migration_baseline.py [--docker]``. Docker mode builds
one synthetic stack and removes only objects carrying this run's registry UUID.
No machine policy or existing Bosn state is changed. Output is JSON for comparison
with the eventual Rust installed-artifact measurements.
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from unittest.mock import patch

from bosn import __version__, daemon, labels
from bosn.engine import Engine
from bosn.registry import Registry
from bosn.resources import ResourceScanner


def measure(command: list[str], env: dict[str, str], samples: int) -> dict[str, object]:
    durations = []
    for _ in range(samples):
        started = time.perf_counter()
        result = subprocess.run(command, env=env, capture_output=True, text=True, timeout=180)
        if result.returncode:
            raise RuntimeError(f"{command!r} failed: {result.stdout}\n{result.stderr}")
        durations.append((time.perf_counter() - started) * 1000)
    return {"samples_ms": durations, "median_ms": statistics.median(durations)}


def clean_owned(registry_id: str) -> None:
    engine = Engine()
    scanner = ResourceScanner(engine)
    for kind, listing, args in (
        ("container", ["ps", "-a", "-q", "--no-trunc"], ["rm", "-f"]),
        ("network", ["network", "ls", "-q"], ["network", "rm"]),
        ("volume", ["volume", "ls", "-q"], ["volume", "rm"]),
        ("image", ["image", "ls", "-q", "--no-trunc"], ["image", "rm"]),
    ):
        inventory = engine.run([*listing, "--filter", f"label={labels.REGISTRY}={registry_id}"])
        if not inventory.ok:
            raise RuntimeError(f"cannot list benchmark {kind}: {inventory.stderr}")
        for name in dict.fromkeys(inventory.stdout.splitlines()):
            if labels.is_owned_by(scanner.inspect_labels(kind, name), registry_id):
                result = engine.run([*args, name])
                if not result.ok:
                    raise RuntimeError(f"cleanup failed for {kind}:{name}: {result.stderr}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--docker", action="store_true")
    parser.add_argument("--samples", type=int, default=7)
    args = parser.parse_args()
    if args.samples < 1:
        parser.error("--samples must be positive")
    with tempfile.TemporaryDirectory(prefix="bosn-migration-baseline-") as temporary:
        root = Path(temporary)
        state_dir = root / "state"
        env = dict(os.environ, BOSN_STATE_DIR=str(state_dir))
        env.pop("BOSN_PORT", None)
        env.pop("BOSN_CONFIG", None)
        env["XDG_CONFIG_HOME"] = str(root / "config")
        command = [sys.executable, "-m", "bosn", "--state-dir", str(state_dir)]
        measurements = {"cli_version": measure(command + ["--version"], env, args.samples)}
        with patch.dict(os.environ, env, clear=True):
            state = daemon.spawn(state_dir)
        registry_id = None
        try:
            with Registry(state_dir / "registry.sqlite3", read_only=True) as registry:
                registry_id = registry.registry_id
            measurements["status_empty_registry"] = measure(
                command + ["status", "--json"], env, args.samples
            )
            process_status = Path(f"/proc/{state.pid}/status")
            rss = None
            if process_status.exists():
                rss = next(
                    int(line.split()[1])
                    for line in process_status.read_text().splitlines()
                    if line.startswith("VmRSS:")
                )
            if args.docker:
                (root / "Dockerfile").write_text("FROM alpine:3.20\nRUN true\n")
                manifest = root / "bosn.toml"
                manifest.write_text(
                    '[stack.baseline]\ndockerfile = "Dockerfile"\ndefault = true\n'
                    '[stack.baseline.volumes]\ncache = { scope = "stack" }\n'
                )
                ensure = command + ["--manifest", str(manifest), "ensure", "--json"]
                measurements["ensure_first_build"] = measure(ensure, env, 1)
                measurements["ensure_reuse"] = measure(ensure, env, args.samples)
            measurements["wheel_build"] = measure(
                ["uv", "build", "--wheel", "--out-dir", str(root / "dist")], env, 1
            )
            print(
                json.dumps(
                    {
                        "implementation": f"python-{__version__}",
                        "platform": platform.platform(),
                        "python": platform.python_version(),
                        "daemon_idle_rss_kib": rss,
                        "measurements": measurements,
                        "notes": "Wall-clock; caches are not cleared. RSS is Linux-only.",
                    },
                    indent=2,
                )
            )
        finally:
            with patch.dict(os.environ, env, clear=True):
                daemon.stop(state_dir, timeout=45)
                if daemon.is_serving(state_dir):
                    raise RuntimeError(f"benchmark daemon did not stop: {state_dir}")
            if args.docker and registry_id:
                clean_owned(registry_id)


if __name__ == "__main__":
    main()
