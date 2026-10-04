"""Synthetic receipts must fail closed before Bosn source-bound attestation."""

from __future__ import annotations

import json
import unittest
from pathlib import Path

from ci import bosn_gate


class BosnGateProofTests(unittest.TestCase):
    def receipt(self) -> dict[str, bosn_gate.JsonValue]:
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

    def error(self, receipt: dict[str, bosn_gate.JsonValue]) -> str | None:
        return bosn_gate.proof_error(
            json.dumps(receipt),
            selection=bosn_gate.LINUX,
            workspace=Path("/work/repo"),
            head_sha="a" * 40,
        )

    def test_complete_test_tier_receipt_passes(self) -> None:
        self.assertIsNone(self.error(self.receipt()))

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
