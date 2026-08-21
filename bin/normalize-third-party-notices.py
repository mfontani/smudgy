#!/usr/bin/env python3
"""Normalize cargo-about output for a stable, diff-clean packaged notice."""

from __future__ import annotations

import pathlib


ROOT = pathlib.Path(__file__).resolve().parent.parent
NOTICE = ROOT / "THIRD-PARTY-NOTICES.md"


def main() -> None:
    source = NOTICE.read_text(encoding="utf-8")
    normalized = "\n".join(line.rstrip() for line in source.splitlines()).rstrip() + "\n"
    NOTICE.write_text(normalized, encoding="utf-8", newline="\n")
    print(f"Normalized {NOTICE.name}")


if __name__ == "__main__":
    main()
