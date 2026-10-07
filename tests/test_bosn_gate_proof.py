"""The compatibility entry point delegates proof to the shared CI tool."""

from __future__ import annotations

import importlib.util
import sys
from pathlib import Path
from unittest.mock import patch

ROOT = Path(__file__).resolve().parent.parent
SPEC = importlib.util.spec_from_file_location("bosn_gate", ROOT / "ci" / "bosn_gate.py")
assert SPEC is not None and SPEC.loader is not None
bosn_gate = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = bosn_gate
SPEC.loader.exec_module(bosn_gate)


def test_compatibility_entry_point_preserves_shared_tool_failure() -> None:
    with patch.object(bosn_gate.subprocess, "run") as run:
        run.return_value.returncode = 7
        assert bosn_gate.main(["--no-cache"]) == 7
    command = run.call_args.args[0]
    assert command[command.index("ci-lint") :] == ["ci-lint", "local-gate", "run", "--no-cache"]
    assert run.call_args.kwargs["cwd"] == ROOT
    assert not hasattr(bosn_gate, "proof_error"), (
        "the repository must not keep a second receipt verifier"
    )
