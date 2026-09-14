#!/usr/bin/env python3
"""Stage the payload the macOS Recovery guest fetches to smoke-test the wheel.

Recovery has no Python, no pip, no network to PyPI and no persistent disk, so
everything the smoke needs travels in one tarball served by the
``zackees/docker-mac-x64`` action:

    payload/
      python/                     relocatable CPython 3.10 (python-build-standalone),
                                  libpython patched by ci/patch_recovery_python.py
      bosn-<v>-cp310-abi3-macosx_10_12_x86_64.whl
      verify_installed_wheel.py   the same verifier the Linux/Windows lanes run

CPython 3.10 is deliberate: the wheel is abi3-py310, so the guest exercises it
on the oldest interpreter the tag claims.  The archive is pinned by URL and
sha256; a mismatch refuses to stage rather than smoke-testing against an
unknown interpreter.

    python3 ci/recovery_payload.py --wheel dist/bosn-*.whl --out payload
    python3 ci/recovery_payload.py --wheel ... --out payload --cpython-archive cpython.tar.gz
"""

from __future__ import annotations

import argparse
import glob
import hashlib
import json
import shutil
import sys
import tarfile
import tempfile
import urllib.request
from pathlib import Path
from typing import Any

import patch_recovery_python

CPYTHON_URL = (
    "https://github.com/astral-sh/python-build-standalone/releases/download/20250612/"
    "cpython-3.10.18%2B20250612-x86_64-apple-darwin-install_only.tar.gz"
)
CPYTHON_SHA256 = "92ecfbfb89e8137cc88cabc2f408d00758d67454d07c1691706d3dcccc8fc446"
LIBPYTHON = "python/lib/libpython3.10.dylib"
WHEEL_TAG = "cp310-abi3-macosx_10_12_x86_64"


def sha256_of(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def download(url: str, destination: Path) -> None:
    with urllib.request.urlopen(url, timeout=120) as response, destination.open("wb") as out:
        shutil.copyfileobj(response, out)


def stage(
    out: Path,
    *,
    wheel: Path,
    verifier: Path,
    cpython_archive: Path,
    sha256: str,
) -> dict[str, Any]:
    if not wheel.name.endswith(f"-{WHEEL_TAG}.whl"):
        raise SystemExit(
            f"{wheel.name} is not the x86_64 Darwin abi3 wheel ({WHEEL_TAG}); "
            "only Intel macOS can execute in the Recovery guest"
        )
    actual = sha256_of(cpython_archive)
    if actual != sha256:
        raise SystemExit(
            f"CPython archive sha256 mismatch: expected {sha256}, got {actual}; refusing to stage"
        )
    if out.exists():
        shutil.rmtree(out)
    out.mkdir(parents=True)
    with tarfile.open(cpython_archive) as archive:
        archive.extractall(out, filter="data")
    libpython = out / LIBPYTHON
    if not libpython.is_file():
        raise SystemExit(f"archive did not contain {LIBPYTHON}")
    patch_recovery_python.patch(str(libpython))
    shutil.copy2(wheel, out / wheel.name)
    shutil.copy2(verifier, out / "verify_installed_wheel.py")
    manifest = {
        "wheel": wheel.name,
        "cpython_sha256": sha256,
        "libpython_patched": LIBPYTHON,
        "verifier": "verify_installed_wheel.py",
    }
    (out / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    return manifest


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--wheel", required=True)
    parser.add_argument("--out", required=True)
    parser.add_argument("--cpython-archive", help="use a local archive instead of downloading")
    parser.add_argument(
        "--verifier",
        default=str(Path(__file__).resolve().parent / "verify_installed_wheel.py"),
    )
    args = parser.parse_args(argv)
    wheels = [Path(match) for match in sorted(glob.glob(args.wheel))]
    if len(wheels) != 1:
        raise SystemExit(f"expected exactly one wheel, got {wheels}")
    with tempfile.TemporaryDirectory() as temporary:
        if args.cpython_archive:
            archive = Path(args.cpython_archive)
        else:
            archive = Path(temporary) / "cpython.tar.gz"
            print(f"downloading {CPYTHON_URL}")
            download(CPYTHON_URL, archive)
        manifest = stage(
            Path(args.out),
            wheel=wheels[0],
            verifier=Path(args.verifier),
            cpython_archive=archive,
            sha256=CPYTHON_SHA256,
        )
    print(json.dumps(manifest, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main())
