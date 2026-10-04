"""Synthetic receipts must fail closed before Bosn source-bound attestation."""

from __future__ import annotations

import importlib.util
import json
import sys
import unittest
from pathlib import Path
from typing import TypeAlias
from unittest.mock import patch

JsonValue: TypeAlias = str | int | float | bool | list["JsonValue"] | dict[str, "JsonValue"] | None

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("bosn_gate", ROOT / "ci" / "bosn_gate.py")
assert SPEC is not None and SPEC.loader is not None
bosn_gate = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = bosn_gate
SPEC.loader.exec_module(bosn_gate)


class BosnGateProofTests(unittest.TestCase):
    def receipt(self) -> dict[str, JsonValue]:
        # Deliberately synthetic; this fixture never establishes a real gate pass.
        return {
            "schema_version": 1,
            "exit_code": 0,
            "dirty": None,
            "workspace": "/work/repo",
            "sha": "a" * 40,
            "state": "done",
            "conclusion": "success",
            "engine": "act",
            "event": "workflow_dispatch",
            "workflow": ".github/workflows/ci.yml",
            "job": "linux",
            "mode": "minimal",
            "act_version": "0.2.89-act2.3",
            "params": {"inputs": {"tier": "test"}},
            "tree": {
                "malformed_lines": 0,
                "groups": [
                    {
                        "jobs": [
                            {
                                "job_id": required.job_id,
                                "status": "completed",
                                "conclusion": "success",
                                "sections": [
                                    {
                                        "name": name,
                                        "stage": "Main",
                                        "status": "completed",
                                        "conclusion": "success",
                                    }
                                    for name in required.steps
                                ],
                            }
                            for required in bosn_gate.LINUX.required_jobs
                        ]
                    }
                ],
            },
        }

    def error(self, receipt: dict[str, JsonValue]) -> str | None:
        return bosn_gate.proof_error(
            json.dumps(receipt),
            selection=bosn_gate.LINUX,
            workspace=Path("/work/repo"),
            head_sha="a" * 40,
        )

    def test_old_runner_is_rejected_before_engine_submission(self) -> None:
        self.assertIsNone(bosn_gate.version_error("bosn 0.1.12", 0))
        for version in ("bosn 0.1.10", "bosn 0.1.11", "unknown"):
            self.assertIsNotNone(bosn_gate.version_error(version, 0))
        self.assertIsNotNone(bosn_gate.version_error("bosn 0.1.12", 1))
        with (
            patch.object(bosn_gate, "head_sha", return_value="a" * 40),
            patch.object(
                bosn_gate, "run_captured", return_value=bosn_gate.Captured(0, "bosn 0.1.10")
            ) as run,
        ):
            self.assertEqual(1, bosn_gate.run_selection(bosn_gate.RUST))
        run.assert_called_once_with(["bosn", "--version"])

    def test_complete_test_tier_receipt_passes(self) -> None:
        self.assertIsNone(self.error(self.receipt()))

    def test_published_runner_override_uses_isolated_state(self) -> None:
        with patch.dict(
            "os.environ",
            {"BOSN_GATE_VERSION": "0.1.13", "BOSN_GATE_STATE_DIR": "/work/state"},
        ):
            self.assertEqual(["uvx", "--from", "bosn==0.1.13", "bosn"], bosn_gate.bosn_argv())
            self.assertEqual(["--state-dir", "/work/state"], bosn_gate.command(bosn_gate.RUST)[-2:])
        with patch.dict("os.environ", {"BOSN_GATE_VERSION": "not-a-version"}):
            with self.assertRaises(ValueError):
                bosn_gate.bosn_argv()

    def test_minimal_placeholder_cannot_prove_python_or_docker_tests(self) -> None:
        receipt = self.receipt()
        params = bosn_gate.document(receipt["params"])
        bosn_gate.document(params["inputs"])["tier"] = "minimal"
        self.assertIsNotNone(self.error(receipt))

    def test_skipped_docker_test_step_cannot_prove_tests(self) -> None:
        receipt = self.receipt()
        tree = bosn_gate.document(receipt["tree"])
        group = bosn_gate.document(bosn_gate.values(tree["groups"])[0])
        jobs = bosn_gate.values(group["jobs"])
        job = next(
            bosn_gate.document(j) for j in jobs if bosn_gate.document(j)["job_id"] == "linux"
        )
        section = next(
            bosn_gate.document(s)
            for s in bosn_gate.values(job["sections"])
            if bosn_gate.document(s)["name"] == "Test (unit + docker)"
        )
        section["conclusion"] = "skipped"
        self.assertIsNotNone(self.error(receipt))

    def test_other_source_and_dirty_receipt_fail(self) -> None:
        for field, value in {"sha": "b" * 40, "workspace": "/other/repo", "dirty": False}.items():
            receipt = self.receipt()
            receipt[field] = value
            self.assertIsNotNone(self.error(receipt))


if __name__ == "__main__":
    unittest.main()
