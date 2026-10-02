"""Windows Git checkout must preserve the daemon's pinned publisher bytes."""

import hashlib
import os
import subprocess
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
ASSETS = {
    "engine-manifest.json": "6acc6aaf783ac1c1100822e542534c3dab3f1d38782760b0bdcb688280574d9e",
    "engine-config.json": "8cdb6d492106752d557cda50e628b88e7bb303a7eaea91a10bdf672b95ad4f52",
}
DIRECTORY = Path("crates/bosn-service/src/act_engine_data")


@pytest.mark.parametrize("preserve_bytes", [False, True])
def test_windows_checkout_publisher_identity(tmp_path, preserve_bytes):
    repository = tmp_path / "repository"
    repository.mkdir()
    subprocess.run(["git", "init", "--quiet", str(repository)], check=True)
    for name, digest in ASSETS.items():
        original = (ROOT / DIRECTORY / name).read_bytes()
        assert hashlib.sha256(original).hexdigest() == digest
        destination = repository / DIRECTORY / name
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_bytes(original)
    if preserve_bytes:
        (repository / ".gitattributes").write_bytes((ROOT / ".gitattributes").read_bytes())
    subprocess.run(["git", "-c", "core.autocrlf=false", "add", "."], cwd=repository, check=True)
    export = tmp_path / "windows-checkout"
    export.mkdir()
    subprocess.run(
        [
            "git",
            "-c",
            "core.autocrlf=true",
            "checkout-index",
            "--all",
            "--prefix=" + str(export) + os.sep,
        ],
        cwd=repository,
        check=True,
    )
    observed = {
        name: hashlib.sha256((export / DIRECTORY / name).read_bytes()).hexdigest()
        for name in ASSETS
    }
    if preserve_bytes:
        assert observed == ASSETS
    else:
        # Control: the old checkout policy corrupts the multiline OCI manifest.
        assert observed["engine-manifest.json"] != ASSETS["engine-manifest.json"]
        assert b"\r\n" in (export / DIRECTORY / "engine-manifest.json").read_bytes()
