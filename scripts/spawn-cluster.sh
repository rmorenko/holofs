#!/usr/bin/env bash
# Локальное демо реального многопроцессного кластера holofs.
#
# Поднимает N узлов как отдельные процессы (каждый со своим storage и identity),
# собирает их pubkey'и, генерит admin-keypair, подписывает whitelist, запускает
# гейтвей с этим whitelist'ом.
#
# Останавливается по Ctrl-C — все узлы тоже гасятся через trap.
#
# Usage: ./scripts/spawn-cluster.sh [N=8] [BASE_PORT=5100] [GATEWAY=127.0.0.1:8787]
#
# Env-knobs (forwarded to the gateway, unset by default):
#   HOLOFS_ASYNC_ENCODE=1         202 Accepted PUT + Retry-After polling
#   HOLOFS_ENCODE_CONCURRENCY=N   async encoder concurrency cap (default 8)
#   HOLOFS_ENCODE_QUEUE_MAX=N     intake ceiling before AsyncQueueFull
#   HOLOFS_NODE_FSYNC=0|1         per-shard WAL fsync (default 1 = safe)
#   HOLOFS_NODE_FLUSH_INTERVAL_MS N  group-commit interval (default 5 ms)
set -euo pipefail

N="${1:-8}"
BASE="${2:-5100}"
GW="${3:-127.0.0.1:8787}"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DATA="$ROOT/.cluster-data"
LOG="$ROOT/.cluster-logs"
mkdir -p "$DATA" "$LOG"

# Билдим всё что нужно (release). `holofs-web` требует ssr feature.
( cd "$ROOT" \
  && cargo build --release --bin holofs-node --bin holofs-admin \
  && cargo build --release --features ssr --bin holofs-web )

BIN_NODE="$ROOT/target/release/holofs-node"
BIN_ADMIN="$ROOT/target/release/holofs-admin"
BIN_WEB="$ROOT/target/release/holofs-web"

PIDS=()
cleanup() {
  echo
  echo "── shutdown: kill ${#PIDS[@]} процессов"
  for pid in "${PIDS[@]:-}"; do
    kill "$pid" 2>/dev/null || true
  done
  wait 2>/dev/null || true
}
trap cleanup EXIT INT TERM

# 1. Поднимаем N узлов в фоне, каждый со своим storage. Собираем addr+pubkey
#    из их stderr.
echo "── starting $N узлов"
NODE_SPECS=()  # для --node ADDR=PUBKEY_HEX:ZONE
for ((i=0; i<N; i++)); do
  port=$((BASE + i))
  addr="127.0.0.1:$port"
  storage="$DATA/node-$i"
  zone=$((i % 4))
  mkdir -p "$storage"
  log="$LOG/node-$i.log"
  "$BIN_NODE" "$addr" --storage "$storage" 2>"$log" &
  PIDS+=($!)

  # Ждём пока узел напечатает "pubkey=HEX" в свой лог.
  for _ in $(seq 1 50); do
    if grep -q "pubkey=" "$log" 2>/dev/null; then break; fi
    sleep 0.05
  done
  pk=$(grep -oE "pubkey=[a-f0-9]+" "$log" | head -1 | cut -d= -f2 || true)
  if [[ -z "$pk" || "$pk" == "<ephemeral>" ]]; then
    echo "FAIL: не получил pubkey для $addr (см. $log)"
    exit 1
  fi
  echo "   node-$i  $addr  zone=$zone  pubkey=${pk:0:12}…"
  NODE_SPECS+=("--node" "$addr=$pk:$zone")
done

# 2. Генерим admin-keypair и подписываем whitelist.
ADMIN_KEY="$DATA/admin.key"
WL="$DATA/cluster.wl"
if [[ ! -f "$ADMIN_KEY" ]]; then
  "$BIN_ADMIN" gen-key "$ADMIN_KEY" >/dev/null
fi
ADMIN_PK=$("$BIN_ADMIN" pubkey "$ADMIN_KEY")
echo "── admin pubkey: $ADMIN_PK"

"$BIN_ADMIN" sign-whitelist --admin "$ADMIN_KEY" "${NODE_SPECS[@]}" --out "$WL" >/dev/null
echo "── whitelist подписан: $WL"

# 3. Запускаем гейтвей (holofs-web с Leptos SSR UI на / + /api/*).
# --enable-embed включает CLIP-based semantic search индекс. Первый
# `/api/search` PUT-ов ~155 MiB весов CLIP из HuggingFace в
# `~/.cache/huggingface/hub/` (кэшируется между рестартами).
echo "── starting holofs-web @ $GW"
GATEWAY_STORAGE="$DATA/gateway"
mkdir -p "$GATEWAY_STORAGE"
exec "$BIN_WEB" \
  --addr "$GW" \
  --storage "$GATEWAY_STORAGE" \
  --catalog "$DATA/catalog.bin" \
  --whitelist "$WL" \
  --admin-pubkey "$ADMIN_PK" \
  --enable-embed \
  --enable-versions \
  --log info \
  --log-format text
