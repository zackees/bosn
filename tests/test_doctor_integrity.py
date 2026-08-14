"""`doctor` must diagnose sqlite corruption and print non-destructive recovery guidance.

Registry.integrity_check() (src/bosn/registry.py) already proves the read-only happy
path (tests/test_registry.py::test_integrity_check_is_read_only). What is untested is
the failure path: cmd_doctor's response when integrity_check reports something other
than "ok". These tests corrupt a *real* database file (not a from-scratch garbage file,
which sqlite would refuse to open at all) so integrity_check reports the corruption
instead of raising, then assert doctor surfaces it, prints copy-pasteable backup-then-
`.recover` commands, and never mutates the corrupted file itself.
"""

from __future__ import annotations

import hashlib
from pathlib import Path

from bosn import cli
from bosn.registry import Registry


def _corrupt_mid_file(path: Path) -> None:
    """Flip bytes well past the header so sqlite reports page corruption, not open failure.

    A file of random junk bytes fails at *connection* time ("file is not a database"),
    before PRAGMA integrity_check can even run. Corrupting live pages in the middle of a
    real database lets sqlite open the file, walk the btree, and report the damage
    through PRAGMA integrity_check -- the actual code path cmd_doctor must handle.
    """
    data = bytearray(path.read_bytes())
    mid = len(data) // 2
    for i in range(mid, min(mid + 512, len(data))):
        data[i] = 0xFF
    path.write_bytes(bytes(data))


def _make_corrupted_registry(tmp_path: Path) -> Path:
    db_path = tmp_path / "registry.sqlite3"
    with Registry(db_path) as registry:
        registry.register_resource(
            kind="volume",
            name="cache",
            stack="dev",
            generation="digest",
            scope="spec",
            workspace="workspace",
        )
    _corrupt_mid_file(db_path)
    return db_path


def test_doctor_surfaces_bad_integrity_and_prints_recovery_guidance(tmp_path: Path, capsys) -> None:
    db_path = _make_corrupted_registry(tmp_path)

    cli.main(
        ["--state-dir", str(tmp_path), "--engine", "definitely-not-an-engine-binary", "doctor"]
    )

    captured = capsys.readouterr()
    assert "registry integrity: ok" not in captured.out
    assert "registry integrity:" in captured.out

    # Exact, copy-pasteable commands: back up first, then .recover -- never overwrite
    # the original file in place.
    assert "VACUUM INTO" in captured.err
    assert str(db_path) in captured.err
    assert ".recover" in captured.err
    assert "sqlite3" in captured.err


def test_doctor_never_mutates_the_corrupted_database(tmp_path: Path, capsys) -> None:
    """Non-destructiveness is the actual safety property: hash before and after."""
    db_path = _make_corrupted_registry(tmp_path)
    before = hashlib.sha256(db_path.read_bytes()).hexdigest()

    cli.main(
        ["--state-dir", str(tmp_path), "--engine", "definitely-not-an-engine-binary", "doctor"]
    )

    after = hashlib.sha256(db_path.read_bytes()).hexdigest()
    assert after == before, "doctor must never write to a database it is only diagnosing"
