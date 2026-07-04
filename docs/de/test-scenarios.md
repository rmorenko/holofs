# Testszenarien

Manuelle Ende-zu-Ende-Checkliste für holofs. Deckt jedes große
Subsystem ab — CRUD, hierarchischer Katalog, HTTP-Range, holografische
Degradation, perzeptuelle Suche, Escrow, Persistenz und i18n. Jedes
Szenario listet Kommandos, erwartetes Ergebnis und Erfolgsmarker.

> Zielt auf Prototyp v0.4.0. CLI-Flags, Env-Vars und Pfade spiegeln
> den Code zum Zeitpunkt des Verfassens wider; sollte etwas driften,
> siehe [docs/operations.md](./operations.md) oder
> `cargo run -p holofs-web -- --help`.

## Inhalt

1. [Cluster hochfahren](#1-cluster-hochfahren)
2. [Grundlegendes Objekt-CRUD](#2-grundlegendes-objekt-crud)
3. [Hierarchischer Katalog](#3-hierarchischer-katalog)
4. [HTTP-Range auf GET](#4-http-range-auf-get)
5. [Holografische Degradation](#5-holografische-degradation)
6. [Perzeptuelle Suche und Diff](#6-perzeptuelle-suche-und-diff)
7. [Inspect: visuelles Shard-Audit](#7-inspect-visuelles-shard-audit)
8. [Holografische Schlüsselhinterlegung](#8-holografische-schlüsselhinterlegung)
9. [In-App-Docs-Viewer](#9-in-app-docs-viewer)
10. [i18n: Sprachumschaltung](#10-i18n-sprachumschaltung)
11. [Persistenz und Neustart](#11-persistenz-und-neustart)
12. [Multi-Prozess-Cluster](#12-multi-prozess-cluster)
13. [TLS / mTLS auf dem Draht](#13-tls--mtls-auf-dem-draht)
14. [Metriken, Logs, SSE](#14-metriken-logs-sse)
15. [Regressionsprüfungen](#15-regressionsprüfungen)
16. [Weitere Regressionsprüfungen](#16-weitere-regressionsprüfungen)
17. [Wavelet-Operationen](#17-wavelet-operationen)
18. [Quickstart mit dem Sample-Tree](#18-quickstart-mit-dem-sample-tree)
19. [Per-Datei-Metrik-Seite](#19-per-datei-metrik-seite)
20. [CLIP-semantische Suche + Bänder](#20-clip-semantische-suche--bänder)
21. [Robust-Copy-Spalte auf `/similar`](#21-robust-copy-spalte-auf-similar)
22. [Streaming-Hologramm](#22-streaming-hologramm)
23. [Holografische Spotlight-Modi](#23-holografische-spotlight-modi)
24. [Per-Objekt-Versionierung](#24-per-objekt-versionierung)
25. [Orphan-Shard-GC + Embedding-GC](#25-orphan-shard-gc--embedding-gc)
26. [Reliability-Szenarien](#26-reliability-szenarien)

---

## 1. Cluster hochfahren

**Ziel.** Einen eingebetteten Cluster (40 Nodes in einem Prozess, 4
Zonen) hochfahren und bestätigen, dass jeder Node lebt und der Katalog
leer ist.

```sh
rm -rf ./holofs-data    # fresh start
cargo run --release --bin holofs-web -- \
    --storage ./holofs-data \
    --addr 127.0.0.1:8787 \
    --log info --log-format text \
    --no-seed
```

Erwartete Log-Zeilen:

```
INFO holofs_web: starting holofs-web version=0.4.0 addr=127.0.0.1:8787
INFO holofs_web::bootstrap: embedded cluster ready n_nodes=40 n_zones=4
INFO holofs_web::bootstrap: catalog loaded objects=0
INFO holofs_web: listening addr=127.0.0.1:8787
```

**Erfolgsmarker.**

- `GET http://127.0.0.1:8787/` liefert das HTML des Katalogs (leeres
  Raster).
- `GET /api/stats` gibt
  `{"nodes_total":40,"nodes_live":40,"objects_total":0,…}` zurück.
- 40 Ports lauschen auf 9100..9139
  (`lsof -nP -iTCP:9100-9139 -sTCP:LISTEN | wc -l` ≥ 40).

Ohne `--no-seed` seedet sich der Katalog selbst mit zwei Demo-Bildern
(`photo.png`, `mandala.png`); praktisch für nachgelagerte Szenarien,
aber unpraktisch für saubere CRUD-Tests.

---

## 2. Grundlegendes Objekt-CRUD

**Ziel.** Alle vier unterstützten Arten abdecken — image / audio / text
/ opaque — plus der perzeptuelle Grenzfall des Cross-Format-Dedup.

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

Jeder PUT liefert JSON `{"name":…,"object_id":…,"shards":…,"put_ms":…}`
zurück.

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

**Erfolgsmarker.**

- Jeder PUT gibt `201 Created` mit einer Nicht-Null-`object_id` zurück.
- GET gibt die originalen PNG-/WAV-/Text-Bytes zurück, byte-perfekt für
  image und opaque (text erlaubt Ganz-Chunk-Verlust, niemals Bytes
  innerhalb eines Chunks).
- `/api/stats.objects_by_kind` spiegelt die Per-Art-Zählungen wider.
- `curl -X DELETE http://127.0.0.1:8787/photo.png` liefert
  `200 {"deleted":"photo.png",…}` und `objects_total` sinkt.

### Cross-Format-Dedup

```sh
# same frame as PNG and BMP — data_cid is identical
convert assets/sample.png /tmp/sample.bmp
curl -X PUT --data-binary @assets/sample.png http://127.0.0.1:8787/a.png
curl -X PUT --data-binary @/tmp/sample.bmp http://127.0.0.1:8787/a.bmp
curl -s http://127.0.0.1:8787/api/stats | python3 -m json.tool | grep dedup
```

`dedup_savings_pct > 0`, weil verlustfreie Formate dieselbe `data_cid`
produzieren → Shards auf der Disk werden dedupliziert.

---

## 3. Hierarchischer Katalog

**Ziel.** mkdir, Navigation in Unterverzeichnisse, korrekte
Zurückweisungen bei Kollision, Umbenennen, rmdir verifizieren.

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

**UI-Check.** Öffne `http://127.0.0.1:8787/?p=photos/2026/raw` — der
Breadcrumb sollte `home / photos / 2026 / raw` lauten, das
img.png-Tile ist klickbar, das „+ folder"-Formular funktioniert.

**Reservierte Segmente.** `PUT /health/foo`, `PUT /api/foo`,
`PUT /help/foo`, `PUT /inspect-zoom/foo` geben alle `400` zurück —
diese Routen können nicht überdeckt werden.

---

## 4. HTTP-Range auf GET

**Ziel.** Bestätigen, dass Teil-GETs funktionieren — erforderlich für
Audio-Scrubbing, fortsetzbare große Downloads, künftiges Video-Seek.

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

**Erfolgsmarker.** Statuscodes und `Content-Range` stimmen mit der
Tabelle oben überein; gesliceter Bytes sind byte-exakt (das
`0..255 × 4`-Muster gibt `00 01 02 03` für `bytes=256-259` zurück).

**Real-Media-Szenario.**

```html
<!-- open in a browser, confirm the seek bar works -->
<audio src="http://127.0.0.1:8787/track.wav" controls></audio>
```

Der Browser sendet `Range` bei jedem Seek. Das Gateway-Log zeigt
`206 Partial Content`-Antworten.

---

## 5. Holografische Degradation

**Ziel.** Der Flaggschiff-Trick — wenn ein großer Anteil des Clusters
stirbt, decodiert die Datei immer noch **in geringerer Auflösung**.
Über die UI unter `/health/<name>` betreiben.

1. Mit geseedetem `photo.png` starten (`--no-seed` weglassen).
2. `http://127.0.0.1:8787/health/photo.png` öffnen. Du bekommst eine
   Margentabelle pro `(channel, layer)`, Monte-Carlo-Läufe bei
   10/25/50/75 % Verlust und ein Ganz-Zonen-Ausfall-Szenario.
3. `http://127.0.0.1:8787/health` öffnen. Ein Raster von 40 Nodes mit
   **Kill**- / **Revive**-Buttons.
4. Nodes einen nach dem anderen killen und `/health/photo.png`
   beobachten:
   - 10–20 % Verlust: Marge überall positiv, PSNR ~99 dB.
   - 30–40 % Verlust: L3 (Detail) Marge → 0, PSNR fällt auf ~30 dB —
     Bild wird unschärfer.
   - 50–60 % Verlust: L2 stirbt, nur L0+L1 bleiben — nur grobe Form.
   - 75 % Verlust: jede Schicht stirbt — Ausgabe kollabiert zu
     Rauschen.
5. Zwischen den Schritten `GET /photo.png` abrufen und das PNG per Auge
   prüfen.

**Erfolgsmarker.**

- Unter der K-Schwelle jeder Schicht — volle Datei.
- Über L3, aber unter L2 — erkennbares Bild mit fehlenden hohen
  Frequenzen (unschärfer).
- Die Margentabelle aktualisiert sich per SSE
  (`/api/health/events`) — Zahlen ändern sich nach einem Kill ohne
  Seiten-Reload.

**Wiederherstellung.** Klicke **Revive** auf den gekillten Nodes. Nach
1–2 Zyklen des Health-Monitors (`HOLOFS_MONITOR_INTERVAL`, Default
15 s) läuft Auto-Repair und die Marge kehrt zurück.

### Ganz-Zonen-Ausfall

Jeder Node trägt eine `zone` (0..3). Kille **alle 10 Nodes** einer
Zone: das Objekt decodiert dank Zone-Aware-Platzierung noch bis L2
(`ceil(n/z)` Shards pro Zone).

---

## 6. Perzeptuelle Suche und Diff

**Ziel.** Ähnliche Objekte über einen 16-Byte-perzeptuellen Hash finden
+ Dedup durch Diff beobachten.

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

**Per-Chunk-Diff.**

```
http://127.0.0.1:8787/diff?a=orig.png&b=blurry.png
```

Die UI zeichnet grüne Zellen (übereinstimmende Chunks) und rote
(verschiedene). Zwei identische Kopien → 100 % grün plus großes
`storage_saved_kb`.

---

## 7. Inspect: visuelles Shard-Audit

**Ziel.** Bestätigen, dass das Raster alle 444 Shards (3 Kanäle × 4
Schichten × 26..64 pro Schicht) ohne Lücken zeigt. Regressionscheck
für
1. `http://127.0.0.1:8787/inspect/mandala.png` öffnen.
2. Scrollen — für jeden Kanal (R, G, B) solltest du 4 Abschnitte sehen
   (Schichten 0..3), jeder mit der richtigen Thumbnail-Anzahl:
   - L0 — 64 Shards (16 systematisch + 48 RLNC)
   - L1 — 40 Shards (16 + 24)
   - L2 — 26 Shards (16 + 10)
   - L3 — 18 Shards (16 + 2)
3. **Alle 444 Thumbnails müssen rendern** (keine kaputten
   `<img>`-Platzhalter). Vor 2 würden ~8 % unter gleichzeitiger Last
   ausfallen.
4. Beliebiges Thumbnail anklicken → landen auf
   `/inspect-zoom/<c_l_idx>/<name>` mit dem großen PNG, Hex-Coeffs,
   Payload.

**Farbcodierung.** Systematische Shards (die ersten K=16 jeder Schicht)
haben einen grünen Rand und tragen sinnvollen Payload (Struktur
sichtbar). RLNC — orangener Rand, Payload sieht wie Rauschen aus.

**Stresstest.** Öffne 4 Browser-Tabs von `/inspect/photo.png`
gleichzeitig — jeder rendert vollständig. Das Gateway-Log darf keine
`status=404`-Zeilen für `/api/shard/...` enthalten.

---

## 8. Holografische Schlüsselhinterlegung

**Ziel.** Shamir-artiges Schwellenschema — eine beliebige Datei in N
Anteile mit Schwelle K aufteilen, aus beliebigen K wiederherstellen.

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

**Erfolgsmarker.**

- Beliebige **K** von N Anteilen rekonstruieren die Datei exakt
  (byte-perfekt).
- **K-1** Anteile nicht (Recover liefert 400 zurück).
- Anteile sind **nicht im Cluster gespeichert** — sie verschwinden bei
  Gateway-Neustart. Direkt nach Split herunterladen; sonst gibt
  `/escrow/download/...` `410 Gone` zurück.

**UI.** `http://127.0.0.1:8787/escrow` trägt sowohl Split- als auch
Recover-Formulare.

---

## 9. In-App-Docs-Viewer

**Ziel.** Den Docs-Viewer, Mermaid- und KaTeX-Rendering verifizieren.

1. `http://127.0.0.1:8787/help` öffnen. Linke Seitenleiste hat 7
   Dokumente, das rechte Fenster zeigt `README.md`.
2. Auf **Architecture** klicken: `/help/architecture` öffnet sich mit
   einem korrekt gerenderten **Mermaid**-Diagramm (dem
   Crate-Abhängigkeitsgraph) — SVG clientseitig via `mermaid.min.js`
   gezeichnet.
3. Auf **Theory** klicken: viele **KaTeX**-Formeln (`$x^2 + y^2$`,
   `$$E = mc^2$$`, usw.) — jede gerendert.
4. Der untere Rand der Seitenleiste trägt einen Sprachumschalter
   (en, ru, de, fr, es). Klicke auf **Русский** — das Dokument rendert
   aus `docs/ru/<slug>.md` neu. Mermaid und KaTeX funktionieren
   weiter (Formeln und Diagramme sind Code, nicht übersetzt).
5. Wo eine lokalisierte Variante fehlt, liefert das Gateway die
   englische (`docs/<slug>.md`) mit `locale: en` in der Meta-Zeile.

**Erfolgsmarker.**

- Alle 7 Dokumente öffnen sich in allen 5 Sprachen ohne 404.
- Mermaid-Diagramme sind echte SVGs, nicht Rohcode in einem `<div>`.
- KaTeX-Formeln erscheinen als gesetzte Mathematik, nicht als
  TeX-Quelle.
- Die Seitenleiste hebt das aktive Dokument hervor (`.active`-Klasse).

---

## 10. i18n: Sprachumschaltung

**Ziel.** Die UI funktioniert in 5 Sprachen auf jeder Route.

1. Beliebige Seite öffnen (`/`, `/help`, `/escrow`).
2. Die rechte Seite der Topbar trägt einen kompakten Umschalter:
   `en · ru · de · fr · es`.
3. Durchklicken:
   - `?lang=ru` → „каталог", „состояние", „эскроу", „помощь".
   - `?lang=de` → „Katalog", „Zustand", „Treuhand", „Hilfe".
   - `?lang=fr` → „catalogue", „santé", „séquestre", „aide".
   - `?lang=es` → „catálogo", „estado", „depósito", „ayuda".
4. Die URL wird via `rewrite_lang` umgeschrieben — Pfad und andere
   Query-Parameter (`?p=…`, `?a=&b=…`) bleiben erhalten.

**Unbekannte Locale.** `?lang=ja` oder anderes fällt auf Englisch
zurück.

---

## 11. Persistenz und Neustart

**Ziel.** Bestätigen, dass Daten einen Neustart überleben.

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

**Erfolgsmarker.**

- `catalog.bin` (~KB) und Shards (`node_*/<hex>/<hex>.shard`) sind
  intakt.
- Nach Neustart gibt `GET` das Original byte-für-byte zurück.
- Node-Identitäten (`node_*/identity.key`) sind stabil — Pubkeys
  stimmen mit den Vor-Neustart-Werten überein.

---

## 12. Multi-Prozess-Cluster

**Ziel.** Den „echten" verteilten Modus üben — Nodes als separate
Prozesse.

```sh
./scripts/spawn-cluster.sh 8 5100 127.0.0.1:8787
```

Das Skript:

1. Startet 8 `holofs-node`-Prozesse mit Storage unter
   `.cluster-data/node-N`.
2. Sammelt ihre Ed25519-Pubkeys ein.
3. Erzeugt ein Admin-Keypair und signiert die Whitelist.
4. Startet das Gateway mit `--whitelist`.

In einem anderen Terminal:

```sh
curl -X PUT --data-binary @assets/sample.png http://127.0.0.1:8787/test.png

# shards spread across the 8 processes
for i in 0 1 2 3 4 5 6 7; do
    n=$(find .cluster-data/node-$i -name '*.shard' 2>/dev/null | wc -l)
    echo "node-$i: $n shards"
done
```

**Erfolgsmarker.**

- Summe der Shards über die Nodes ≈ 444 (×3 Kanäle × Per-Layer-
  Zählungen).
- Ctrl-C auf dem Skript stoppt alle 8 Nodes und das Gateway.
- Erneutes Ausführen desselben Skripts (ohne `.cluster-data/` zu
  wischen) stellt den vorherigen Zustand wieder her — Daten auf der
  Disk sind intakt.

---

## 13. TLS / mTLS auf dem Draht

**Ziel.** Opt-in-TLS auf dem Gateway ↔ Node-Verkehr aktivieren.

```sh
# embedded mode — a self-signed CA is generated automatically
HOLOFS_TLS=1 cargo run --release --bin holofs-web -- \
    --storage ./holofs-data-tls --no-seed
```

Log: `TLS material self-signed; ca → ./holofs-data-tls/tls/ca.crt`.

Mutuelle Auth:

```sh
HOLOFS_TLS=1 HOLOFS_MTLS=1 cargo run --release --bin holofs-web -- \
    --storage ./holofs-data-mtls --no-seed
```

**Was zu verifizieren ist.**

- Verkehr auf 9100..9139 ist nicht mehr Plain-TCP — `tcpdump` auf
  Loopback zeigt TLS-Handshakes (`16 03 ...`).
- PUT/GET/inspect funktionieren genauso wie ohne TLS.
- Ohne `--tls` bleiben Verbindungen plain — rückwärtskompatibel.

PKI-Details und der Distributed-Mode-Fluss mit
Operator-bereitgestellten Zertifikaten leben in
[docs/operations.md](./operations.md).

---

## 14. Metriken, Logs, SSE

**Prometheus.**

```sh
curl http://127.0.0.1:8787/metrics
```

Erwartet:

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

**Strukturierte Logs.**

```sh
cargo run --release --bin holofs-web -- \
    --storage ./holofs-data --log-format json --log info
```

Jede Zeile ist JSON mit `timestamp`, `level`, `target`, `fields`.
Praktisch für journald / fluentd / Vector / Loki.

**Health-SSE-Stream.**

```sh
curl -N http://127.0.0.1:8787/api/health/events
```

Ungefähr alle 3 Sekunden trifft ein `event: health\ndata: {…}\n\n`-
Frame mit einem JSON-Snapshot ein — das ist es, was das
Live-`/health`-Dashboard antreibt.

---

## 15. Regressionsprüfungen

Drei schnelle Sonden, die auf kürzlich behobene Issues zielen. Nach
jeder Änderung am Gateway oder der Ingest-Pipeline ausführen.

### 11.1 Range auf Medien

```sh
curl -i -H "Range: bytes=0-99" http://127.0.0.1:8787/photo.png \
    | head -10
```

Erwarte `206 Partial Content`, `Content-Range: bytes 0-99/<total>`,
`Content-Length: 100`. Nicht 200, nicht 416.

### 11.2 Inspect verwirft keine Shards

`http://127.0.0.1:8787/inspect/mandala.png` in einem Browser öffnen.
Alle 444 Thumbnails müssen rendern. Im Log:

```sh
grep -E "/api/shard/" /tmp/holofs-cluster.log | grep -v "status=200" | wc -l
```

Erwartet **0**. Vor 2 war das ~41.

### 11.3 Große Multipart-Uploads

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

Beide müssen `200`/`201` zurückgeben, nicht
`400 multipart read: Error parsing`.

---

## 16. Weitere Regressionsprüfungen

### 16.1 Similar Scope

Drei Scope-Pillen oben auf `/similar/<name>`: **alle Dateien** /
**aktueller Ordner** / **aktueller Ordner (rekursiv)**.

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

Scope ist sticky — Klicken auf einen Nachbarn navigiert zu dessen
eigener `/similar`-URL, wobei dasselbe `?scope=` (und `?lang=`)
erhalten bleibt.

### 16.2 Katalog-Filter + Datei-Löschung

Serverseitiger Filter auf `/` und `/?p=<prefix>` über drei Query-
Parameter: `q` (Namens-Glob, `*` = Wildcard, Basename-Match,
case-insensitiv), `from`, `to` (`YYYY-MM-DD`, Bereich über
`created_at_unix`).

```sh
# all PNG files
curl -s 'http://127.0.0.1:8787/?q=*.png' \
  | grep -oE 'class="tree-leaf' | wc -l

# combined: text files added in 2026
curl -s 'http://127.0.0.1:8787/?q=*.txt&from=2026-01-01' \
  | grep -oE 'class="tree-leaf' | wc -l
```

Die Baumansicht behält Vorfahren-Verzeichnisse jedes behaltenen
Blatts, damit die Pfade navigierbar bleiben. Legacy-Einträge mit
`created_at_unix=0` (HOLOFSM6/HOLOFSM7) passieren immer jeden
Datumsfilter.

Das Datei-Löschen ist ein Form-POST-Spiegel des bestehenden
`rmdir_form`:

```sh
curl -s -X PUT 'http://127.0.0.1:8787/tmp-delete-me.txt' \
  -H 'Content-Type: text/plain' --data 'will be deleted'
curl -s -o /dev/null -w '%{http_code}\n' \
  -X POST 'http://127.0.0.1:8787/api/rm' \
  -H 'Content-Type: application/x-www-form-urlencoded' \
  --data 'path=tmp-delete-me.txt&return_to=/'
# expect 303 (redirect to return_to on success)
```

Die Baum-Blatt-Zeilen rendern einen kleinen `✕`-Button mit einer
Bestätigungsabfrage.

### 16.3 Lokalisierter Datumsauswähler

Nativer `<input type="date">` auf der Filterleiste trägt ein
`lang`-Attribut passend zur Seiten-Locale; in Chromium-Browsern
ersetzt ein flatpickr-Overlay (von jsdelivr geladen) den nativen
Picker, sodass der Kalender immer die Sprache der Seite spricht, nicht
die OS-Locale.

Besuche `/?lang=ru`, klicke ein Datumsfeld an — die Kalender-Kopfzeile
ist auf Russisch. Wechsle zu `/?lang=fr`, wiederhole — Französisch.
Der `value=…` läuft unabhängig von der Locale als `YYYY-MM-DD` durch.

### 16.4 MCP-Server-Smoke-Test

Starte den Cluster mit einem Token, damit Write-Tools aktiviert sind:

```sh
HOLOFS_MCP_TOKEN=devtoken ./holofs-web \
  --storage ./holofs-data --addr 127.0.0.1:8787 \
  --log warn --log-format text
```

Initialisiere eine MCP-Session und liste jedes Tool:

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

Erwarte 12 Namen: `diff_objects`, `find_similar`,
`get_cluster_health`, `get_object_health`, `inspect_object`,
`inspect_shard`, `list_catalog`, `mkdir`, `mv_object`,
`put_object_text`, `read_object_text`, `rmdir`.

Auth-Gating:

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

Ressourcen-Oberfläche:

```sh
curl -s -X POST http://127.0.0.1:8787/mcp \
  -H "Authorization: Bearer $TOKEN" -H "Mcp-Session-Id: $SID" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":3,"method":"resources/list"}' \
  | grep -oE '"uri":"holofs:///[^"]+"' | head -5
```

Zur Verdrahtung in Claude Code siehe
[api.md §5](./api.md#5-mcp-server).

---

## 17. Wavelet-Operationen

Beide Operationen laufen über den bestehenden MCP-Endpunkt (`/mcp`) —
behalte dieselbe Session wie in §16.4 bei. Setze `HOLOFS_MCP_TOKEN`
vor dem Cluster-Start, damit das `save_as`-Formular funktioniert.

### 17.1 Wavelet-Mix

Baue ein hybrides PNG aus zwei kompatiblen Bildern und speichere es im
Katalog als `hybrid.png`:

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

`file /tmp/hybrid.png` sollte ein echtes PNG-Bild mit den erwarteten
Abmessungen melden.

Kompatibilitäts-Fehler — inkompatible Shapes / k / Per-Layer-Params
geben `BadRequest` zurück:

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

### 17.2 Audio-Layer-Filter

Rendere ein Audio-Objekt, wobei nur Bass (L0) erhalten bleibt,
speichere als neuen Katalog-Eintrag:

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

`keep_layers:[]` oder jede Schicht verworfen → `BadRequest` (die
Ausgabe wäre Stille).

### 17.3 Der „No-Copy"-Inline-Modus

`save_as` weglassen, um die Bytes inline als base64-Blob
zurückzubekommen — nützlich, wenn du willst, dass das LLM das Ergebnis
ansieht, ohne einen Katalog-Artefakt zurückzulassen:

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

`bytes_len` meldet die PNG-Größe; `saved_as` sollte fehlen.

---

## 18. Quickstart mit dem Sample-Tree

`tools/test-data/` liefert vier Teile, die einen frischen Checkout
direkt zu „jedes Feature ausgeübt, jede Seite befüllt" bringen, ohne
Eingabedateien von Hand basteln zu müssen:

```
tools/test-data/
├── generate-samples.py    # deterministic, dependency-free Python 3.10+
├── clean-cluster.sh       # wipes catalog + shards + embeddings + versions
├── upload-samples.sh      # PUTs the sample tree, preserving hierarchy
└── run-tests.sh           # end-to-end smoke across Stages 12.6–15.0
```

### 18.1 Den Tree generieren

```sh
python3 tools/test-data/generate-samples.py
# → wrote 38 samples (1,862,535 bytes) under <repo>/samples
```

Die Ausgabe lebt unter `./samples/` (gitignored). Alle Bytes sind
deterministisch — erneutes Ausführen mit denselben Argumenten erzeugt
byte-identische Dateien, sodass versionierte Tests gegen die exakten
Hashes pinnen können.

Hierarchie:

```
samples/
  photos/{landscapes,abstract,brand-pairs}/*.png
  audio/{music,effects,silence}/*.wav
  docs/{notes,spec,legal}/{*.txt,*.md,*.json}
  binaries/{archives,blobs}/{*.zip,*.tar,*.bin}
```

Der brand-pairs-Ordner enthält absichtliche Nahezu-Duplikate
(`logo-N.png` + `logo-N-wm.png`), sodass die
Robust-Copy-Spalte von `/similar` Treffer produziert.

### 18.2 Sauberer Neustart

```sh
tools/test-data/clean-cluster.sh
# (FORCE=1 to skip the confirmation prompt)

./target/release/holofs-web \
    --storage ./holofs-data \
    --addr 127.0.0.1:8787 \
    --enable-embed \
    --enable-versions &
```

Ohne `--enable-embed` rendert die `/search`-Seite ein
„Embed-deaktiviert"-Banner. Ohne `--enable-versions` löschen PUTs, die
ein bestehendes Objekt ersetzen, die vorigen Shards (kein Archiv).

### 18.3 Den Tree pushen

```sh
tools/test-data/upload-samples.sh
```

Das Skript mkdirt jeden Prefix-Ordner zuerst (damit `/?p=<dir>` sofort
funktioniert), dann PUTet es jede Datei. Schließlich fragt es
`/api/stats` ab und druckt die neuen Katalog-Summen — erwarte
`objects_total = 38 + <directory_markers>` (die mkdirs des
Upload-Skripts werden ebenfalls als Verzeichnis-Einträge gezählt).

### 18.4 Ende-zu-Ende-Smoke

```sh
tools/test-data/run-tests.sh
```

Was es durchgeht, nach Stufe:

| Stufe   | Prüfung                                                    |
|---------|------------------------------------------------------------|
| 9       | `/` + `/?p=<folder>` für jedes Unterverzeichnis            |
| 12.6    | `/mix?a=<image>` rendert den Wavelet-Mix-Komponisten       |
| 12.7    | `/health/<name>` Per-Datei-Metriken                        |
| 12.7    | `/about`-Marketing-Seite                                   |
| 12.8/9  | `/search`-UI + `/api/search?band=<any|coarse|mid|full>`   |
| 13.0    | `/similar/<brand-pair logo>` enthält Robust-Copy-Spalte    |
| 13.1    | `/holo/<name>` + `/preview/stream/<name>`-Multipart        |
| 13.2    | `/api/spotlight.png?mode=spatial`                          |
| 14.1    | `/api/spotlight.png?mode=coeff`                            |
| 13.4    | Zweimal PUT → `/versions/<name>` zeigt archivierte Zeile   |
| 14.0/3  | `POST /api/gc` gibt eine `GcReport`-JSON zurück            |

Jede Prüfung gibt `✓` / `✗` aus, und der Exit-Code des Skripts ist
ungleich null, wenn eine Prüfung fehlschlägt.

---

## 19. Per-Datei-Metrik-Seite

**Ziel**: bestätigen, dass der „Unique-Metrics"-Block unter
`/health/<name>` korrekt befüllt wird.

**Schritte**:

1. Wähle ein beliebiges Bild aus dem Sample-Tree, z. B.
   `photos/landscapes/mountain.png`.
2. Besuche
   `http://127.0.0.1:8787/health/photos/landscapes/mountain.png` in
   einem Browser, oder curle die zugrunde liegende API direkt:

   ```sh
   # POST — the endpoint is a leptos server fn, so the name argument
   # rides in the form body, not the query string. A GET returns
   # 405 Method Not Allowed.
   curl -s -X POST -d 'name=photos/landscapes/mountain.png' \
        http://127.0.0.1:8787/api/file_metrics \
        | python3 -m json.tool
   ```

**Erwartetes Payload**: eine `FileMetricsView` mit:

- `total_shards_in_file` ≈ `unique_shards_in_file` (PUT-zeitliches
  Dedup komprimiert nicht innerhalb der RLNC-Codierung einer Datei).
- `catalog_total_shards` ≥ `total_shards_in_file`.
- `originality_pct` irgendwo in `[0, 100]`; ein Sample-Tree-Bild ohne
  geteilte Struktur sollte nahe 100 liegen.
- `originality_per_layer` ist ein `Vec<f32>` mit `nlayers` Einträgen.
- `layer_energy` befüllt für image / audio; `None` für text / opaque.
- `audio_bands` nur vorhanden, wenn `kind == "audio"`.
- `neighbours` ist leer, es sei denn, der Katalog enthält dieselben
  Bytes auch unter einem anderen Namen.

**Brand-Pair-Check**: gegen `photos/brand-pairs/logo-1.png` sollte das
`neighbours[]`-Array `photos/brand-pairs/logo-1-wm.png` als
**Top**-Eintrag auflisten (höchstes `shared_total`) mit einem
Nicht-Null-`shared_per_layer[0]` — d. h. die
Layer-0-(LL-/Coarse-)systematischen Shards überleben byte-für-byte
trotz des Ecken-Wasserzeichens. Andere Bilder im Katalog zeigen
`shared_per_layer[0] == 0`. Dieser Layer-0-Overlap ist das, was den
0-Robust-Copy-Score speist. Siehe §21 zur
Score-Formel-Warnung auf synthetischen Testdaten.

---

## 20. CLIP-semantische Suche + Bänder

**Voraussetzung**: Server mit `--enable-embed` gestartet. Beim ersten
Aufruf lädt das Gateway ~155 MiB CLIP-Gewichte von HuggingFace nach
`~/.cache/huggingface/hub`; nachfolgende Neustarts sind sofort.

**Bulk-Index** (nur einmal nach sauberem Neustart nötig):

```sh
curl -s -X POST http://127.0.0.1:8787/api/embed_all
# → {"new":<N>,"skipped":<M>}
```

`new` zählt neu eingebettete Katalog-Einträge; `skipped` zählt Bilder,
deren `(data_cid, band)` bereits in `embeddings.bin` war (derselbe
Inhalt unter mehreren Pfaden hochgeladen).

**Per-Band-Abfrage**:

```sh
for band in any coarse mid full; do
  echo "--- band=$band ---"
  curl -s "http://127.0.0.1:8787/api/search?q=mountain&band=$band&limit=3" \
    | python3 -m json.tool
done
```

**Erwartete Ergebnisse**:

- `band=any` gibt das am höchsten bewertete Band pro Datei zurück
  (Dedup nach Name).
- `band=coarse` rankt nach Silhouette / Farbklecks —
  Landschaftsfotos mit einer Horizontlinie sollten nach oben blubbern.
- `band=full` rankt nach Textur — die Rausch- / Pixelblock-Abstrakte
  sollten sich neu mischen.
- `band=mid` sitzt dazwischen — Gradientenbilder sollten gut
  abschneiden.

**UI-Oberfläche**: `/search?q=mountain&band=any` zeigt ein Karten-
Raster, wobei das Coarse-Thumbnail jeder Karte in die volle Auflösung
überblendet. Die Karte trägt ein farbiges Band-Badge (blau = coarse,
lila = mid, pink = full).

---

## 21. Robust-Copy-Spalte auf `/similar`

**Ziel**: „Struktur passt, Detail unterscheidet sich"-Paare erkennen
(die Wasserzeichen- / Re-Encode- / Leicht-Retusche-Signatur).

**Schritte**:

1. Besuche `/similar/photos/brand-pairs/logo-1.png`.
2. Scrolle zur „Shard-Overlaps"-Tabelle.

**Erwartet**:

- `photos/brand-pairs/logo-1-wm.png` ist der **Top-Nachbar**
  (höchste `shared shards`) — bestätigt den Mechanismus: das
  lokalisierte Ecken-unten-rechts-Wasserzeichen erhält den Großteil
  der LL-(Layer-0)-systematischen Shards, sodass 39+ dieser 192
  Layer-0-Shards zwischen der Basis und der wasserzeichenversehenen
  Variante identisch hashen. Kein unbezogenes Bild (Mandala, Gradient,
  andere Marke) teilt einen einzigen Layer-0-Shard.
- `low-band %` > 0 (Layer-0-Overlap).

**Warnung zum Score** (Beschränkung synthetischer Testdaten, kein Bug
im Feature): der `robust copy?`-Zahlenwert auf dem geseedeten
Sample-Tree ist für jedes Brand-Paar **negativ**, und die
+30-Wasserzeichen-Warnglyphe leuchtet hier nie auf. Der Grund ist,
dass das Gateway 256×256-Sample-PNGs auf seine 512×512-Arbeits-
Auflösung upsampelt, bevor es codiert; bilineares/bikubisches
Upsampling macht das feinste Haar-Band (Schicht 3) für jedes glatte
synthetische Bild fast vollständig zu Nullen. Die K=16 systematischen
Shards über diesen Nullen hashen auf denselben „All-Zero"-Wert über
**jedes** Bild im Sample-Tree, sodass jedes Paar eine Basislinie von
~36 % `high-band %` bekommt, die die Score-Formel überschwemmt. Auf
echten Fotografien mit reichhaltigem Hochfrequenz-Detail überschreitet
der Score +30 sauber; auf diesem Testset behandle den **Top-Rang +
Nicht-Null-Layer-0-Overlap** als Erfolgssignal, nicht die absolute
Zahl.

Curle die zugrundeliegende Server-Function über die Seite (nur
Browser):

```sh
curl -s 'http://127.0.0.1:8787/similar/photos/brand-pairs/logo-1.png' \
  | grep -oE 'robust_copy_score":-?[0-9.]+'
```

---

## 22. Streaming-Hologramm

**Ziel**: bestätigen, dass `/preview/stream/<name>` einen Multipart-
Body zurückgibt und die browserseitige `/holo/<name>`-Seite
funktioniert.

**Curl-Sonde**:

```sh
curl -sI 'http://127.0.0.1:8787/preview/stream/photos/abstract/mandala-a.png'
# Content-Type should be: multipart/x-mixed-replace; boundary=hololayer-```

**Browser**:

1. Besuche `/holo/photos/abstract/mandala-a.png`.
2. Force-Reload (Cmd+Shift+R), um den Per-(name, layer)-PNG-Cache zu
   umgehen.
3. Beobachte, wie das Bild sichtbar schärfer wird — der erste Frame in
   ~zehn Millisekunden, jeder nachfolgende Frame fügt das Detail einer
   DWT-Schicht hinzu.

**Warnung**: nachfolgende Besuche treffen den Cache und fühlen sich
sofort an. Der JavaScript-freie `<img>`-Swap beruht auf
`multipart/x-mixed-replace`, das Chrome und Firefox anmutig
handhaben.

---

## 23. Holografische Spotlight-Modi

**Ziel**: dieselbe ROI zweifach rendern und visuell vergleichen.

```sh
img=photos/landscapes/mountain.png
for mode in spatial coeff; do
  curl -s -o "/tmp/spot-$mode.png" \
       "http://127.0.0.1:8787/api/spotlight.png?name=$img&x=0.35&y=0.35&w=0.3&h=0.3&mode=$mode"
done
file /tmp/spot-*.png
md5 /tmp/spot-*.png    # expect distinct hashes
```

**Erwartet**: zwei PNGs derselben Abmessungen, aber verschiedener
Bytes.

- `spatial` behält den Bereich außerhalb der ROI als unscharfe, aber
  sichtbare L0-Rekonstruktion.
- `coeff` behält Nicht-ROI-Pixel nahe Schwarz (Haar-Reverse-Map
  nullt jeden Koeffizienten, der die ROI nicht berührt).

**Header**:

```sh
curl -sI \
  "http://127.0.0.1:8787/api/spotlight.png?name=$img&x=0.35&y=0.35&w=0.3&h=0.3&mode=coeff" \
  | grep -i 'x-holofs'
```

`x-holofs-roi-px` gibt die geklampte Pixel-ROI zurück;
`x-holofs-decode-ms` meldet die Server-Arbeit;
`x-holofs-bytes-downloaded` ist informativ (1 wird sie für
`?mode=coeff` auf replizierten Objekten in eine echte
Bandbreiten-Ersparnis-Zahl verwandeln).

**UI**: `/spotlight?a=<image>` exponiert den Modus-Toggle + ROI-
Presets + ein Formular für benutzerdefinierte Koordinaten.

---

## 24. Per-Objekt-Versionierung

**Voraussetzung**: Server mit `--enable-versions` gestartet.
Versionierte PUTs ÜBERSPRINGEN das übliche Shard-Purge, sodass der
Speicher monoton wächst, solange das Flag an ist. Führe `/api/gc`
(Szenario 25) aus, um zurückzugewinnen.

**Schritte**:

1. Wähle einen Zielnamen, z. B.
   `samples/photos/abstract/mandala-a.png`, den du bereits hochgeladen
   hast.
2. Lade ein anderes Bild auf denselben Pfad hoch:

   ```sh
   curl -sf -X PUT \
        --data-binary @samples/photos/abstract/mandala-b.png \
        http://127.0.0.1:8787/photos/abstract/mandala-a.png
   ```

3. Inspiziere die Historie:

   ```sh
   open 'http://127.0.0.1:8787/versions/photos/abstract/mandala-a.png'
   ```

   Erwarte mindestens eine archivierte Zeile mit dem aktuellen Datum.
   Das CID-Präfix sollte mit dem des Original-Uploads übereinstimmen,
   nicht mit dem des Ersatzes.

4. Klicke „restore" auf der archivierten Zeile. Bestätige im Dialog.

   ```sh
   # Or via curl:
   curl -X POST \
        -d 'name=photos/abstract/mandala-a.png&id=v<TS>_<CIDSHORT>' \
        http://127.0.0.1:8787/api/restore
   ```

5. Hole das Bild neu:

   ```sh
   md5 <(curl -sf http://127.0.0.1:8787/photos/abstract/mandala-a.png)
   ```

**Erwartet**: der Post-Restore-MD5 stimmt mit dem Pre-Replace-MD5
überein; der Ersatz ist nun selbst archiviert (Restore ist
reversibel).

---

## 25. Orphan-Shard-GC + Embedding-GC

**Ziel**: bestätigen, dass das Gateway Shards zurückgewinnt, die von
keinem lebenden Manifest oder Versions-Archiv referenziert werden, UND
stale Embeddings aus `embeddings.bin` bereinigt.

**Schritte**:

1. Löse einen PUT-Replace-Pass aus (Szenario 24), sodass der Cluster
   verwaisbare Shards hat.
2. Lösche die Version-Seitendateien für diesen Namen (simuliert den
   Operator, der die Historie entfernt):

   ```sh
   rm -rf holofs-data/versions/photos__abstract__mandala-a.png
   ```

   (Das Skript `clean-cluster.sh` erledigt dasselbe pauschal.)

3. Führe GC aus:

   ```sh
   curl -s -X POST http://127.0.0.1:8787/api/gc | python3 -m json.tool
   ```

**Erwartet**:

- `purged_total` > 0 (die vorigen Shards sind nun nicht referenziert).
- `embeddings_dropped` > 0, wenn stale CIDs im Index lebten.
- `embeddings_kept` stimmt mit der Anzahl der verbleibenden lebenden
  `(data_cid, band)`-Records überein.
- Jedes `ok: true` des Nodes, kein `error`-Feld gesetzt.
- `duration_ms` typisch < 100 ms auf dem Dev-Cluster.

**Nebenläufigkeits-Check** (optional): einen langen PUT + eine GC
parallel ausführen und verifizieren, dass beide erfolgreich sind. Die
RwLock-Barriere in `Gateway` sollte sie serialisieren — GC wartet, bis
der PUT beendet ist, und läuft dann allein.

```sh
( curl -sf -X PUT --data-binary @samples/photos/landscapes/ocean.png \
       http://127.0.0.1:8787/race-test.png ) &
sleep 0.2
( curl -sf -X POST http://127.0.0.1:8787/api/gc | python3 -m json.tool ) &
wait
# Both should complete; GC's `duration_ms` will include the wait time.
```

---

## 26. Reliability-Szenarien

### 26.1 Auto-Repair-on-Read-Zähler

Ziel: verifizieren, dass der Retry-Arm von `decode_with_autorepair`
die Zähler in `/api/stats` nur bewegt, wenn es etwas zu reparieren
gibt.

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

Automatisch abgedeckt von
`crates/holofs-e2e/tests/auto_repair_e2e.rs`.

### 26.2 Hintergrund-Scrub repariert proaktiv

Ziel: beweisen, dass der Scrub Placement-Drift abfängt, bevor Nutzer
es tun.

```sh
# Set scrub to 15 s for the demo (default is 600 s).
HOLOFS_SCRUB_INTERVAL=15 \
  cargo run --release --bin holofs-web
# wait for the first tick:
sleep 20
curl -s http://127.0.0.1:8787/api/stats | jq '.scrub_runs_total'
# 1+ — scrub_repairs_total stays 0 on a healthy cluster.
```

Eine lautere Demo liegt in
`crates/holofs-e2e/tests/reliability_repair.rs::prometheus_metrics_expose_auto_repair_gauges`.

### 26.3 Cluster-Degraded → 503, kein Panic

Ziel: `place_shard` hat früher auf ein leeres Live-Set asserted und
das Gateway zum Absturz gebracht. Jetzt gibt PUT gegen einen
vollständig ausgefallenen Cluster ein sauberes 503 zurück.

```sh
# Kill every node.
for i in $(seq 0 39); do curl -X POST -d "i=$i" http://127.0.0.1:8787/admin/node; done
curl -i -X PUT --data-binary @some.png http://127.0.0.1:8787/test.png
# HTTP/1.1 503 Service Unavailable
# content-type: text/plain
# cluster has no live nodes
```

Nach Un-Killing der Nodes (`POST /admin/node` schaltet um) gelingt
derselbe PUT mit 2xx.

Abgedeckt von `crates/holofs-e2e/tests/cluster_degraded.rs`.

### 26.4 Version-Löschung + Retention-Cap

Ziel: Per-Name-Historie wächst nicht unbegrenzt.

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

Abgedeckt von `crates/holofs-e2e/tests/versions_lifecycle.rs`.

### 26.5 cd-into-folder im Katalog-Tree

Ziel: das Klicken auf „open →" auf einem Ordner zeigt NUR die Inhalte
dieses Ordners auf der obersten Ebene, mit einem Breadcrumb, um wieder
nach oben zu navigieren.

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

Inline-Upload- + mkdir-Formulare auf jeder `<details>`-Zeile landen
Dateien in dem Ordner, den du gerade betrachtet hast; das
Upload-Formular der Wurzel-Toolbar scoped sich auf das aktuelle
`?p=<path>`-Prefix.

Abgedeckt vom manuellen Smoke in §18 plus den Katalog-Rendering-Tests
unter `crates/holofs-e2e/tests/ui_catalog.rs`.

### 26.6 Synthetische PNG-Samples decodieren sauber

Ziel: der 22-von-29-kaputt-Bug ist weg.

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

Das deterministische LSB-Jitter, das von `write_png` injiziert wird
(x), stellt sicher, dass Hochfrequenz-DWT-Shards pro Datei
selbst auf den glattesten synthetischen Generatoren eindeutig sind.

---

## Abschluss

Sauberer Stopp:

```sh
pkill -f 'target/release/holofs-web'
# or Ctrl-C in the terminal running the cluster
```

Sauberes Wischen — allen Zustand löschen:

```sh
tools/test-data/clean-cluster.sh
# or, manually:
rm -rf ./holofs-data ./.cluster-data
```

Falls sich etwas fehlverhält, gegen die obigen Beschreibungen
vergleichen und konsultieren:

- [docs/operations.md](./operations.md) — Konfiguration und Betrieb
- [docs/architecture.md](./architecture.md) — PUT → GET-Datenfluss
- [docs/api.md](./api.md) — HTTP-API, Wire-Protokoll-Format, neue
  Endpunkte
- [docs/threat-model.md](./threat-model.md) — abgedeckte Bedrohungen
- `tools/test-data/README.md` — Sample-Tree- + Smoke-Runner-Nutzung
