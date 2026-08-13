"""Allow `python -m bosn` to invoke the CLI."""

from bosn.cli import main

if __name__ == "__main__":
    raise SystemExit(main())
