"""Allow `python -m bosn` to invoke the installed Rust CLI."""

from bosn.native_cli import main

if __name__ == "__main__":
    raise SystemExit(main())
