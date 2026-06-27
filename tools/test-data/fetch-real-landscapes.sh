#!/usr/bin/env bash
# tools/test-data/fetch-real-landscapes.sh — download real landscape photos
# from picsum.photos (Unsplash-backed, no API key) into
# samples/photos/landscapes-xl/.
#
# Six curated 2560×1440 JPEGs are fetched, each ~280 KB–900 KB. The IDs are
# pinned to specific landscape photos so re-running gives identical content
# (picsum is content-addressed per ID).
#
# Usage:
#   tools/test-data/fetch-real-landscapes.sh
#   SAMPLES=/tmp/my-samples tools/test-data/fetch-real-landscapes.sh
#
# Requires: curl. Exits non-zero if any download fails or returns < 50 KB
# (which would indicate a placeholder / error page instead of a real photo).

set -euo pipefail

SAMPLES="${SAMPLES:-./samples}"
DEST="$SAMPLES/photos/landscapes-xl"
MIN_BYTES=50000

mkdir -p "$DEST"

# id<TAB>filename — curated landscape IDs from picsum.photos.
PHOTOS=$(cat <<'EOF'
1015	norway-fjord.jpg
1018	highland-pass.jpg
1019	stormy-shore.jpg
1036	himalaya-camp.jpg
1037	yosemite-sunset.jpg
1043	yosemite-valley.jpg
EOF
)

echo "fetching real landscapes into: $DEST"

# Drop any leftover synthetic PNGs from prior runs so the directory only
# contains the curated real photos.
find "$DEST" -maxdepth 1 -type f \( -name '*.png' -o -name '*.jpg' \) -delete

fail=0
while IFS=$'\t' read -r id name; do
  [ -z "$id" ] && continue
  out="$DEST/$name"
  url="https://picsum.photos/id/$id/2560/1440"
  if curl -sL --fail --connect-timeout 10 --max-time 60 -o "$out" "$url"; then
    bytes=$(stat -f%z "$out" 2>/dev/null || stat -c%s "$out")
    if [ "$bytes" -lt "$MIN_BYTES" ]; then
      echo "  ✗ $name ($bytes bytes — too small, likely a placeholder)" >&2
      rm -f "$out"
      fail=$((fail + 1))
    else
      printf "  ✓ %-22s %7d bytes  (picsum id=%s)\n" "$name" "$bytes" "$id"
    fi
  else
    echo "  ✗ $name (curl failed)" >&2
    rm -f "$out"
    fail=$((fail + 1))
  fi
done <<<"$PHOTOS"

echo
if [ "$fail" -ne 0 ]; then
  echo "$fail download(s) failed." >&2
  exit 1
fi
echo "all landscapes downloaded into $DEST"
