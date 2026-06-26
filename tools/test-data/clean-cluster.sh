#!/usr/bin/env bash
# tools/test-data/clean-cluster.sh — wipe a holofs cluster's persistent
# state without removing the binary.
#
# Use case: you want to start a clean sample-tree upload from zero.
# The script kills any running holofs-web process, then removes:
#   * the per-node shard dirs under $STORAGE/node_NN
#   * the catalog file at $STORAGE/catalog.bin
#   * the embeddings index at $STORAGE/embeddings.bin (if any)
#   * the versions side-store under $STORAGE/versions (if any)
#
# It does NOT touch:
#   * TLS material (certs/) — re-generated on boot if missing
#   * The HuggingFace model cache (~/.cache/huggingface/hub) — keeping
#     it makes the first /api/search after a clean restart fast
#
# Usage:
#   tools/test-data/clean-cluster.sh                # uses ./holofs-data
#   tools/test-data/clean-cluster.sh /tmp/holofs    # custom storage dir
#
# Set FORCE=1 to skip the "really wipe?" prompt.

set -euo pipefail

STORAGE="${1:-./holofs-data}"
STORAGE_ABS="$(cd "$STORAGE" 2>/dev/null && pwd || echo "$STORAGE")"

echo "About to wipe holofs storage at: $STORAGE_ABS"
if [ -d "$STORAGE_ABS" ]; then
  shard_dirs=$(find "$STORAGE_ABS" -maxdepth 1 -type d -name "node_*" 2>/dev/null | wc -l | tr -d ' ')
  has_catalog=$([ -f "$STORAGE_ABS/catalog.bin" ] && echo yes || echo no)
  has_embed=$([ -f "$STORAGE_ABS/embeddings.bin" ] && echo yes || echo no)
  has_versions=$([ -d "$STORAGE_ABS/versions" ] && echo yes || echo no)
  echo "  shard dirs:    $shard_dirs"
  echo "  catalog.bin:   $has_catalog"
  echo "  embeddings.bin: $has_embed"
  echo "  versions/:     $has_versions"
else
  echo "  (storage dir does not exist; nothing to remove)"
fi

if [ "${FORCE:-0}" != "1" ]; then
  read -r -p "Proceed? [y/N] " ans
  case "$ans" in
    [yY]|[yY][eE][sS]) ;;
    *) echo "aborted."; exit 0 ;;
  esac
fi

echo "→ stopping any running holofs-web…"
pkill -f "holofs-web --storage" 2>/dev/null || true
# Give the OS a moment to release the listening socket.
sleep 1

if [ -d "$STORAGE_ABS" ]; then
  echo "→ removing per-node shard dirs…"
  find "$STORAGE_ABS" -maxdepth 1 -type d -name "node_*" -exec rm -rf {} +
  echo "→ removing catalog.bin…"
  rm -f "$STORAGE_ABS/catalog.bin"
  echo "→ removing embeddings.bin…"
  rm -f "$STORAGE_ABS/embeddings.bin"
  echo "→ removing versions/ side-store…"
  rm -rf "$STORAGE_ABS/versions"
fi

echo "done. Restart the gateway and re-seed with upload-samples.sh."
