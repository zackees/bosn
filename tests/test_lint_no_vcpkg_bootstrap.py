"""Regression coverage for the soldr#3231 no-vcpkg-bootstrap policy."""

from __future__ import annotations

import sys
from pathlib import Path

import yaml

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "ci"))
import lint_no_vcpkg_bootstrap as lint  # noqa: E402


def test_current_workflows_have_no_vcpkg_install() -> None:
    root = Path(__file__).resolve().parents[1]
    assert lint.check(root / ".github/workflows") == 0


def test_lint_rejects_the_removed_windows_openssl_step() -> None:
    document = yaml.safe_load(
        """jobs:
  native-wheel:
    steps:
      - name: Provide OpenSSL to Cargo (Windows)
        shell: pwsh
        run: |
          $vcpkg = $env:VCPKG_INSTALLATION_ROOT
          & "$vcpkg\\vcpkg.exe" install openssl:x64-windows
      - run: vcpkg --triplet x64-windows-static-md install zlib
"""
    )
    assert list(lint.offenders(document)) == [
        ("jobs.native-wheel.steps[0].run", '& "$vcpkg\\vcpkg.exe" install openssl:x64-windows'),
        ("jobs.native-wheel.steps[1].run", "vcpkg --triplet x64-windows-static-md install zlib"),
    ]


def test_lint_ignores_vcpkg_prose_and_comments() -> None:
    document = yaml.safe_load(
        """jobs:
  native-wheel:
    name: Native wheel (no vcpkg install)
    steps:
      - name: Build without vcpkg install openssl
        run: |
          # vcpkg install openssl is unnecessary: native-tls uses SChannel.
          uv build --wheel --out-dir dist
"""
    )
    assert list(lint.offenders(document)) == []
