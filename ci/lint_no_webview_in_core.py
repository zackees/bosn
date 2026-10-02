"""Gate: the core workspace (CLI, daemon, wheels) never links a webview toolkit.

The desktop widget (`crates/bosn-widget`) is a separate workspace with its own lockfile
precisely so headless servers and containers stay lean. If a webview toolkit appears in the
root `Cargo.lock`, something in the core started depending on it.
"""

from __future__ import annotations

import sys
from pathlib import Path

import tomllib

ROOT = Path(__file__).resolve().parent.parent
FORBIDDEN = frozenset(
    {"webkit2gtk", "webkit2gtk-sys", "webview2-com", "objc2-app-kit", "wry", "tao"}
)


def offenders(lock_path: Path) -> list[str]:
    packages = tomllib.loads(lock_path.read_text(encoding="utf-8")).get("package", [])
    return sorted({p["name"] for p in packages if p.get("name") in FORBIDDEN})


def main(argv: list[str] | None = None) -> int:
    del argv
    found = offenders(ROOT / "Cargo.lock")
    if found:
        print(f"core Cargo.lock links a webview toolkit: {', '.join(found)}", file=sys.stderr)
        return 1
    print("lint_no_webview_in_core: the core workspace links no webview toolkit")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
