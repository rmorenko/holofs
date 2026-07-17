#!/usr/bin/env python3
"""Content-hash gate for security-critical documentation sections.

Heading-parity alone can't catch the C1-in-locales drift class: §8
exists in every locale with the right title, but its body silently
disagrees with the EN canon.

This script hashes the EN body of each security-critical section and
compares against a checked-in baseline. When EN drifts, CI fails with
an explicit list of locale files that must be updated in the same
commit — the author then runs `--update` after confirming the locale
edits to record the new baseline.

The gate is intentionally *manual*: we can't semantically compare
translated prose, but forcing a visible baseline diff makes the
reviewer notice the security section touched and check the twins.

Usage:
    python3 scripts/check-security-parity.py            # CI mode
    python3 scripts/check-security-parity.py --update   # rewrite baseline

Exit 0 clean, 1 if any hash drifted or a section is missing a baseline.
"""

from __future__ import annotations

import hashlib
import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
BASELINE_PATH = REPO_ROOT / "scripts" / ".docs-security-baseline"

# (file, marker, human-name, locale-twins)
# Add rows when a new section becomes security-critical. Marker is either
# an H2/H3 heading prefix (`## 8.`) or a table-row prefix (`| I6 `).
CRITICAL_SECTIONS: list[tuple[str, str, str, list[str]]] = [
    (
        "docs/theory.md",
        "## 8.",
        "theory §8 body (threshold-erasure security claim)",
        [
            "docs/ru/theory.md",
            "docs/de/theory.md",
            "docs/fr/theory.md",
            "docs/es/theory.md",
        ],
    ),
    (
        "docs/threat-model.md",
        "| I6 ",
        "threat-model I6 mitigation (holoshare leak)",
        [
            "docs/ru/threat-model.md",
            "docs/de/threat-model.md",
            "docs/fr/threat-model.md",
            "docs/es/threat-model.md",
        ],
    ),
    (
        "docs/threat-model.md",
        "| I2 ",
        "threat-model I2 mitigation (K-shard adversary)",
        [
            "docs/ru/threat-model.md",
            "docs/de/threat-model.md",
            "docs/fr/threat-model.md",
            "docs/es/threat-model.md",
        ],
    ),
    (
        "docs/threat-model.md",
        "| S1 ",
        "threat-model S1 mitigation (node impersonation)",
        [
            "docs/ru/threat-model.md",
            "docs/de/threat-model.md",
            "docs/fr/threat-model.md",
            "docs/es/threat-model.md",
        ],
    ),
    (
        "docs/threat-model.md",
        "| S2 ",
        "threat-model S2 mitigation (gateway impersonation / TLS)",
        [
            "docs/ru/threat-model.md",
            "docs/de/threat-model.md",
            "docs/fr/threat-model.md",
            "docs/es/threat-model.md",
        ],
    ),
]


def extract_section(path: Path, marker: str) -> str:
    """Return the section that starts at the first line beginning with `marker`.

    For H2/H3 markers (leading '#'), the section runs until the next
    same-or-higher heading. For table-row markers (leading '|'), only
    the matched row is returned.
    """
    lines = path.read_text(encoding="utf-8").splitlines()
    start: int | None = None
    for i, line in enumerate(lines):
        if line.startswith(marker):
            start = i
            break
    if start is None:
        raise ValueError(f"marker {marker!r} not found")

    if marker.startswith("|"):
        return lines[start].rstrip() + "\n"

    m = re.match(r"^(#+)", marker)
    if m is None:
        raise ValueError(f"unrecognised marker {marker!r}")
    depth = len(m.group(1))
    end = len(lines)
    for i in range(start + 1, len(lines)):
        m2 = re.match(r"^(#+)\s", lines[i])
        if m2 and len(m2.group(1)) <= depth:
            end = i
            break
    return "\n".join(line.rstrip() for line in lines[start:end]) + "\n"


def hash_section(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def load_baseline() -> dict[tuple[str, str], str]:
    out: dict[tuple[str, str], str] = {}
    if not BASELINE_PATH.exists():
        return out
    for line in BASELINE_PATH.read_text(encoding="utf-8").splitlines():
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        # keep trailing whitespace inside markers like "| I6 "
        parts = line.split("\t")
        if len(parts) != 3:
            continue
        h, file, marker = parts
        out[(file, marker)] = h
    return out


def write_baseline(entries: list[tuple[str, str, str]]) -> None:
    header = [
        "# scripts/.docs-security-baseline",
        "#",
        "# SHA-256 hashes of security-critical EN doc sections. If any of",
        "# these changes, the same commit MUST update the corresponding",
        "# locale twin files (ru/de/fr/es) to reflect the same content.",
        "#",
        "# To regenerate after intentional edits:",
        "#   python3 scripts/check-security-parity.py --update",
        "#",
        "# Format: <sha256>\\t<file>\\t<heading-or-row-marker>",
        "",
    ]
    body = [f"{h}\t{file}\t{marker}" for h, file, marker in entries]
    BASELINE_PATH.write_text("\n".join(header + body) + "\n", encoding="utf-8")


def main() -> int:
    update_mode = "--update" in sys.argv
    baseline = load_baseline()

    problems: list[str] = []
    current: list[tuple[str, str, str]] = []

    for file, marker, name, twins in CRITICAL_SECTIONS:
        en_path = REPO_ROOT / file
        try:
            body = extract_section(en_path, marker)
        except (ValueError, OSError) as e:
            problems.append(f"{file}: {e}")
            continue

        h = hash_section(body)
        current.append((h, file, marker))

        stored = baseline.get((file, marker))
        if stored is None:
            problems.append(
                f"NEW: {name}\n"
                f"    file: {file} @ marker {marker!r}\n"
                f"    no baseline yet — run with --update after verifying locale twins"
            )
        elif stored != h:
            twins_list = "\n      ".join(twins)
            problems.append(
                f"DRIFT: {name}\n"
                f"    file: {file} @ marker {marker!r}\n"
                f"    baseline: {stored[:12]}…  current: {h[:12]}…\n"
                f"    MUST also update these locale twins in the same commit:\n"
                f"      {twins_list}\n"
                f"    then run: python3 scripts/check-security-parity.py --update"
            )

    if update_mode:
        write_baseline(current)
        print(f"Baseline updated: {BASELINE_PATH.relative_to(REPO_ROOT)}")
        print(f"  {len(current)} sections recorded.")
        return 0

    if not problems:
        print(f"OK: {len(current)} security-critical sections match baseline.")
        return 0

    print(f"FAIL: {len(problems)} security-critical section(s) drifted:\n", file=sys.stderr)
    for p in problems:
        print(f"  {p}\n", file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main())
