#!/usr/bin/env python3
"""Cross-check documented HOLOFS_* defaults against the code source of truth.

# Why this exists

The July 2026 PRODUCTION-READINESS-v2 review round found three
defaults in `docs/tuning.md` that had drifted from the actual
`unwrap_or(N)` values in the code (`HOLOFS_MONITOR_INTERVAL`,
`_AUDIT_INTERVAL`, `_SCRUB_INTERVAL`). The three existing doc gates
(anchor / locale-parity / security-content) only check STRUCTURE;
none of them checked VALUES. This gate closes that gap.

# How it works

1. Read `docs/tuning.md` and extract every row of every Markdown
   table that starts with a `HOLOFS_*` env var + a `★ <value>`
   default marker.
2. For each var, look up the expected default from `EXPECTED` below
   (the source-of-truth manifest, embedded in the script). If it's
   listed here and matches the tuning.md row → OK.
3. Also verify the manifest against the actual code — grep for
   `std::env::var("VAR")` sites and confirm at least one uses the
   documented default via `unwrap_or(<value>)`.
4. Emit a report and exit non-zero on any mismatch.

The manifest sits in this file (not a separate config) so a doc
edit + code edit + manifest edit either land in the same commit or
CI fails — no way for the three to drift apart silently.

Usage: python3 scripts/check-env-defaults.py
Exit 0 on match, 1 otherwise.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
TUNING_MD = REPO_ROOT / "docs" / "tuning.md"
CODE_DIRS = [REPO_ROOT / "crates"]

# Source-of-truth manifest. Each entry: env var → (documented default
# in tuning.md, expected literal in code's `unwrap_or(...)`, code
# path hint for the grep site).
#
# Notes:
# - `documented` is the literal that must appear on the same row as
#   the env var in the tuning.md table (after `★`, ignoring units
#   like " s", " min", " MiB", brackets `(...)`).
# - `code_literal` is the number as it appears in `unwrap_or(N)` in
#   the code (integers, floats, or a named constant string). When
#   `None`, the code-side check is skipped (e.g. compile-time
#   `const N: usize = X` that isn't inline in an `unwrap_or`).
# - `hint` is a substring used to disambiguate multiple env-var
#   reads (mostly for future maintainers when two crates read the
#   same var).
EXPECTED: dict[str, dict[str, object]] = {
    # ----- Backpressure -----
    "HOLOFS_MEDIUM_CONCURRENCY": {
        "documented": "64",
        "code_literal": "DEFAULT_MEDIUM_CONCURRENCY",  # named constant
        "hint": None,
    },
    "HOLOFS_LONG_CONCURRENCY": {
        "documented": "24",
        "code_literal": "DEFAULT_LONG_CONCURRENCY",
        "hint": None,
    },
    "HOLOFS_ENCODE_CONCURRENCY": {
        "documented": "8",
        "code_literal": "DEFAULT_ENCODE_CONCURRENCY",
        "hint": None,
    },
    # ----- Node storage -----
    "HOLOFS_NODE_FSYNC": {
        "documented": "1",  # docs write "★ `1` (on)"
        "code_literal": None,  # boolean parse, not unwrap_or literal
        "hint": None,
    },
    "HOLOFS_NODE_FLUSH_INTERVAL_MS": {
        "documented": "5",
        "code_literal": "5",
        "hint": "flush_interval_ms",
    },
    # ----- WAL compaction -----
    "HOLOFS_WAL_COMPACT_INTERVAL_SECS": {
        "documented": "300",
        "code_literal": "300",
        "hint": "wal_compact_interval_secs",
    },
    "HOLOFS_WAL_COMPACT_RATIO": {
        "documented": "2.0",
        "code_literal": "2.0",
        "hint": "wal_compact_ratio",
    },
    "HOLOFS_WAL_COMPACT_MIN_BYTES": {
        "documented": "4",  # docs write "★ 4 MiB"
        "code_literal": "4 * 1024 * 1024",
        "hint": "wal_compact_min_bytes",
    },
    # ----- Background loops -----
    "HOLOFS_MONITOR_INTERVAL": {
        "documented": "15",
        "code_literal": "15",
        "hint": "monitor_interval_secs",
    },
    "HOLOFS_AUDIT_INTERVAL": {
        "documented": "60",
        "code_literal": "60",
        "hint": "audit_interval_secs",
    },
    "HOLOFS_SCRUB_INTERVAL": {
        "documented": "600",
        "code_literal": "600",
        "hint": "scrub_interval_secs",
    },
    "HOLOFS_REPUTATION_PERSIST_INTERVAL": {
        "documented": "30",
        "code_literal": "30",
        "hint": "reputation_persist_interval_secs",
    },
    "HOLOFS_CAPACITY_POLL_INTERVAL_SECS": {
        "documented": "60",
        "code_literal": "DEFAULT_POLL_INTERVAL_SECS",
        "hint": None,
    },
    "HOLOFS_REBALANCE_INTERVAL_SECS": {
        "documented": "300",
        "code_literal": "DEFAULT_REBALANCE_INTERVAL_SECS",
        "hint": None,
    },
    "HOLOFS_REBALANCE_TRIGGER_PCT": {
        "documented": "85",
        "code_literal": "DEFAULT_REBALANCE_TRIGGER_PCT",
        "hint": None,
    },
    "HOLOFS_REBALANCE_COLD_CEILING_PCT": {
        "documented": "60",
        "code_literal": "DEFAULT_REBALANCE_COLD_CEILING_PCT",
        "hint": None,
    },
    "HOLOFS_RETENTION_GC_INTERVAL_SECS": {
        "documented": "3600",
        "code_literal": "DEFAULT_RETENTION_GC_INTERVAL_SECS",
        "hint": None,
    },
    # ----- Networking + limits -----
    "HOLOFS_POOL_PER_NODE": {
        "documented": "32",
        "code_literal": "32",
        "hint": None,
    },
    "HOLOFS_POOL_IDLE_SECS": {
        "documented": "30",
        "code_literal": "30",
        "hint": None,
    },
    "HOLOFS_POOL_DISABLE": {
        "documented": "off",  # boolean-ish
        "code_literal": None,
        "hint": None,
    },
    "HOLOFS_RPC_TIMEOUT_MS": {
        "documented": "8_000",
        "code_literal": "8_000",
        "hint": None,
    },
    "HOLOFS_RATE_LIMIT_RPS_PER_IP": {
        "documented": "off",  # opt-in, no numeric default
        "code_literal": None,
        "hint": None,
    },
    "HOLOFS_RATE_LIMIT_BURST": {
        "documented": "2",  # docs say "★ 2 × rps"
        "code_literal": None,  # computed from rps
        "hint": None,
    },
    "HOLOFS_UPLOAD_MAX_SIZE": {
        "documented": "50",  # docs write "★ 50 MiB"
        "code_literal": None,  # computed from a const in bytes
        "hint": None,
    },
    # ----- Encoder + async -----
    "HOLOFS_ENCODE_QUEUE_MAX": {
        "documented": "8",  # docs say "★ 8 × encode_concurrency (≥ 32)"
        "code_literal": None,  # computed via saturating_mul(8)
        "hint": None,
    },
    # ----- At-rest encryption -----
    "HOLOFS_AT_REST_ENC": {
        "documented": "off",  # boolean
        "code_literal": None,
        "hint": None,
    },
    # ----- Observability -----
    "HOLOFS_OTLP_SERVICE_NAME": {
        "documented": "holofs-web",
        "code_literal": None,
        "hint": None,
    },
}


TABLE_ROW_RE = re.compile(
    # Matches:  | `HOLOFS_X` | ★ VALUE | …
    # Also accepts "★ VALUE (comment)" and "★ VALUE unit_word".
    r"^\|\s*`(HOLOFS_[A-Z_0-9]+)`\s*\|\s*★\s*`?(\S+?)`?(?:\s+[^|]*?)?\s*\|",
    re.MULTILINE,
)


def parse_tuning_md() -> dict[str, str]:
    """Return {env_var: documented_default_literal_from_the_★_column}."""
    text = TUNING_MD.read_text(encoding="utf-8")
    out: dict[str, str] = {}
    for m in TABLE_ROW_RE.finditer(text):
        out[m.group(1)] = m.group(2)
    return out


def grep_code_default(env_var: str, code_literal: str, hint: str | None) -> bool:
    """Return True if some code file has the env var string
    (`"HOLOFS_X"`) followed within ~20 lines by `unwrap_or(<literal>)`.
    We match on the *string literal* rather than `std::env::var("X")`
    so multi-line `env::var(\\n "X",\\n)` still counts. Kept crude
    on purpose — a fuzzy match beats a brittle AST walk here.
    """
    pattern_env_str = re.compile(rf'"{re.escape(env_var)}"')
    # Allow anything up to the literal inside `unwrap_or(...)` — the
    # gate matches both bare `unwrap_or(30)` and fully-qualified
    # `unwrap_or(holofs_gateway::capacity::DEFAULT_POLL_INTERVAL_SECS)`.
    # Word-boundary at the tail keeps `unwrap_or(30_000)` from matching
    # a manifest expecting `3` etc.
    pattern_default = re.compile(
        rf"unwrap_or\([^)]*\b{re.escape(code_literal)}\b",
        re.DOTALL,
    )
    for root in CODE_DIRS:
        for path in root.rglob("*.rs"):
            if "target" in path.parts or "tests" in path.parts:
                continue
            try:
                text = path.read_text(encoding="utf-8")
            except Exception:
                continue
            lines = text.splitlines()
            for i, line in enumerate(lines):
                if not pattern_env_str.search(line):
                    continue
                # Env var string found on this line. Check ±5 lines
                # before (in case the var name comes AFTER the
                # `env::var(` open-paren) and 20 lines after.
                start = max(0, i - 5)
                window = "\n".join(lines[start : i + 20])
                if pattern_default.search(window):
                    if hint is None or hint in window:
                        return True
    return False


def main() -> int:
    problems: list[str] = []
    documented = parse_tuning_md()

    # 1. Every EXPECTED var must appear in tuning.md.
    for var, spec in EXPECTED.items():
        got = documented.get(var)
        want = spec["documented"]
        if got is None:
            problems.append(
                f"[MISSING] {var}: manifest says default={want!r}, "
                f"but tuning.md has no `★` row for it"
            )
            continue
        if got != want:
            problems.append(
                f"[DRIFT-DOC] {var}: manifest default={want!r}, "
                f"tuning.md says {got!r}"
            )

    # 2. Every documented var should also be in EXPECTED
    #    (so a new var added to the docs prompts a manifest update).
    for var in documented:
        if var not in EXPECTED:
            problems.append(
                f"[UNKNOWN-DOC] {var}: appears in tuning.md but not "
                f"in scripts/check-env-defaults.py::EXPECTED — add it "
                f"to the manifest with the source-of-truth default"
            )

    # 3. Every EXPECTED entry with a code_literal must match some
    #    unwrap_or(<lit>) site in code.
    for var, spec in EXPECTED.items():
        code_lit = spec["code_literal"]
        if code_lit is None:
            continue
        if not grep_code_default(var, str(code_lit), spec.get("hint")):  # type: ignore[arg-type]
            problems.append(
                f"[DRIFT-CODE] {var}: manifest says code default={code_lit!r}, "
                f"but no `std::env::var(\"{var}\")` site in crates/ has a "
                f"matching `unwrap_or({code_lit})` within 15 lines"
                + (f" (hint: {spec['hint']!r})" if spec.get("hint") else "")
            )

    if not problems:
        print(
            f"OK: {len(EXPECTED)} HOLOFS_* env vars match "
            f"tuning.md ↔ manifest ↔ code."
        )
        return 0
    print(f"FAILED: {len(problems)} default(s) drift:")
    for p in problems:
        print(f"  {p}")
    print()
    print(
        "Manifest source of truth is `scripts/check-env-defaults.py::EXPECTED`. "
        "Update it in the same commit as any doc or code default change."
    )
    return 1


if __name__ == "__main__":
    sys.exit(main())
