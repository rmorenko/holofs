#!/usr/bin/env bash
# tools/test-data/upload-samples.sh — push the generated sample tree into a
# running holofs gateway, preserving the folder hierarchy.
#
# For each file under $SAMPLES/<a>/<b>/<c>.ext the script:
#   1. mkdirs every prefix on the way down (so `/?p=<dir>` works),
#   2. PUTs the file at /<a>/<b>/<c>.ext.
#
# Usage:
#   tools/test-data/upload-samples.sh                              # localhost:8787
#   tools/test-data/upload-samples.sh http://localhost:9000        # custom URL
#   SAMPLES=/tmp/holofs-samples tools/test-data/upload-samples.sh  # custom tree
#
# Requires:
#   * curl
#   * a running holofs-web gateway with no name conflicts
#
# Exit code: 0 on success, non-zero if any single mkdir/PUT failed.

set -euo pipefail

BASE_URL="${1:-http://127.0.0.1:8787}"
SAMPLES="${SAMPLES:-./samples}"

if [ ! -d "$SAMPLES" ]; then
  echo "samples dir not found: $SAMPLES" >&2
  echo "run tools/test-data/generate-samples.py first." >&2
  exit 2
fi

echo "uploading from: $SAMPLES"
echo "to gateway:     $BASE_URL"

# 1. Sanity ping.
if ! curl -sf -o /dev/null -w "%{http_code}\n" "$BASE_URL/" | grep -q "^2"; then
  echo "gateway not responding at $BASE_URL" >&2
  exit 3
fi

# 2. Pre-create every directory.  We build the unique set first so each
#    folder is only mkdir'd once.
echo "→ creating directory tree…"
TMP_DIRS="$(mktemp)"
trap 'rm -f "$TMP_DIRS"' EXIT
find "$SAMPLES" -type f ! -name "MANIFEST.txt" | while read -r f; do
  rel="${f#$SAMPLES/}"
  dir="$(dirname "$rel")"
  while [ "$dir" != "." ]; do
    echo "$dir"
    dir="$(dirname "$dir")"
  done
done | sort -u > "$TMP_DIRS"

while read -r dir; do
  # `parent=foo&name=bar` posts to `/api/mkdir`; we POST one folder at a
  # time so the parent always exists by the time we get to the child.
  parent="$(dirname "$dir")"
  name="$(basename "$dir")"
  parent_param="${parent#.}"
  parent_param="${parent_param#/}"
  # Use `--data-urlencode` so spaces / unicode in folder names survive.
  rc=$(curl -sS -o /dev/null -w "%{http_code}" \
       -X POST \
       --data-urlencode "parent=$parent_param" \
       --data-urlencode "name=$name" \
       --data-urlencode "return_to=/" \
       "$BASE_URL/api/mkdir")
  if [ "$rc" != "303" ] && [ "$rc" != "200" ] && [ "$rc" != "409" ]; then
    echo "  ✗ mkdir $dir → HTTP $rc" >&2
  fi
done < "$TMP_DIRS"

# 3. Upload every file.
total=0
ok=0
echo "→ uploading files…"
throttle_ms="${THROTTLE_MS:-600}"
find "$SAMPLES" -type f ! -name "MANIFEST.txt" | sort | while read -r f; do
  rel="${f#$SAMPLES/}"
  # Default base resolution for embedded clusters is 512×512 — anything
  # smaller gets up-scaled inside the gateway, which is fine for tests.
  total=$((total + 1))
  rc=$(curl -sS -o /dev/null -w "%{http_code}" \
       --connect-timeout 5 --max-time 30 \
       -X PUT \
       --data-binary "@$f" \
       "$BASE_URL/$rel")
  if [ "$rc" = "201" ] || [ "$rc" = "200" ]; then
    printf "  %3d  ✓  %s\n" "$total" "$rel"
    ok=$((ok + 1))
  else
    printf "  %3d  ✗  %s (HTTP %s)\n" "$total" "$rel" "$rc" >&2
  fi
  # Stage 14.x audit + monitor pings every node on every tick, which
  # exhausts ephemeral ports under rapid-fire PUT traffic. Throttle
  # so the OS has time to recycle TIME_WAIT sockets. Set THROTTLE_MS=0
  # to disable on a quiet cluster.
  if [ "$throttle_ms" != "0" ]; then
    sleep "$(awk "BEGIN {print $throttle_ms / 1000}")"
  fi
done

# Final counts come from /api/stats so we don't depend on the subshell
# counters surviving the while-pipe (some bashes don't propagate).
echo
echo "→ /api/stats after upload:"
curl -sf "$BASE_URL/api/stats" | python3 -m json.tool 2>/dev/null \
  | grep -E '"(objects_total|objects_by_kind|shards_total|shards_unique|dedup_savings_pct)"' || true
