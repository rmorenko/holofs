#!/usr/bin/env bash
# tools/test-data/run-tests.sh — end-to-end smoke against a freshly-seeded
# cluster.  Walks through every feature added in Stages 12.6–15.0 and
# prints PASS / FAIL per check.
#
# Prerequisites:
#   * gateway running at $BASE_URL with --enable-embed --enable-versions
#   * tools/test-data/generate-samples.py already produced ./samples
#   * tools/test-data/upload-samples.sh already pushed them into the gateway
#
# This script is read-mostly — it does PUT a couple of files for the
# versioning scenario, then cleans them up at the end.  Existing
# uploads are not modified.
#
# Usage:
#   tools/test-data/run-tests.sh                        # localhost
#   tools/test-data/run-tests.sh http://localhost:9000

set -u

BASE_URL="${1:-http://127.0.0.1:8787}"
PASS=0
FAIL=0

ok()   { printf "  \033[32m✓\033[0m %s\n" "$1"; PASS=$((PASS + 1)); }
bad()  { printf "  \033[31m✗\033[0m %s\n" "$1"; FAIL=$((FAIL + 1)); }
note() { printf "    %s\n" "$1"; }

section() { printf "\n\033[1m%s\033[0m\n" "$1"; }

http_code() {
  curl -sS -o /dev/null -w "%{http_code}" "$@"
}

# --- 1. baseline reachability ---------------------------------------------
section "0. gateway baseline"
code=$(http_code "$BASE_URL/")
if [ "$code" = "200" ]; then ok "GET / returned 200"; else bad "GET / returned $code"; fi

stats=$(curl -sf "$BASE_URL/api/stats" || true)
if [ -n "$stats" ]; then
  obj=$(echo "$stats" | python3 -c "import sys,json; print(json.load(sys.stdin)['objects_total'])")
  ok "/api/stats: $obj objects in catalog"
else
  bad "/api/stats unreachable"
fi

# --- 1. catalog tree -------------------------------------------------------
section "1. catalog & hierarchy (Stage 9 baseline)"
for dir in photos/landscapes audio/music docs/notes binaries/blobs photos/brand-pairs; do
  code=$(http_code "$BASE_URL/?p=$dir")
  if [ "$code" = "200" ]; then ok "/?p=$dir loads"; else bad "/?p=$dir → $code"; fi
done

# --- 2. per-file metrics ---------------------------------------------------
section "2. per-file unique metrics (Stage 12.7)"
target="photos/landscapes/mountain.png"
code=$(http_code "$BASE_URL/health/$target")
if [ "$code" = "200" ]; then ok "/health/$target loads"; else bad "/health/$target → $code"; fi

# --- 3. about page ---------------------------------------------------------
section "3. /about marketing page (Stage 12.7)"
code=$(http_code "$BASE_URL/about")
if [ "$code" = "200" ]; then ok "/about loads"; else bad "/about → $code"; fi

# --- 4. semantic search + bands -------------------------------------------
section "4. CLIP search + hierarchical bands (Stage 12.8/12.9/13.3)"
code=$(http_code "$BASE_URL/search?q=mountain")
if [ "$code" = "200" ]; then ok "/search?q=mountain loads"; else bad "/search → $code"; fi

for band in any coarse mid full; do
  body=$(curl -sf "$BASE_URL/api/search?q=mandala&limit=3&band=$band" || true)
  if echo "$body" | grep -q '"hits"'; then
    n=$(echo "$body" | python3 -c "import sys,json; print(len(json.load(sys.stdin)['hits']))")
    ok "/api/search?q=mandala&band=$band: $n hits"
  else
    bad "/api/search?band=$band missing 'hits'"
  fi
done

# --- 5. robust-copy column on /similar ------------------------------------
section "5. robust-copy detection (Stage 13.0)"
# The brand-pairs uploads include `logo-N` + `logo-N-wm` — same structure,
# different fine layers.  /similar's robust_copy_score should be high.
target="photos/brand-pairs/logo-1.png"
similar_body=$(curl -sf "$BASE_URL/api/similar?name=$target&scope=all" \
               -X POST -d "" 2>/dev/null || true)
# This API is a server fn; raw probe via the rendered page is more
# reliable in shell.  We check the page surface instead.
code=$(http_code "$BASE_URL/similar/$target")
if [ "$code" = "200" ]; then ok "/similar/$target loads"; else bad "/similar → $code"; fi

# --- 6. streaming hologram ------------------------------------------------
section "6. streaming hologram (Stage 13.1)"
code=$(http_code "$BASE_URL/holo/$target")
if [ "$code" = "200" ]; then ok "/holo/$target loads"; else bad "/holo → $code"; fi
ct=$(curl -sI "$BASE_URL/preview/stream/$target" | grep -i "content-type" | tr -d "\r")
if echo "$ct" | grep -q "multipart/x-mixed-replace"; then
  ok "/preview/stream/<name>: multipart/x-mixed-replace"
else
  bad "/preview/stream missing multipart content-type"
  note "got: $ct"
fi

# --- 7. spotlight (both modes) --------------------------------------------
section "7. holographic spotlight (Stage 13.2 + 14.1)"
for mode in spatial coeff; do
  hdr=$(curl -sI "$BASE_URL/api/spotlight.png?name=$target&x=0.3&y=0.3&w=0.4&h=0.4&mode=$mode" \
        | grep -iE "content-type|x-holofs")
  if echo "$hdr" | grep -qi "image/png"; then
    bytes=$(echo "$hdr" | grep -i "x-holofs-bytes-downloaded" | tr -d "\r")
    ok "/api/spotlight.png?mode=$mode returns PNG"
    note "$bytes"
  else
    bad "/api/spotlight.png?mode=$mode did not return image/png"
  fi
done

# --- 8. versions + restore ------------------------------------------------
section "8. per-object versioning (Stage 13.4)"
ver_name="test-versioned-$(date +%s).png"
src1="samples/photos/abstract/mandala-a.png"
src2="samples/photos/abstract/mandala-b.png"
if [ -f "$src1" ] && [ -f "$src2" ]; then
  curl -sS -X PUT --data-binary "@$src1" "$BASE_URL/$ver_name" > /dev/null
  curl -sS -X PUT --data-binary "@$src2" "$BASE_URL/$ver_name" > /dev/null
  body=$(curl -sf "$BASE_URL/versions/$ver_name" || true)
  if echo "$body" | grep -q "archived version"; then
    ok "PUT-twice → /versions/<name> shows an archived row"
  else
    bad "/versions/<name> doesn't list the archived prior PUT"
  fi
  curl -sS -X DELETE "$BASE_URL/$ver_name" > /dev/null
else
  bad "missing test PNGs for versioning scenario"
fi

# --- 9. orphan shard GC ---------------------------------------------------
section "9. orphan-shard GC + embedding GC (Stage 14.0 + 14.3)"
gc=$(curl -sf -X POST "$BASE_URL/api/gc" || true)
if echo "$gc" | grep -q '"purged_total"'; then
  python3 -c "
import sys,json
d = json.load(sys.stdin)
print(f'    live={d[\"live_hashes\"]} held={d[\"held_total\"]} purged={d[\"purged_total\"]} \
emb_kept={d.get(\"embeddings_kept\")} emb_dropped={d.get(\"embeddings_dropped\")} \
ms={d[\"duration_ms\"]}')" <<< "$gc"
  ok "/api/gc returned a GcReport"
else
  bad "/api/gc body unrecognised"
fi

# --- 10. wavelet mix (image-only) -----------------------------------------
section "10. wavelet mix (Stage 12.6)"
code=$(http_code "$BASE_URL/mix?a=photos/abstract/mandala-a.png")
if [ "$code" = "200" ]; then ok "/mix?a=...loads"; else bad "/mix → $code"; fi

# --- summary --------------------------------------------------------------
echo
total=$((PASS + FAIL))
if [ "$FAIL" = "0" ]; then
  printf "\033[32m%d / %d checks passed\033[0m\n" "$PASS" "$total"
  exit 0
else
  printf "\033[31m%d / %d checks failed\033[0m\n" "$FAIL" "$total"
  exit 1
fi
