"""A full run proves every platform cell, including hosted Mac execution."""

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "ci"))
import verify_full_coverage as full  # noqa: E402


def test_all_cells_must_succeed() -> None:
    jobs = [
        {"name": name, "conclusion": "success", "id": index}
        for index, name in enumerate(full.REQUIRED, 1)
    ]
    assert full.failures(jobs) == []
    for name in full.REQUIRED:
        assert name in " ".join(full.failures([job for job in jobs if job["name"] != name]))
    jobs[0]["conclusion"] = "skipped"
    assert full.failures(jobs)
    jobs[0]["conclusion"] = "failure"
    assert full.failures(jobs)
