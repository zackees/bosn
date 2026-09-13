"""The native surface is exercised after ``maturin develop`` in release checks."""

from pathlib import Path

import pytest

import bosn

native = pytest.importorskip("bosn._native")


def test_native_extension_is_reexported_by_python_package(tmp_path: Path) -> None:
    assert bosn.Client is native.Client
    assert bosn.Status is native.Status
    assert bosn.native_version() == bosn.__version__
    assert bosn.protocol_version() == 1

    client = bosn.Client(tmp_path / "state")
    assert client.state_dir == str(tmp_path / "state")
    with pytest.raises(RuntimeError, match="Io"):
        client.status()
