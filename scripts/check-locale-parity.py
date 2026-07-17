#!/usr/bin/env python3
"""Compare heading counts between EN docs and each locale twin.

Fails if any locale doc has a different number of ## / ### headings
than the EN canon. This catches the case where a locale silently
misses an EN section (like the §10.7 Soak testing gap that ate an
entire round of DOCS-review).

Usage: python3 scripts/check-locale-parity.py
Exit 0 if all locales in parity, 1 otherwise.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
EN_ROOT = REPO_ROOT / "docs"
LOCALES = ["ru", "de", "fr", "es"]

# Files that MUST exist in every locale with matching section counts.
TRACKED_FILES = [
    "theory.md",
    "architecture.md",
    "api.md",
    "operations.md",
    "threat-model.md",
    "test-scenarios.md",
    "README.md",
]


def count_headings(path: Path) -> tuple[int, int]:
    """Return (H2_count, H3_count) for a markdown file."""
    h2 = h3 = 0
    for line in path.read_text(encoding="utf-8").splitlines():
        if re.match(r"^## [^#]", line):
            h2 += 1
        elif re.match(r"^### [^#]", line):
            h3 += 1
    return h2, h3


def main() -> int:
    problems: list[str] = []

    for fname in TRACKED_FILES:
        en_path = EN_ROOT / fname
        if not en_path.exists():
            problems.append(f"EN canon missing: {en_path.relative_to(REPO_ROOT)}")
            continue
        en_h2, en_h3 = count_headings(en_path)

        for loc in LOCALES:
            loc_path = EN_ROOT / loc / fname
            if not loc_path.exists():
                problems.append(f"{loc}/{fname}: missing (EN has {en_h2}× H2, {en_h3}× H3)")
                continue
            loc_h2, loc_h3 = count_headings(loc_path)
            if loc_h2 != en_h2 or loc_h3 != en_h3:
                problems.append(
                    f"{loc}/{fname}: {loc_h2}× H2 + {loc_h3}× H3 "
                    f"(EN: {en_h2}× H2 + {en_h3}× H3)"
                )

    if not problems:
        print(
            f"OK: all {len(LOCALES)} locales in heading parity with EN "
            f"across {len(TRACKED_FILES)} docs."
        )
        return 0

    print(f"FAIL: {len(problems)} locale parity issues:\n", file=sys.stderr)
    for p in problems:
        print(f"  {p}", file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main())
