#!/usr/bin/env bash
# crates/holofs-e2e/scripts/run-tests.sh — macOS / Linux runner.
#
# What it does:
#   1. Confirms the release binary exists (or builds it).
#   2. Starts a chromedriver child process if one isn't already on
#      `localhost:9515`. (Skipped when `HOLOFS_E2E_WEBDRIVER` points
#      at a remote endpoint.)
#   3. Runs `cargo test -p holofs-e2e -- --test-threads=1`.
#   4. Cleans up the chromedriver child on exit.
#
# Env knobs (passed straight through to the harness):
#   HOLOFS_E2E_WEBDRIVER   default http://localhost:9515
#   HOLOFS_E2E_HEADED      set to keep the browser visible
#   HOLOFS_E2E_BINARY      override the gateway binary path
#
# Args after `--` go to cargo test:
#   ./run-tests.sh ui_catalog::tree_loads
#   ./run-tests.sh -- --nocapture

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
cd "$REPO_ROOT"

WEBDRIVER_URL="${HOLOFS_E2E_WEBDRIVER:-http://localhost:9515}"
CHROMEDRIVER_PORT="${CHROMEDRIVER_PORT:-9515}"

# Step 1 — ensure the gateway binary is present.
EXE="target/release/holofs-web"
if [ ! -x "$EXE" ]; then
  echo "→ building holofs-web (release)…"
  cargo build --release --features ssr --bin holofs-web
fi

# Step 2 — start chromedriver if we own its endpoint.
CHILD_PID=""
cleanup() {
  if [ -n "$CHILD_PID" ] && kill -0 "$CHILD_PID" 2>/dev/null; then
    kill "$CHILD_PID" 2>/dev/null || true
    wait "$CHILD_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT

if [[ "$WEBDRIVER_URL" == "http://localhost:$CHROMEDRIVER_PORT" || \
      "$WEBDRIVER_URL" == "http://127.0.0.1:$CHROMEDRIVER_PORT" ]]; then
  if ! curl -sf "$WEBDRIVER_URL/status" >/dev/null 2>&1; then
    if ! command -v chromedriver >/dev/null 2>&1; then
      echo "✗ chromedriver not found on PATH." >&2
      echo "  macOS:   brew install --cask chromedriver" >&2
      echo "  Linux:   apt-get install chromium-chromedriver  (or chromedriver)" >&2
      echo "  CI/alt:  docker-compose -f crates/holofs-e2e/docker-compose.yml up -d" >&2
      exit 2
    fi
    echo "→ starting chromedriver on port $CHROMEDRIVER_PORT…"
    chromedriver --port="$CHROMEDRIVER_PORT" --silent >/tmp/holofs-e2e-chromedriver.log 2>&1 &
    CHILD_PID=$!
    # Wait up to 10 s for it to become ready.
    for _ in $(seq 1 50); do
      if curl -sf "$WEBDRIVER_URL/status" >/dev/null 2>&1; then break; fi
      sleep 0.2
    done
    if ! curl -sf "$WEBDRIVER_URL/status" >/dev/null 2>&1; then
      echo "✗ chromedriver never became ready. See /tmp/holofs-e2e-chromedriver.log" >&2
      exit 3
    fi
  fi
fi

echo "→ webdriver:  $WEBDRIVER_URL"
echo "→ gateway:    $(realpath "$EXE")"

# Step 3 — run the test suite.
# `--test-threads=1` because every harness instance binds its own
# ephemeral port + chromedriver session but the harness itself is
# stateful (chromedriver enforces one-session-per-driver by default).
exec cargo test -p holofs-e2e -- --test-threads=1 "$@"
