#!/usr/bin/env python3
"""Select the CI tier from the current event payload, including label edits."""

from __future__ import annotations

import argparse
import json
from pathlib import Path


def select(
    event_name: str,
    payload: dict,
    manual_tier: str = "minimal",
    manual_sha: str = "",
    workflow_sha: str = "",
) -> str:
    if event_name == "workflow_dispatch":
        if manual_tier not in {"minimal", "test", "full"}:
            raise ValueError(f"invalid manual CI tier: {manual_tier}")
        if manual_tier == "full" and (not manual_sha or manual_sha != workflow_sha):
            raise ValueError("full manual CI requires commit_sha equal to the workflow commit SHA")
        return manual_tier
    if event_name == "pull_request":
        labels = {label["name"] for label in payload["pull_request"].get("labels", [])}
        if "ci-full" in labels:
            return "full"
        if "ci-test" in labels:
            return "test"
    return "minimal"


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--event", required=True)
    parser.add_argument("--payload", type=Path, required=True)
    parser.add_argument("--manual-tier", default="minimal")
    parser.add_argument("--manual-sha", default="")
    parser.add_argument("--workflow-sha", default="")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    tier = select(
        args.event,
        json.loads(args.payload.read_text()),
        args.manual_tier,
        args.manual_sha,
        args.workflow_sha,
    )
    with args.output.open("a") as output:
        output.write(f"tier={tier}\n")
        output.write(f"test={'true' if tier in {'test', 'full'} else 'false'}\n")
        output.write(f"full={'true' if tier == 'full' else 'false'}\n")
    print(f"Bosn CI tier: {tier}")


if __name__ == "__main__":
    main()
