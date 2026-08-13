"""The `bosn-docker` command line entry point (Docker/Compose drop-in front door)."""

from __future__ import annotations

import argparse
import sys
from collections.abc import Sequence

from bosn import __version__


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="bosn-docker", description=__doc__)
    parser.add_argument("--version", action="version", version=__version__)
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    parser = build_parser()
    parser.parse_args(argv if argv is not None else sys.argv[1:])
    parser.print_help()
    return 0
