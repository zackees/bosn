"""The `bosn` command line entry point.

Every verb from the design is registered here. Verbs whose implementation has not landed
yet exit with a specific error naming the verb and the phase that will land it — never a
silent no-op and never a fallback to raw Docker.
"""

from __future__ import annotations

import argparse
import sys
from collections.abc import Sequence

from bosn import __version__
from bosn.engine import Engine

NOT_IMPLEMENTED_EXIT = 3

# verb -> (help text, phase that lands it)
VERBS: dict[str, tuple[str, str]] = {
    "run": ("run an ad-hoc command in a stack", "phase 5"),
    "shell": ("interactive session in the persistent container", "phase 5"),
    "tasks": ("list manifest tasks, stacks, digests, registration state", "phase 5"),
    "jobs": ("list daemon-owned jobs", "phase 3"),
    "attach": ("attach to a daemon-owned job", "phase 3"),
    "status": ("tiers, leases, managed bytes vs ceiling, foreign registries", "phase 6"),
    "gc": ("report or reclaim collectable resources", "phase 6"),
    "done": ("mark this workspace finished; its caches become collectable", "phase 6"),
    "doctor": ("engine health and reachability", "implemented"),
}


class VerbNotImplementedError(RuntimeError):
    def __init__(self, verb: str, phase: str) -> None:
        super().__init__(f"`bosn {verb}` is not implemented yet (lands in {phase}).")
        self.verb = verb
        self.phase = phase


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="bosn", description="bosn - container lifecycle supervisor"
    )
    parser.add_argument("--version", action="version", version=__version__)
    parser.add_argument(
        "--engine",
        default="docker",
        help="container engine binary to drive (default: docker)",
    )
    subparsers = parser.add_subparsers(dest="verb", metavar="VERB")
    for verb, (help_text, _) in VERBS.items():
        sub = subparsers.add_parser(verb, help=help_text)
        sub.add_argument("args", nargs=argparse.REMAINDER, help=argparse.SUPPRESS)
    return parser


def cmd_doctor(engine_binary: str) -> int:
    info = Engine(engine_binary).info()
    print(f"engine binary:  {info.binary}")
    print(f"client version: {info.client_version or '-'}")
    print(f"server version: {info.server_version or '-'}")
    print(f"reachable:      {'yes' if info.reachable else 'no'}")
    if not info.reachable:
        print(f"diagnosis:      {info.detail}", file=sys.stderr)
        return 1
    return 0


def main(argv: Sequence[str] | None = None) -> int:
    parser = build_parser()
    ns = parser.parse_args(argv if argv is not None else sys.argv[1:])

    if ns.verb is None:
        parser.print_help()
        return 0

    if ns.verb == "doctor":
        return cmd_doctor(ns.engine)

    error = VerbNotImplementedError(ns.verb, VERBS[ns.verb][1])
    print(str(error), file=sys.stderr)
    return NOT_IMPLEMENTED_EXIT
