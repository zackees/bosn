"""Run the native clean-machine acceptance binaries inside Docker isolation."""

from __future__ import annotations

import argparse
import os
import shutil
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
IMAGE = "python@sha256:00faa2debb87529f9f0764e9491d8ba400a3678976616c3bd7cb193745ac20d1"
ALPINE = "alpine@sha256:28bd5fe8b56d1bd048e5babf5b10710ebe0bae67db86916198a6eec434943f8b"
TARGETS = (
    "managed_retention_docker",
    "retention_peer_docker",
    "retention_prepare_docker",
)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target-dir", type=Path, default=ROOT / "target")
    args = parser.parse_args()
    target = args.target_dir.resolve()
    docker = shutil.which("docker")
    if docker is None:
        raise RuntimeError("retention acceptance requires Docker")
    for image in (IMAGE, ALPINE):
        if subprocess.run([docker, "image", "inspect", image], capture_output=True).returncode:
            subprocess.run([docker, "pull", image], check=True)
    compile_command = ["soldr", "cargo", "test", "-p", "bosn-service", "--locked", "--no-run"]
    for name in TARGETS:
        compile_command.extend(("--test", name))
    subprocess.run(
        compile_command,
        cwd=ROOT,
        env={**os.environ, "CARGO_TARGET_DIR": str(target)},
        check=True,
    )
    mounts = [ROOT, target]
    if Path("/nix/store").is_dir():
        mounts.append(Path("/nix/store"))
    for name in TARGETS:
        binaries = [
            path
            for path in (target / "debug" / "deps").glob(f"{name}-*")
            if path.is_file() and os.access(path, os.X_OK)
        ]
        if len(binaries) != 1:
            raise RuntimeError(
                f"expected one {name} executable, found {binaries}; use a dedicated --target-dir"
            )
        command = [docker, "run", "--rm", "--network", "none", "-e", "BOSN_TEST_ISOLATED=1"]
        for path in dict.fromkeys(mounts):
            command.extend(("-v", f"{path}:{path}:ro"))
        command.extend(
            (
                "-v",
                f"{Path(docker).resolve()}:/usr/local/bin/docker:ro",
                "-v",
                "/var/run/docker.sock:/var/run/docker.sock",
            )
        )
        command.extend(
            (
                "-w",
                str(ROOT),
                "--entrypoint",
                str(binaries[0]),
                IMAGE,
                "--ignored",
                "--nocapture",
                "--test-threads",
                "1",
            )
        )
        subprocess.run(command, cwd=ROOT, check=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
