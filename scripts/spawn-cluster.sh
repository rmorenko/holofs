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
set -euo pipefail

N="${1:-8}"
BASE="${2:-5100}"
GW="${3:-127.0.0.1:8787}"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DATA="$ROOT/.cluster-data"
LOG="$ROOT/.cluster-logs"
mkdir -p "$DATA" "$LOG"

# Билдим всё что нужно (release).
( cd "$ROOT" && cargo build --release --bin holofs-node --bin holofs-admin --bin holofs-http )

BIN_NODE="$ROOT/target/release/holofs-node"
BIN_ADMIN="$ROOT/target/release/holofs-admin"
BIN_HTTP="$ROOT/target/release/holofs-http"

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

# 3. Запускаем гейтвей.
echo "── starting holofs-http @ $GW"
"$BIN_HTTP" \
  --addr "$GW" \
  --whitelist "$WL" \
  --admin-pubkey "$ADMIN_PK" \
  --catalog "$DATA/catalog.bin" \
  --no-seed
