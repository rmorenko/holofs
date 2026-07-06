
# Test Scenarios

Manual end-to-end checklist for holofs. Covers every major subsystem —
CRUD, hierarchical catalog, HTTP Range, holographic degradation,
perceptual search, escrow, persistence, and i18n. Each scenario lists
commands, expected result, and success markers.

> Targets prototype v0.4.0. CLI flags, env vars, and paths reflect the
> code at time of writing; if anything drifts, check
> [docs/operations.md](./operations.md) or `cargo run -p holofs-web -- --help`.

## Contents

1. [Bring up the cluster](#1-bring-up-the-cluster)
2. [Basic object CRUD](#2-basic-object-crud)
3. [Hierarchical catalog](#3-hierarchical-catalog)
4. [HTTP Range on GET (1)](#4-http-range-on-get)
5. [Holographic degradation](#5-holographic-degradation)
6. [Perceptual search and diff](#6-perceptual-search-and-diff)
7. [Inspect: visual shard audit](#7-inspect-visual-shard-audit)
8. [Holographic Key Escrow](#8-holographic-key-escrow)
9. [In-app docs viewer](#9-in-app-docs-viewer)
10. [i18n: language switching](#10-i18n-language-switching)
11. [Persistence and restart](#11-persistence-and-restart)
12. [Multi-process cluster](#12-multi-process-cluster)
13. [TLS / mTLS on the wire](#13-tls--mtls-on-the-wire)
14. [Metrics, logs, SSE](#14-metrics-logs-sse)
15. [Regression checks](#15-regression-checks)

---

## 1. Bring up the cluster

**Goal.** Boot an embedded cluster (40 nodes in one process, 4 zones)
and confirm every node is alive with an empty catalog.

```sh
rm -rf ./holofs-data    # fresh start
cargo run --release --bin holofs-web -- \
    --storage ./holofs-data \
    --addr 127.0.0.1:8787 \
    --log info --log-format text \
    --no-seed
```

Expected log lines:

```
INFO holofs_web: starting holofs-web version=0.4.0 addr=127.0.0.1:8787
INFO holofs_web::bootstrap: embedded cluster ready n_nodes=40 n_zones=4
INFO holofs_web::bootstrap: catalog loaded objects=0
INFO holofs_web: listening addr=127.0.0.1:8787
```

**Success markers.**

- `GET http://127.0.0.1:8787/` returns the catalog HTML (empty grid).
- `GET /api/stats` returns `{"nodes_total":40,"nodes_live":40,"objects_total":0,…}`.
- 40 ports listening on 9100..9139 (`lsof -nP -iTCP:9100-9139 -sTCP:LISTEN | wc -l` ≥ 40).

Without `--no-seed` the catalog seeds itself with two demo images
(`photo.png`, `mandala.png`); handy for downstream scenarios but
inconvenient for clean CRUD tests.

---

## 2. Basic object CRUD

**Goal.** Cover all four supported kinds — image / audio / text / opaque
— plus the perceptual edge case of cross-format dedup.

```sh
# image (PNG → image kind, DWT + RLNC across 4 layers)
curl -X PUT --data-binary @assets/sample.png http://127.0.0.1:8787/photo.png

# text (UTF-8 → text kind, chunked + partial recovery)
printf "Hello, holofs!\nLine 2\n" | curl -X PUT --data-binary @- \
    http://127.0.0.1:8787/note.txt

# opaque (arbitrary binary → 1 RLNC layer, no DWT)
dd if=/dev/urandom of=/tmp/blob.bin bs=1024 count=100 2>/dev/null
curl -X PUT --data-binary @/tmp/blob.bin http://127.0.0.1:8787/blob.bin

# audio (WAV → audio kind, 1D DWT per channel)
# (skip if you don't have a wav file handy)
curl -X PUT --data-binary @track.wav http://127.0.0.1:8787/track.wav
```

Each PUT returns JSON `{"name":…,"object_id":…,"shards":…,"put_ms":…}`.

```sh
# GET — byte-perfect recovery (full-quality decode)
curl -s -o /tmp/got.png http://127.0.0.1:8787/photo.png
cmp assets/sample.png /tmp/got.png && echo "image roundtrip OK"

# Preview = L0 only
curl -s -o /tmp/prev.png http://127.0.0.1:8787/preview/photo.png
file /tmp/prev.png   # should be PNG image

# Catalog stats
curl -s http://127.0.0.1:8787/api/stats | python3 -m json.tool
```

**Success markers.**

- Every PUT returns `201 Created` with a non-zero `object_id`.
- GET returns the original PNG/WAV/text bytes, byte-perfect for image
  and opaque (text allows whole-chunk loss, never bytes within a chunk).
- `/api/stats.objects_by_kind` reflects the per-kind counts.
- `curl -X DELETE http://127.0.0.1:8787/photo.png` returns
  `200 {"deleted":"photo.png",…}` and `objects_total` drops.

### Cross-format dedup

```sh
# same frame as PNG and BMP — data_cid is identical
convert assets/sample.png /tmp/sample.bmp
curl -X PUT --data-binary @assets/sample.png http://127.0.0.1:8787/a.png
curl -X PUT --data-binary @/tmp/sample.bmp http://127.0.0.1:8787/a.bmp
curl -s http://127.0.0.1:8787/api/stats | python3 -m json.tool | grep dedup
```

`dedup_savings_pct > 0` because lossless formats produce the same
`data_cid` → shards on disk are deduplicated.

---

## 3. Hierarchical catalog

**Goal.** Verify mkdir, navigation into subdirs, correct refusals on
collision, rename, rmdir.

```sh
# build a tree
curl -X POST -d "parent=&name=photos"          http://127.0.0.1:8787/api/mkdir
curl -X POST -d "parent=photos&name=2026"      http://127.0.0.1:8787/api/mkdir
curl -X POST -d "parent=photos/2026&name=raw"  http://127.0.0.1:8787/api/mkdir

# upload deep
curl -X PUT --data-binary @assets/sample.png \
    http://127.0.0.1:8787/photos/2026/raw/img.png

# fetch back
curl -o /tmp/got.png http://127.0.0.1:8787/photos/2026/raw/img.png
cmp assets/sample.png /tmp/got.png && echo "deep path roundtrip OK"

# refusals
curl -i -X POST -d "parent=does/not/exist&name=x" \
    http://127.0.0.1:8787/api/mkdir         # 400 parent missing
curl -i -X DELETE http://127.0.0.1:8787/api/rmdir/photos
                                            # 409 directory not empty
curl -i -X DELETE http://127.0.0.1:8787/photos
                                            # 409 is a directory

# rename (carries every descendant)
curl -X POST -d "from=photos&to=archive" http://127.0.0.1:8787/api/mv
curl -s http://127.0.0.1:8787/archive/2026/raw/img.png -o /tmp/r.png
cmp assets/sample.png /tmp/r.png && echo "rename moved descendants"

# rmdir bottom-up
curl -X DELETE http://127.0.0.1:8787/archive/2026/raw/img.png
curl -X DELETE http://127.0.0.1:8787/api/rmdir/archive/2026/raw
curl -X DELETE http://127.0.0.1:8787/api/rmdir/archive/2026
curl -X DELETE http://127.0.0.1:8787/api/rmdir/archive
```

**UI check.** Open `http://127.0.0.1:8787/?p=photos/2026/raw` — breadcrumb
should read `home / photos / 2026 / raw`, the img.png tile is clickable,
the "+ folder" form works.

**Reserved segments.** `PUT /health/foo`, `PUT /api/foo`, `PUT /help/foo`,
`PUT /inspect-zoom/foo` all return `400` — these routes can't be shadowed.

---

## 4. HTTP Range on GET

**Goal.** Confirm partial GETs work — required for audio scrubbing,
resumable large downloads, future video seek.

```sh
# 1000-byte blob
printf 'A%.0s' $(seq 1 1000) > /tmp/blob.bin
curl -X PUT --data-binary @/tmp/blob.bin http://127.0.0.1:8787/range-test

# full GET — 200, Accept-Ranges: bytes
curl -I http://127.0.0.1:8787/range-test 2>&1 | grep -i accept-ranges

# first 10 bytes
curl -i -H "Range: bytes=0-9" http://127.0.0.1:8787/range-test
# expect 206 Partial Content, content-range: bytes 0-9/1000

# last 50 bytes
curl -i -H "Range: bytes=-50" http://127.0.0.1:8787/range-test
# content-range: bytes 950-999/1000

# open interval
curl -i -H "Range: bytes=900-" http://127.0.0.1:8787/range-test
# content-range: bytes 900-999/1000

# past EOF — 416
curl -i -H "Range: bytes=5000-" http://127.0.0.1:8787/range-test
# 416 Range Not Satisfiable, content-range: bytes */1000

# multi-range not supported — degrades to 200 (full body)
curl -i -H "Range: bytes=0-9,20-29" http://127.0.0.1:8787/range-test
# 200 OK, 1000 bytes
```

**Success markers.** Status codes and `Content-Range` match the table
above; sliced bytes are byte-exact (the `0..255 × 4` pattern returns
`00 01 02 03` for `bytes=256-259`).

**Real media scenario.**

```html
<!-- open in a browser, confirm the seek bar works -->
<audio src="http://127.0.0.1:8787/track.wav" controls></audio>
```

The browser sends `Range` on every seek. The gateway log shows
`206 Partial Content` responses.

---

## 5. Holographic degradation

**Goal.** The flagship trick — when a large fraction of the cluster
dies, the file still decodes **at lower resolution**. Drive it from the
UI at `/health/<name>`.

1. Boot with seeded `photo.png` (drop `--no-seed`).
2. Open `http://127.0.0.1:8787/health/photo.png`. You get a margin table
   per `(channel, layer)`, Monte-Carlo runs at 10/25/50/75% loss, and a
   whole-zone-failure scenario.
3. Open `http://127.0.0.1:8787/health`. A grid of 40 nodes with **kill**
   / **revive** buttons.
4. Kill nodes one by one and watch `/health/photo.png`:
   - 10–20% loss: margin positive everywhere, PSNR ~99 dB.
   - 30–40% loss: L3 (detail) margin → 0, PSNR drops to ~30 dB — image
     gets blurrier.
   - 50–60% loss: L2 dies, only L0+L1 remain — coarse shape only.
   - 75% loss: every layer dies — output collapses to noise.
5. Between steps fetch `GET /photo.png` and eyeball the PNG.

**Success markers.**

- Below the K-threshold of every layer — full file.
- Above L3 but below L2 — recognisable image with the high frequencies
  gone (blurrier).
- The margin table updates over SSE (`/api/health/events`) — numbers
  shift after a kill without a page reload.

**Recovery.** Hit **revive** on the killed nodes. After 1-2 cycles of
the health monitor (`HOLOFS_MONITOR_INTERVAL`, default 15s) auto-repair
runs and margin returns.

### Whole-zone failure

Each node carries a `zone` (0..3). Kill **all 10 nodes** of one zone:
the object still decodes up to L2 thanks to zone-aware placement
(`ceil(n/z)` shards per zone).

---

## 6. Perceptual search and diff

**Goal.** Find similar objects by a 16-byte perceptual hash + observe
dedup through diff.

```sh
# upload two similar versions of the same image
curl -X PUT --data-binary @assets/sample.png http://127.0.0.1:8787/orig.png
convert assets/sample.png -gaussian-blur 0x2 /tmp/blurry.png
curl -X PUT --data-binary @/tmp/blurry.png http://127.0.0.1:8787/blurry.png

# top-10 neighbours
curl -s 'http://127.0.0.1:8787/api/fingerprint/orig.png' | python3 -m json.tool
# →  {"name":"orig.png","fingerprint":"a3f0…","kind":"image"}

# UI: open /similar/orig.png — neighbours sorted by L1 distance.
```

**Per-chunk diff.**

```
http://127.0.0.1:8787/diff?a=orig.png&b=blurry.png
```

The UI draws green cells (matching chunks) and red (different). Two
identical copies → 100% green plus a large `storage_saved_kb`.

---

## 7. Inspect: visual shard audit

**Goal.** Confirm the grid shows all 444 shards (3 channels × 4 layers
× 26..64 per layer) without gaps. Regression check for the shard
generation math.

1. Open `http://127.0.0.1:8787/inspect/mandala.png`.
2. Scroll — for each channel (R, G, B) you should see 4 sections
   (layers 0..3), each with the right thumbnail count:
   - L0 — 64 shards (16 systematic + 48 RLNC)
   - L1 — 40 shards (16 + 24)
   - L2 — 26 shards (16 + 10)
   - L3 — 18 shards (16 + 2)
3. **All 444 thumbnails must render** (no broken `<img>` placeholders).
   Before 2 ~8% would drop under concurrent load.
4. Click any thumbnail → land on `/inspect-zoom/<c_l_idx>/<name>` with
   the large PNG, hex coeffs, payload.

**Colour coding.** Systematic shards (first K=16 of each layer) are
green-bordered and carry meaningful payload (structure visible). RLNC —
orange border, payload looks like noise.

**Stress test.** Open 4 browser tabs of `/inspect/photo.png`
simultaneously — every one renders in full. The gateway log must not
contain `status=404` lines for `/api/shard/...`.

---

## 8. Holographic Key Escrow

**Goal.** Shamir-style threshold scheme — split an arbitrary file into N
shares with threshold K, recover from any K.

```sh
echo "my secret seed phrase" > /tmp/secret.txt

# split 3-of-5
curl -F "file=@/tmp/secret.txt" -F "k=3" -F "n=5" \
    -o /tmp/split.html http://127.0.0.1:8787/escrow/split

# the HTML carries /escrow/download/<eid>_<idx>.holoshare links
grep -oE '/escrow/download/[a-f0-9]+_[0-9]+\.holoshare' /tmp/split.html \
    | head -5

# download any 3 shares
for idx in 0 2 4; do
    eid=$(grep -oE '[a-f0-9]{32}' /tmp/split.html | head -1)
    curl -o /tmp/share_${idx}.holoshare \
        "http://127.0.0.1:8787/escrow/download/${eid}_${idx}.holoshare"
done

# recover from 3 shares
curl -F "shares=@/tmp/share_0.holoshare" \
     -F "shares=@/tmp/share_2.holoshare" \
     -F "shares=@/tmp/share_4.holoshare" \
     -o /tmp/recovered.txt http://127.0.0.1:8787/escrow/recover
cmp /tmp/secret.txt /tmp/recovered.txt && echo "escrow roundtrip OK"
```

**Success markers.**

- Any **K** of N shares reconstruct the file exactly (byte-perfect).
- **K-1** shares do not (recover returns 400).
- Shares are **not stored in the cluster** — they vanish on gateway
  restart. Download right after split; otherwise
  `/escrow/download/...` returns `410 Gone`.

**UI.** `http://127.0.0.1:8787/escrow` carries both split and recover
forms.

---

## 9. In-app docs viewer

**Goal.** Verify the docs viewer, Mermaid, and KaTeX rendering.

1. Open `http://127.0.0.1:8787/help`. Left sidebar has 7 documents, the
   right pane shows `README.md`.
2. Click **Architecture**: `/help/architecture` opens with a correctly
   rendered **Mermaid** diagram (the crate dependency graph) — SVG
   drawn client-side via `mermaid.min.js`.
3. Click **Theory**: lots of **KaTeX** formulas (`$x^2 + y^2$`,
   `$$E = mc^2$$`, etc.) — every one rendered.
4. The bottom of the sidebar carries a language switcher
   (en, ru, de, fr, es). Click **Русский** — the doc re-renders from
   `docs/ru/<slug>.md`. Mermaid and KaTeX keep working (formulas and
   diagrams are code, not translated).
5. Where a localized variant is missing, the gateway serves the English
   one (`docs/<slug>.md`) with `locale: en` in the meta line.

**Success markers.**

- All 7 documents open in all 5 languages without 404.
- Mermaid diagrams are real SVGs, not raw code in a `<div>`.
- KaTeX formulas appear as typeset math, not TeX source.
- The sidebar highlights the active document (`.active` class).

---

## 10. i18n: language switching

**Goal.** The UI works in 5 languages on every route.

1. Open any page (`/`, `/help`, `/escrow`).
2. The right side of the topbar carries a compact switcher:
   `en · ru · de · fr · es`.
3. Cycle through:
   - `?lang=ru` → "каталог", "состояние", "эскроу", "помощь".
   - `?lang=de` → "Katalog", "Zustand", "Treuhand", "Hilfe".
   - `?lang=fr` → "catalogue", "santé", "séquestre", "aide".
   - `?lang=es` → "catálogo", "estado", "depósito", "ayuda".
4. The URL is rewritten via `rewrite_lang` — path and other query
   parameters (`?p=…`, `?a=&b=…`) are preserved.

**Unknown locale.** `?lang=ja` or anything else falls back to English.

---

## 11. Persistence and restart

**Goal.** Confirm data survives a restart.

```sh
# 1. seed the cluster
cargo run --release --bin holofs-web -- --storage ./holofs-data --no-seed &
SERVER_PID=$!
sleep 5

curl -X POST -d "parent=&name=keep" http://127.0.0.1:8787/api/mkdir
curl -X PUT --data-binary @assets/sample.png \
    http://127.0.0.1:8787/keep/img.png

# 2. stop
kill $SERVER_PID; wait $SERVER_PID 2>/dev/null

# 3. verify on-disk state is in place
ls -la ./holofs-data/catalog.bin
ls ./holofs-data/node_00 | head -5    # should contain .shard files

# 4. restart
cargo run --release --bin holofs-web -- --storage ./holofs-data --no-seed &
sleep 5

# 5. the object and the catalog came back
curl -s 'http://127.0.0.1:8787/api/list_dir' \
    -X POST -d '' -H 'Content-Type: application/json' \
    --data-raw '{"prefix":""}'

curl -o /tmp/after-restart.png http://127.0.0.1:8787/keep/img.png
cmp assets/sample.png /tmp/after-restart.png && echo "persistence OK"
```

**Success markers.**

- `catalog.bin` (~KB) and shards (`node_*/<hex>/<hex>.shard`) are intact.
- After restart `GET` returns the original byte-for-byte.
- Node identities (`node_*/identity.key`) are stable — pubkeys match
  the pre-restart values.

---

## 12. Multi-process cluster

**Goal.** Exercise the "real" distributed mode — nodes as separate
processes.

```sh
./scripts/spawn-cluster.sh 8 5100 127.0.0.1:8787
```

The script:

1. Spawns 8 `holofs-node` processes with storage under
   `.cluster-data/node-N`.
2. Collects their Ed25519 pubkeys.
3. Generates an admin keypair and signs the whitelist.
4. Starts the gateway with `--whitelist`.

In another terminal:

```sh
curl -X PUT --data-binary @assets/sample.png http://127.0.0.1:8787/test.png

# shards spread across the 8 processes
for i in 0 1 2 3 4 5 6 7; do
    n=$(find .cluster-data/node-$i -name '*.shard' 2>/dev/null | wc -l)
    echo "node-$i: $n shards"
done
```

**Success markers.**

- Sum of shards across nodes ≈ 444 (×3 channels × per-layer counts).
- Ctrl-C on the script stops all 8 nodes and the gateway.
- Re-running the same script (without wiping `.cluster-data/`) restores
  the prior state — data on disk is intact.

---

## 13. TLS / mTLS on the wire

**Goal.** Enable opt-in TLS on the gateway ↔ node traffic.

```sh
# embedded mode — a self-signed CA is generated automatically
HOLOFS_TLS=1 cargo run --release --bin holofs-web -- \
    --storage ./holofs-data-tls --no-seed
```

Log: `TLS material self-signed; ca → ./holofs-data-tls/tls/ca.crt`.

Mutual auth:

```sh
HOLOFS_TLS=1 HOLOFS_MTLS=1 cargo run --release --bin holofs-web -- \
    --storage ./holofs-data-mtls --no-seed
```

**What to verify.**

- Traffic on 9100..9139 is no longer plain TCP — `tcpdump` on loopback
  shows TLS handshakes (`16 03 ...`).
- PUT/GET/inspect work just as without TLS.
- Without `--tls`, connections remain plain — backwards-compatible.

PKI details and the distributed-mode flow with operator-supplied certs
live in [docs/operations.md](./operations.md).

---

## 14. Metrics, logs, SSE

**Prometheus.**

```sh
curl http://127.0.0.1:8787/metrics
```

Expected:

```
# TYPE holofs_nodes_total gauge
holofs_nodes_total 40
holofs_nodes_live 40
holofs_objects_total{kind="image"} 2
holofs_shards_total 888
holofs_dedup_savings_pct 0.00
holofs_node_admin_killed{node="n0",addr="127.0.0.1:9100",zone="0"} 0
...
```

**Structured logs.**

```sh
cargo run --release --bin holofs-web -- \
    --storage ./holofs-data --log-format json --log info
```

Each line is JSON with `timestamp`, `level`, `target`, `fields`.
Convenient for journald / fluentd / Vector / Loki.

**Health SSE stream.**

```sh
curl -N http://127.0.0.1:8787/api/health/events
```

Roughly every 3 seconds an `event: health\ndata: {…}\n\n` frame arrives
with a JSON snapshot — this is what drives the live `/health` dashboard.

---

## 15. regression checks

Three quick probes targeting recently-fixed issues. Run them after any
change to the gateway or the ingest pipeline.

### 11.1 Range on media

```sh
curl -i -H "Range: bytes=0-99" http://127.0.0.1:8787/photo.png \
    | head -10
```

Expect `206 Partial Content`, `Content-Range: bytes 0-99/<total>`,
`Content-Length: 100`. Not 200, not 416.

### 11.2 Inspect doesn't drop shards

Open `http://127.0.0.1:8787/inspect/mandala.png` in a browser. All 444
thumbnails must render. In the log:

```sh
grep -E "/api/shard/" /tmp/holofs-cluster.log | grep -v "status=200" | wc -l
```

Expected **0**. Before 2 this was ~41.

### 11.3 Large multipart uploads

```sh
# 3 MB file via escrow
dd if=/dev/urandom of=/tmp/3mb.bin bs=1024 count=3000 2>/dev/null
curl -i -F "file=@/tmp/3mb.bin" -F "k=3" -F "n=5" \
    http://127.0.0.1:8787/escrow/split 2>&1 | head -1

# 30 MB via PUT
dd if=/dev/urandom of=/tmp/30mb.bin bs=1024 count=30000 2>/dev/null
curl -i -X PUT --data-binary @/tmp/30mb.bin \
    http://127.0.0.1:8787/big.bin 2>&1 | head -1
```

Both must return `200`/`201`, not `400 multipart read: Error parsing`.

---

## 16. – 12 regression checks

### 16.1 Similar scope

Three scope pills at the top of `/similar/<name>`: **all files** /
**current folder** / **current folder (recursive)**.

```sh
# unrestricted (legacy default — top-10 across the catalog)
curl -s 'http://127.0.0.1:8787/similar/check.txt' \
  | grep -oE 'top similar \(<!>[0-9]+'

# only files inside the same parent directory
curl -s 'http://127.0.0.1:8787/similar/check.txt?scope=folder' \
  | grep -oE 'top similar \(<!>[0-9]+'

# subtree of the parent (root → whole catalog, equivalent to `all`)
curl -s 'http://127.0.0.1:8787/similar/check.txt?scope=tree' \
  | grep -oE 'top similar \(<!>[0-9]+'
```

Scope is sticky — clicking a neighbour navigates to its own `/similar`
URL with the same `?scope=` (and `?lang=`) preserved.

### 16.2 Catalog filter + file delete

Server-side filter on `/` and `/?p=<prefix>` via three query params:
`q` (name glob, `*` = wildcard, basename match, case-insensitive),
`from`, `to` (`YYYY-MM-DD`, range over `created_at_unix`).

```sh
# all PNG files
curl -s 'http://127.0.0.1:8787/?q=*.png' \
  | grep -oE 'class="tree-leaf' | wc -l

# combined: text files added in 2026
curl -s 'http://127.0.0.1:8787/?q=*.txt&from=2026-01-01' \
  | grep -oE 'class="tree-leaf' | wc -l
```

The tree view keeps ancestor directories of any retained leaf so paths
stay navigable. Legacy entries with `created_at_unix=0`
(HOLOFSM6/HOLOFSM7) always pass any date filter.

File deletion is a form-POST mirror of the existing `rmdir_form`:

```sh
curl -s -X PUT 'http://127.0.0.1:8787/tmp-delete-me.txt' \
  -H 'Content-Type: text/plain' --data 'will be deleted'
curl -s -o /dev/null -w '%{http_code}\n' \
  -X POST 'http://127.0.0.1:8787/api/rm' \
  -H 'Content-Type: application/x-www-form-urlencoded' \
  --data 'path=tmp-delete-me.txt&return_to=/'
# expect 303 (redirect to return_to on success)
```

The tree leaf rows render a small `✕` button with a confirm prompt.

### 16.3 Localized date picker

Native `<input type="date">` on the filter bar carries a `lang`
attribute matching the page locale; in Chromium browsers a flatpickr
overlay (loaded from jsdelivr) replaces the native picker so the
calendar always speaks the page language, not the OS locale.

Visit `/?lang=ru`, click a date field — the calendar header is in
Russian. Switch to `/?lang=fr`, repeat — French. The `value=…` round
trips as `YYYY-MM-DD` regardless of locale.

### 16.4 MCP server smoke test

Start the cluster with a token so write tools are enabled:

```sh
HOLOFS_MCP_TOKEN=devtoken ./holofs-web \
  --storage ./holofs-data --addr 127.0.0.1:8787 \
  --log warn --log-format text
```

Initialise an MCP session and list every tool:

```sh
TOKEN=devtoken
SID=$(curl -si -X POST http://127.0.0.1:8787/mcp \
  -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{
    "protocolVersion":"2025-06-18","capabilities":{},
    "clientInfo":{"name":"curl","version":"1"}}}' \
  | grep -i 'mcp-session-id:' | awk '{print $2}' | tr -d '\r')

curl -s -X POST http://127.0.0.1:8787/mcp \
  -H "Authorization: Bearer $TOKEN" -H "Mcp-Session-Id: $SID" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","method":"notifications/initialized"}' > /dev/null

curl -s -X POST http://127.0.0.1:8787/mcp \
  -H "Authorization: Bearer $TOKEN" -H "Mcp-Session-Id: $SID" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
  | grep -oE '"name":"[^"]+"' | sort
```

Expect 12 names: `diff_objects`, `find_similar`, `get_cluster_health`,
`get_object_health`, `inspect_object`, `inspect_shard`, `list_catalog`,
`mkdir`, `mv_object`, `put_object_text`, `read_object_text`, `rmdir`.

Auth gating:

```sh
# no header → 401
curl -s -o /dev/null -w 'no-auth: %{http_code}\n' \
  -X POST http://127.0.0.1:8787/mcp -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{
    "protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"c","version":"1"}}}'

# valid bearer → 200
curl -s -o /dev/null -w 'with-auth: %{http_code}\n' \
  -X POST http://127.0.0.1:8787/mcp \
  -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{
    "protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"c","version":"1"}}}'
```

Resources surface:

```sh
curl -s -X POST http://127.0.0.1:8787/mcp \
  -H "Authorization: Bearer $TOKEN" -H "Mcp-Session-Id: $SID" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":3,"method":"resources/list"}' \
  | grep -oE '"uri":"holofs:///[^"]+"' | head -5
```

For wiring into Claude Code see [api.md §5](./api.md#5-mcp-server).

---

## 17. Wavelet operations

Both operations work over the existing MCP endpoint (`/mcp`) — keep
the same session as in §16.4. Set `HOLOFS_MCP_TOKEN` before starting
the cluster so the `save_as` form works.

### 17.1 Wavelet mix

Build a hybrid PNG from two compatible images, save it to the
catalog as `hybrid.png`:

```sh
curl -s -X POST http://127.0.0.1:8787/mcp \
  -H "Authorization: Bearer $TOKEN" -H "Mcp-Session-Id: $SID" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{
    "name":"wavelet_mix","arguments":{
      "a":"photo.png","b":"photo_gray.png","split":2,
      "save_as":"hybrid.png"}}}' \
  | grep -oE '"saved_as":"[^"]*"|"width":[0-9]+|"height":[0-9]+'

# Pull it down to inspect the hybrid — should be a regular PNG.
curl -s -o /tmp/hybrid.png 'http://127.0.0.1:8787/hybrid.png'
file /tmp/hybrid.png
```

`file /tmp/hybrid.png` should report a real PNG image with the
expected dimensions.

Compatibility errors — incompatible shapes / k / per-layer params
return `BadRequest`:

```sh
# Mixing image with text → BadRequest from the kind check.
curl -s -X POST http://127.0.0.1:8787/mcp \
  -H "Authorization: Bearer $TOKEN" -H "Mcp-Session-Id: $SID" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{
    "name":"wavelet_mix","arguments":{
      "a":"photo.png","b":"check.txt","split":0}}}' \
  | grep -oE '"message":"[^"]*"' | head -1
```

### 17.2 Audio layer filter

Render an audio object with only bass (L0) preserved, save as a new
catalog entry:

```sh
# Assumes some `track.wav` ingested earlier.
curl -s -X POST http://127.0.0.1:8787/mcp \
  -H "Authorization: Bearer $TOKEN" -H "Mcp-Session-Id: $SID" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":12,"method":"tools/call","params":{
    "name":"audio_filter","arguments":{
      "path":"track.wav","keep_layers":[0],
      "save_as":"track_bass_only.wav"}}}' \
  | grep -oE '"saved_as":"[^"]*"|"kept_layers":\[[^]]*\]'
```

`keep_layers:[]` or every layer dropped → `BadRequest` (the output
would be silence).

### 17.3 The "no copy" inline mode

Omit `save_as` to get the bytes back inline as a base64 blob — useful
when you want the LLM to look at the result without leaving a
catalog artifact behind:

```sh
curl -s -X POST http://127.0.0.1:8787/mcp \
  -H "Authorization: Bearer $TOKEN" -H "Mcp-Session-Id: $SID" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":13,"method":"tools/call","params":{
    "name":"wavelet_mix","arguments":{
      "a":"photo.png","b":"photo_blurry.png","split":1}}}' \
  | grep -oE '"bytes_len":[0-9]+|"saved_as":"[^"]*"' | head -2
```

`bytes_len` reports the PNG size; `saved_as` should be absent.

---

## 18. Quickstart with the sample tree

`tools/test-data/` ships four pieces that take a fresh checkout
straight to "every feature exercised, every page populated" without
needing to hand-craft input files:

```
tools/test-data/
├── generate-samples.py    # deterministic, dependency-free Python 3.10+
├── clean-cluster.sh       # wipes catalog + shards + embeddings + versions
├── upload-samples.sh      # PUTs the sample tree, preserving hierarchy
└── run-tests.sh           # end-to-end smoke across Stages 12.6–15.0
```

### 18.1 Generate the tree

```sh
python3 tools/test-data/generate-samples.py
# → wrote 38 samples (1,862,535 bytes) under <repo>/samples
```

The output lives under `./samples/` (gitignored). All bytes are
deterministic — re-running with the same args produces byte-identical
files, so version-controlled tests can pin against the exact hashes.

Hierarchy:

```
samples/
  photos/{landscapes,abstract,brand-pairs}/*.png
  audio/{music,effects,silence}/*.wav
  docs/{notes,spec,legal}/{*.txt,*.md,*.json}
  binaries/{archives,blobs}/{*.zip,*.tar,*.bin}
```

The brand-pairs folder includes intentional near-duplicates
(`logo-N.png` + `logo-N-wm.png`) so `/similar`'s robust-copy
column produces hits.

### 18.2 Clean restart

```sh
tools/test-data/clean-cluster.sh
# (FORCE=1 to skip the confirmation prompt)

./target/release/holofs-web \
    --storage ./holofs-data \
    --addr 127.0.0.1:8787 \
    --enable-embed \
    --enable-versions &
```

Without `--enable-embed` the `/search` page renders an "embed
disabled" banner. Without `--enable-versions` PUTs that replace an
existing object purge the prior shards (no archive).

### 18.3 Push the tree

```sh
tools/test-data/upload-samples.sh
```

The script mkdirs every prefix folder first (so `/?p=<dir>` works
straight away), then PUTs every file. Finally it queries `/api/stats`
and prints the new catalog totals — expect `objects_total = 38 +
<directory_markers>` (the upload script's mkdirs are also counted as
directory entries).

### 18.4 End-to-end smoke

```sh
tools/test-data/run-tests.sh
```

What it walks through, by stage:

| Stage  | Check                                              |
|--------|----------------------------------------------------|
| 9      | `/` + `/?p=<folder>` for every subdir              |
| 12.6   | `/mix?a=<image>` renders the wavelet-mix composer  |
| 12.7   | `/health/<name>` per-file metrics                  |
| 12.7   | `/about` marketing page                            |
| 12.8/9 | `/search` UI + `/api/search?band=<any\|coarse\|mid\|full>` |
| 13.0   | `/similar/<brand-pair logo>` includes robust-copy column |
| 13.1   | `/holo/<name>` + `/preview/stream/<name>` multipart |
| 13.2   | `/api/spotlight.png?mode=spatial`                  |
| 14.1   | `/api/spotlight.png?mode=coeff`                    |
| 13.4   | PUT twice → `/versions/<name>` shows archived row  |
| 14.0/3 | `POST /api/gc` returns a `GcReport` JSON           |

Each check prints `✓` / `✗` and the script's exit code is non-zero
if any check fails.

---

## 19. Per-file metrics page

**Goal**: confirm the "Unique metrics" block under
`/health/<name>` populates correctly.

**Steps**:

1. Pick any image from the sample tree, e.g.
   `photos/landscapes/mountain.png`.
2. Visit `http://127.0.0.1:8787/health/photos/landscapes/mountain.png`
   in a browser, or curl the underlying API directly:

   ```sh
   # POST — the endpoint is a leptos server fn, so the name argument
   # rides in the form body, not the query string. A GET returns
   # 405 Method Not Allowed.
   curl -s -X POST -d 'name=photos/landscapes/mountain.png' \
        http://127.0.0.1:8787/api/file_metrics \
        | python3 -m json.tool
   ```

**Expected payload**: a `FileMetricsView` with:

- `total_shards_in_file` ≈ `unique_shards_in_file` (PUT-time
  deduplication doesn't compress within one file's RLNC encoding).
- `catalog_total_shards` ≥ `total_shards_in_file`.
- `originality_pct` somewhere in `[0, 100]`; a sample-tree image
  with no shared structure should be close to 100.
- `originality_per_layer` is a `Vec<f32>` with `nlayers` entries.
- `layer_energy` populated for image / audio; `None` for text / opaque.
- `audio_bands` only present when `kind == "audio"`.
- `neighbours` is empty unless the catalog also contains the same
  bytes under a different name.

**Brand-pair check**: against `photos/brand-pairs/logo-1.png`, the
`neighbours[]` array should list `photos/brand-pairs/logo-1-wm.png`
as the **top** entry (highest `shared_total`) with a non-zero
`shared_per_layer[0]` — i.e., layer-0 (LL / coarse) systematic
shards survive byte-for-byte despite the corner watermark. Other
images in the catalog show `shared_per_layer[0] == 0`. That layer-0
overlap is what feeds the 0 robust-copy score. See §21
for the score-formula caveat on synthetic test data.

---

## 20. CLIP semantic search + bands (Stages 12.8 / 12.9 / 13.3)

**Prereq**: server started with `--enable-embed`. On first call the
gateway downloads ~155 MiB of CLIP weights from HuggingFace into
`~/.cache/huggingface/hub`; subsequent restarts are instant.

**Bulk index** (only needed once after a clean restart):

```sh
curl -s -X POST http://127.0.0.1:8787/api/embed_all
# → {"new":<N>,"skipped":<M>}
```

`new` counts catalog entries newly embedded; `skipped` counts
images whose `(data_cid, band)` was already in `embeddings.bin`
(same content uploaded under multiple paths).

**Per-band query**:

```sh
for band in any coarse mid full; do
  echo "--- band=$band ---"
  curl -s "http://127.0.0.1:8787/api/search?q=mountain&band=$band&limit=3" \
    | python3 -m json.tool
done
```

**Expected outcomes**:

- `band=any` returns the highest-scoring band per file (dedup by
  name).
- `band=coarse` ranks by silhouette / colour blob — landscape
  photos with a horizon line should bubble to the top.
- `band=full` ranks by texture — the noise / pixel-blocks abstracts
  should re-shuffle.
- `band=mid` sits between — gradient images should score well.

**UI surface**: `/search?q=mountain&band=any` shows a card grid
where each card's coarse thumbnail cross-fades to the full
resolution. The card carries a coloured band badge (blue =
coarse, purple = mid, pink = full).

---

## 21. Robust-copy column on `/similar`

**Goal**: detect "structure matches, detail differs" pairs (the
watermark / re-encode / light retouch signature).

**Steps**:

1. Visit `/similar/photos/brand-pairs/logo-1.png`.
2. Scroll to the "shard overlaps" table.

**Expected**:

- `photos/brand-pairs/logo-1-wm.png` is the **top neighbour**
  (highest `shared shards`) — confirms the mechanism: the localized
  bottom-right watermark preserves the bulk of the LL (layer-0)
  systematic shards, so 39+ of those 192 layer-0 shards hash
  identically between the base and the watermarked variant. No
  unrelated image (mandala, gradient, other brand) shares a single
  layer-0 shard.
- `low-band %` > 0 (layer-0 overlap).

**Caveat on the score** (synthetic-test-data limitation, not a bug
in the feature): the `robust copy?` numeric on the seeded sample
tree is **negative** for every brand pair, and the +30 watermark-
warning glyph never lights up here. The reason is that the gateway
upscales 256×256 sample PNGs to its 512×512 working resolution
before encoding; bilinear/bicubic upsampling makes the finest Haar
band (layer 3) almost entirely zero for every smooth synthetic
image. The K=16 systematic shards over those zeros hash to the same
"all-zero" value across **every** image in the sample tree, so each
pair gets a baseline ~36 % `high-band %` that swamps the score
formula. On real photographs with rich high-frequency detail the
score crosses +30 cleanly; on this test set, treat the **top-rank +
non-zero layer-0 overlap** as the success signal, not the absolute
number.

Curl the underlying server function via the page (browsers only):

```sh
curl -s 'http://127.0.0.1:8787/similar/photos/brand-pairs/logo-1.png' \
  | grep -oE 'robust_copy_score":-?[0-9.]+'
```

---

## 22. Streaming hologram

**Goal**: confirm that `/preview/stream/<name>` returns a multipart
body and the browser-side `/holo/<name>` page works.

**Curl probe**:

```sh
curl -sI 'http://127.0.0.1:8787/preview/stream/photos/abstract/mandala-a.png'
# Content-Type should be: multipart/x-mixed-replace; boundary=hololayer-```

**Browser**:

1. Visit `/holo/photos/abstract/mandala-a.png`.
2. Force-reload (Cmd+Shift+R) to bypass the per-(name, layer) PNG
   cache.
3. Watch the image visibly sharpen — first frame in ~tens of
   milliseconds, each subsequent frame adds one DWT layer's worth
   of detail.

**Caveat**: subsequent visits hit the cache and feel instant. The
JavaScript-free `<img>` swap relies on `multipart/x-mixed-replace`,
which Chrome and Firefox handle gracefully.

---

## 23. Holographic spotlight modes

**Goal**: render the same ROI two ways and compare visually.

```sh
img=photos/landscapes/mountain.png
for mode in spatial coeff; do
  curl -s -o "/tmp/spot-$mode.png" \
       "http://127.0.0.1:8787/api/spotlight.png?name=$img&x=0.35&y=0.35&w=0.3&h=0.3&mode=$mode"
done
file /tmp/spot-*.png
md5 /tmp/spot-*.png    # expect distinct hashes
```

**Expected**: two PNGs of the same dimensions but distinct bytes.

- `spatial` keeps the area outside the ROI as a blurry-but-visible
  L0 reconstruction.
- `coeff` keeps non-ROI pixels near black (Haar reverse-map zeros
  every coefficient that doesn't touch the ROI).

**Headers**:

```sh
curl -sI \
  "http://127.0.0.1:8787/api/spotlight.png?name=$img&x=0.35&y=0.35&w=0.3&h=0.3&mode=coeff" \
  | grep -i 'x-holofs'
```

`x-holofs-roi-px` echoes the clamped pixel ROI; `x-holofs-decode-ms`
reports server work; `x-holofs-bytes-downloaded` is informational
(1 will turn it into a real bandwidth-saving number for
`?mode=coeff` on replicated objects).

**UI**: `/spotlight?a=<image>` exposes the mode toggle + ROI
presets + a custom-coordinates form.

---

## 24. Per-object versioning

**Prereq**: server started with `--enable-versions`. Versioned PUTs
SKIP the usual shard purge so storage grows monotonically while
the flag is on. Run `/api/gc` (scenario 25) to reclaim.

**Steps**:

1. Pick a target name, e.g. `samples/photos/abstract/mandala-a.png`
   you've already uploaded.
2. Upload a different image to the same path:

   ```sh
   curl -sf -X PUT \
        --data-binary @samples/photos/abstract/mandala-b.png \
        http://127.0.0.1:8787/photos/abstract/mandala-a.png
   ```

3. Inspect history:

   ```sh
   open 'http://127.0.0.1:8787/versions/photos/abstract/mandala-a.png'
   ```

   Expect at least one archived row dated just now. The CID prefix
   should match the original upload, not the replacement.

4. Click "restore" on the archived row. Confirm at the dialog.

   ```sh
   # Or via curl:
   curl -X POST \
        -d 'name=photos/abstract/mandala-a.png&id=v<TS>_<CIDSHORT>' \
        http://127.0.0.1:8787/api/restore
   ```

5. Re-fetch the image:

   ```sh
   md5 <(curl -sf http://127.0.0.1:8787/photos/abstract/mandala-a.png)
   ```

**Expected**: the post-restore MD5 matches the pre-replace MD5; the
replacement is now itself archived (restore is reversible).

---

## 25. Orphan-shard GC + embedding GC (Stages 14.0 / 14.3 / 14.4)

**Goal**: confirm the gateway reclaims shards no longer referenced
by any live manifest or version archive, AND cleans stale
embeddings out of `embeddings.bin`.

**Steps**:

1. Trigger one PUT-replace pass (scenario 24) so the cluster has
   orphan-able shards.
2. Delete the version side-files for that name (simulates the
   operator removing history):

   ```sh
   rm -rf holofs-data/versions/photos__abstract__mandala-a.png
   ```

   (The script `clean-cluster.sh` does the same wholesale.)

3. Run GC:

   ```sh
   curl -s -X POST http://127.0.0.1:8787/api/gc | python3 -m json.tool
   ```

**Expected**:

- `purged_total` > 0 (the prior shards are now unreferenced).
- `embeddings_dropped` > 0 if any stale CIDs lived in the index.
- `embeddings_kept` matches the number of live `(data_cid, band)`
  records remaining.
- Every node's `ok: true`, no `error` field set.
- `duration_ms` typically < 100 ms on the dev cluster.

**Concurrency check** (optional): run a long PUT + a GC in parallel
and verify both succeed. The RwLock barrier in `Gateway` should
serialise them — GC will wait for the PUT to finish, then run
alone.

```sh
( curl -sf -X PUT --data-binary @samples/photos/landscapes/ocean.png \
       http://127.0.0.1:8787/race-test.png ) &
sleep 0.2
( curl -sf -X POST http://127.0.0.1:8787/api/gc | python3 -m json.tool ) &
wait
# Both should complete; GC's `duration_ms` will include the wait time.
```

---

## 26. .x reliability scenarios

### 26.1 Auto-repair-on-read counters

Goal: verify `decode_with_autorepair`'s retry arm moves the
counters in `/api/stats` only when there's something to repair.

```sh
# Baseline — fresh cluster, healthy.
curl -s http://127.0.0.1:8787/api/stats | jq '.auto_repairs_total, .auto_repair_failures_total'
# 0
# 0

# Light degradation — kill 3 of 40 nodes (well under layer-3 redundancy).
for i in 0 1 2; do curl -X POST -d "i=$i" http://127.0.0.1:8787/admin/node; done
curl -s http://127.0.0.1:8787/photo.png -o /dev/null
curl -s http://127.0.0.1:8787/api/stats | jq '.auto_repairs_total'
# Still 0 — auto-repair should NOT fire under light loss.

# Heavy degradation — kill 60 % of the cluster.
for i in $(seq 3 24); do curl -X POST -d "i=$i" http://127.0.0.1:8787/admin/node; done
curl -s http://127.0.0.1:8787/photo.png -o /dev/null
curl -s http://127.0.0.1:8787/api/stats | jq '.auto_repairs_total, .auto_repair_failures_total'
# At least one counter MUST be ≥ 1.
```

Covered automatically by `crates/holofs-e2e/tests/auto_repair_e2e.rs`.

### 26.2 Background scrub proactively repairs

Goal: prove the scrub catches placement drift before users do.

```sh
# Set scrub to 15 s for the demo (default is 600 s).
HOLOFS_SCRUB_INTERVAL=15 \
  cargo run --release --bin holofs-web
# wait for the first tick:
sleep 20
curl -s http://127.0.0.1:8787/api/stats | jq '.scrub_runs_total'
# 1+ — scrub_repairs_total stays 0 on a healthy cluster.
```

A noisier demo is in
`crates/holofs-e2e/tests/reliability_repair.rs::prometheus_metrics_expose_auto_repair_gauges`.

### 26.3 Cluster-degraded → 503, not panic

Goal: `place_shard` used to assert on an empty live set, crashing
the gateway. Now PUT against a fully-down cluster returns a clean
503.

```sh
# Kill every node.
for i in $(seq 0 39); do curl -X POST -d "i=$i" http://127.0.0.1:8787/admin/node; done
curl -i -X PUT --data-binary @some.png http://127.0.0.1:8787/test.png
# HTTP/1.1 503 Service Unavailable
# content-type: text/plain
# cluster has no live nodes
```

After un-killing the nodes (`POST /admin/node` toggles), the same
PUT succeeds with 2xx.

Covered by `crates/holofs-e2e/tests/cluster_degraded.rs`.

### 26.4 Version deletion + retention cap

Goal: per-name history doesn't grow unbounded.

```sh
HOLOFS_VERSIONS_KEEP_LAST=2 \
  cargo run --release --bin holofs-web -- --enable-versions

# PUT four different images under the same name.
for body in a.png b.png c.png d.png; do
  curl -X PUT --data-binary @$body http://127.0.0.1:8787/test.png
done

# /api/versions_list — at most 2 archives, no matter how many PUTs landed.
curl -s -X POST -d "name=test.png" http://127.0.0.1:8787/api/versions_list | jq '.versions | length'
# 2

# Manual delete of one archive — counter drops to 1.
ID=$(curl -s -X POST -d "name=test.png" http://127.0.0.1:8787/api/versions_list | jq -r '.versions[0].id')
curl -X POST -d "name=test.png&id=$ID&return_to=/" http://127.0.0.1:8787/api/versions/delete
```

Covered by `crates/holofs-e2e/tests/versions_lifecycle.rs`.

### 26.5 cd-into-folder in the catalog tree

Goal: clicking "open →" on a folder shows ONLY that folder's
contents at the top level, with a breadcrumb to navigate back up.

```sh
# Seed a nested tree (the standard sample upload script):
tools/test-data/upload-samples.sh

# Visit the catalog at /. Expand `photos/`, then click "open →" on
# `landscapes-xl`. The URL becomes `/?p=photos/landscapes-xl` and the
# tree now shows the six picsum JPEGs as top-level entries — no
# sibling folders.
xdg-open http://127.0.0.1:8787/?p=photos/landscapes-xl  # linux
open http://127.0.0.1:8787/?p=photos/landscapes-xl      # macos
```

Inline upload + mkdir forms on every `<details>` row land files into
the folder you were looking at; the root toolbar's upload form
scopes itself to the current `?p=<path>` prefix.

Covered by the manual smoke in §18 plus the catalog rendering tests
under `crates/holofs-e2e/tests/ui_catalog.rs`.

### 26.6 Synthetic PNG samples decode cleanly

Goal: the 22-of-29-broken bug is gone.

```sh
tools/test-data/clean-cluster.sh           # fresh storage
HOLOFS_NO_SEED=true \
  cargo run --release --bin holofs-web &
sleep 4
python3 tools/test-data/generate-samples.py
tools/test-data/upload-samples.sh

# Walk every PNG / JPG under samples/ and GET it.
broken=0
for f in $(find samples -type f \( -name '*.png' -o -name '*.jpg' \)); do
  code=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:8787/${f#samples/}")
  [ "$code" != "200" ] && broken=$((broken+1))
done
echo "broken=$broken"
# broken=0
```

The deterministic LSB jitter injected by `write_png` (x)
ensures high-frequency DWT shards are unique per file even on the
smoothest synthetic generators.

---

## Wrap-up

Clean stop:

```sh
pkill -f 'target/release/holofs-web'
# or Ctrl-C in the terminal running the cluster
```

Clean wipe — drop all state:

```sh
tools/test-data/clean-cluster.sh
# or, manually:
rm -rf ./holofs-data ./.cluster-data
```

If anything misbehaves, compare against the descriptions above and
consult:

- [docs/operations.md](./operations.md) — configuration and operations
- [docs/architecture.md](./architecture.md) — PUT → GET data flow
- [docs/api.md](./api.md) — HTTP API, wire-protocol format, new endpoints
- [docs/threat-model.md](./threat-model.md) — covered threats
- `tools/test-data/README.md` — sample-tree + smoke-runner usage
