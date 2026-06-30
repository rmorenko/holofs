# Testszenarien


> ⚠ **Translation may be stale.** This file was last synced before Stage 12-15 (versioning + deletion, HNSW-backed semantic search, /spotlight ROI, streaming /holo, /diff, /similar, auto-repair-on-read, background scrub, typed RPC layer with timeouts + NoLiveNodes panic-fix, per-folder inline upload). The English source under [../](../) is the canon for any new feature; the [Unreleased] block of [../../CHANGELOG.md](../../CHANGELOG.md) lists every delta this translation does not yet cover.


Manuelle Ende-zu-Ende-Checkliste für holofs. Sie deckt alle wichtigen
Teilsysteme ab — CRUD, hierarchischen Katalog, HTTP Range, holografische
Degradation, perzeptuelle Suche, escrow, Persistenz und i18n. Jedes
Szenario führt die Befehle, das erwartete Ergebnis und die
Erfolgskriterien auf.

> Bezieht sich auf den Prototyp v0.4.0. CLI-Flags, Umgebungsvariablen und
> Pfade entsprechen dem Code zum Zeitpunkt der Erstellung; falls etwas
> abweicht, konsultieren Sie
> [docs/operations.md](./operations.md) oder `cargo run -p holofs-web -- --help`.

## Inhalt

1. [Cluster hochfahren](#1-cluster-hochfahren)
2. [Grundlegendes Objekt-CRUD](#2-grundlegendes-objekt-crud)
3. [Hierarchischer Katalog (Stufe 9)](#3-hierarchischer-katalog-stufe-9)
4. [HTTP Range bei GET (Stufe 11.1)](#4-http-range-bei-get-stufe-111)
5. [Holografische Degradation](#5-holografische-degradation)
6. [Perzeptuelle Suche und Diff](#6-perzeptuelle-suche-und-diff)
7. [Inspect: visuelles Shard-Audit](#7-inspect-visuelles-shard-audit)
8. [Holografisches Schlüssel-Escrow](#8-holografisches-schlüssel-escrow)
9. [In-App-Dokumentationsanzeige (Stufe 10)](#9-in-app-dokumentationsanzeige-stufe-10)
10. [i18n: Sprachumschaltung](#10-i18n-sprachumschaltung)
11. [Persistenz und Neustart](#11-persistenz-und-neustart)
12. [Multi-Prozess-Cluster](#12-multi-prozess-cluster)
13. [TLS / mTLS auf der Leitung](#13-tls--mtls-auf-der-leitung)
14. [Metriken, Logs, SSE](#14-metriken-logs-sse)
15. [Regressionsprüfungen für Stufe 11](#15-regressionsprüfungen-für-stufe-11)

---

## 1. Cluster hochfahren

**Ziel.** Einen eingebetteten Cluster (40 nodes in einem Prozess,
4 Zonen) starten und bestätigen, dass jeder node mit leerem Katalog
betriebsbereit ist.

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

**Erfolgskriterien.**

- `GET http://127.0.0.1:8787/` liefert das Katalog-HTML (leeres Raster).
- `GET /api/stats` liefert `{"nodes_total":40,"nodes_live":40,"objects_total":0,…}`.
- 40 Ports lauschen auf 9100..9139 (`lsof -nP -iTCP:9100-9139 -sTCP:LISTEN | wc -l` ≥ 40).

Ohne `--no-seed` befüllt sich der Katalog selbst mit zwei Demobildern
(`photo.png`, `mandala.png`); praktisch für nachgelagerte Szenarien, aber
unpraktisch für saubere CRUD-Tests.

---

## 2. Grundlegendes Objekt-CRUD

**Ziel.** Alle vier unterstützten Arten abdecken — Bild / Audio / Text /
opak — sowie den perzeptuellen Spezialfall der formatübergreifenden
Deduplikation.

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

Jedes PUT liefert das JSON `{"name":…,"object_id":…,"shards":…,"put_ms":…}`.

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

**Erfolgskriterien.**

- Jedes PUT liefert `201 Created` mit einer `object_id` ungleich Null.
- GET liefert die ursprünglichen PNG-/WAV-/Text-Bytes zurück, byte-genau
  bei Bild und opak (Text erlaubt vollständigen Chunk-Verlust, niemals
  einzelne Bytes innerhalb eines Chunks).
- `/api/stats.objects_by_kind` spiegelt die Zählung pro Art wider.
- `curl -X DELETE http://127.0.0.1:8787/photo.png` liefert
  `200 {"deleted":"photo.png",…}` und `objects_total` sinkt.

### Formatübergreifende Deduplikation

```sh
# same frame as PNG and BMP — data_cid is identical
convert assets/sample.png /tmp/sample.bmp
curl -X PUT --data-binary @assets/sample.png http://127.0.0.1:8787/a.png
curl -X PUT --data-binary @/tmp/sample.bmp http://127.0.0.1:8787/a.bmp
curl -s http://127.0.0.1:8787/api/stats | python3 -m json.tool | grep dedup
```

`dedup_savings_pct > 0`, weil verlustfreie Formate dieselbe `data_cid`
erzeugen → shards auf der Festplatte werden dedupliziert.

---

## 3. Hierarchischer Katalog (Stufe 9)

**Ziel.** mkdir, Navigation in Unterverzeichnisse, korrekte Ablehnungen
bei Kollision, Umbenennung und rmdir überprüfen.

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

**UI-Prüfung.** Öffnen Sie `http://127.0.0.1:8787/?p=photos/2026/raw` —
die Breadcrumb sollte `home / photos / 2026 / raw` anzeigen, die
Kachel img.png ist anklickbar und das Formular "+ folder" funktioniert.

**Reservierte Segmente.** `PUT /health/foo`, `PUT /api/foo`,
`PUT /help/foo`, `PUT /inspect-zoom/foo` liefern jeweils `400` — diese
Routen lassen sich nicht überdecken.

---

## 4. HTTP Range bei GET (Stufe 11.1)

**Ziel.** Bestätigen, dass partielle GETs funktionieren — erforderlich
für Audio-Scrubbing, fortsetzbare große Downloads und künftige Video-Seek.

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

**Erfolgskriterien.** Statuscodes und `Content-Range` entsprechen der
obigen Tabelle; die ausgeschnittenen Bytes sind byte-genau (das Muster
`0..255 × 4` liefert für `bytes=256-259` die Bytes `00 01 02 03`).

**Realmedien-Szenario.**

```html
<!-- open in a browser, confirm the seek bar works -->
<audio src="http://127.0.0.1:8787/track.wav" controls></audio>
```

Der Browser sendet bei jedem Seek einen `Range`-Header. Das gateway-Log
zeigt `206 Partial Content`-Antworten.

---

## 5. Holografische Degradation

**Ziel.** Der Vorzeigetrick — wenn ein großer Teil des Clusters ausfällt,
lässt sich die Datei trotzdem **in geringerer Auflösung** decodieren.
Steuern Sie das aus der UI unter `/health/<name>`.

1. Starten Sie mit dem geseedeten `photo.png` (lassen Sie `--no-seed` weg).
2. Öffnen Sie `http://127.0.0.1:8787/health/photo.png`. Sie erhalten eine
   Margin-Tabelle pro `(channel, layer)`, Monte-Carlo-Läufe bei 10/25/50/75%
   Verlust sowie ein Szenario für einen Ganz-Zonen-Ausfall.
3. Öffnen Sie `http://127.0.0.1:8787/health`. Ein Raster mit 40 nodes und
   den Schaltflächen **kill** / **revive**.
4. Beenden Sie nodes nacheinander und beobachten Sie `/health/photo.png`:
   - 10–20% Verlust: Margin überall positiv, PSNR ~99 dB.
   - 30–40% Verlust: Margin von L3 (Detail) → 0, PSNR fällt auf ~30 dB —
     das Bild wird unschärfer.
   - 50–60% Verlust: L2 fällt aus, nur noch L0+L1 übrig — nur grobe Form.
   - 75% Verlust: jede Schicht fällt aus — die Ausgabe kollabiert zu Rauschen.
5. Holen Sie zwischen den Schritten jeweils `GET /photo.png` ab und
   begutachten Sie das PNG.

**Erfolgskriterien.**

- Unterhalb der K-Schwelle jeder Schicht — vollständige Datei.
- Über L3, aber unter L2 — erkennbares Bild mit fehlenden hohen
  Frequenzen (unschärfer).
- Die Margin-Tabelle aktualisiert sich per SSE
  (`/api/health/events`) — die Zahlen verschieben sich nach einem
  Kill ohne Seitenneuladen.

**Wiederherstellung.** Klicken Sie **revive** auf den beendeten nodes.
Nach 1–2 Zyklen des Health-Monitors (`HOLOFS_MONITOR_INTERVAL`,
Standardwert 15 s) läuft die automatische Reparatur, und die Margin kommt
zurück.

### Ganz-Zonen-Ausfall

Jeder node trägt eine `zone` (0..3). Beenden Sie **alle 10 nodes** einer
Zone: dank der zonenbewussten Platzierung (`ceil(n/z)` shards pro Zone)
lässt sich das Objekt weiterhin bis L2 decodieren.

---

## 6. Perzeptuelle Suche und Diff

**Ziel.** Ähnliche Objekte über einen 16-Byte-Perzeptual-Hash finden
und Deduplikation per Diff beobachten.

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

**Diff pro Chunk.**

```
http://127.0.0.1:8787/diff?a=orig.png&b=blurry.png
```

Die UI zeichnet grüne Zellen (übereinstimmende Chunks) und rote
(verschiedene). Zwei identische Kopien → 100% grün plus ein großer
`storage_saved_kb`.

---

## 7. Inspect: visuelles Shard-Audit

**Ziel.** Bestätigen, dass das Raster alle 444 shards (3 Kanäle × 4
Schichten × 26..64 pro Schicht) ohne Lücken anzeigt. Regressionsprüfung
für Stufe 11.2.

1. Öffnen Sie `http://127.0.0.1:8787/inspect/mandala.png`.
2. Scrollen Sie — für jeden Kanal (R, G, B) sollten Sie 4 Abschnitte
   sehen (Schichten 0..3), jeder mit der passenden Vorschauanzahl:
   - L0 — 64 shards (16 systematisch + 48 RLNC)
   - L1 — 40 shards (16 + 24)
   - L2 — 26 shards (16 + 10)
   - L3 — 18 shards (16 + 2)
3. **Alle 444 Vorschauen müssen gerendert werden** (keine kaputten
   `<img>`-Platzhalter). Vor Stufe 11.2 fielen ~8% unter Parallellast aus.
4. Klicken Sie auf eine beliebige Vorschau → Sie landen auf
   `/inspect-zoom/<c_l_idx>/<name>` mit dem großen PNG, den
   Hex-Koeffizienten und der Payload.

**Farbcodierung.** Systematische shards (die ersten K=16 jeder Schicht)
haben einen grünen Rand und tragen sinnvolle Payload (Struktur sichtbar).
RLNC — oranger Rand, Payload sieht nach Rauschen aus.

**Stresstest.** Öffnen Sie 4 Browser-Tabs von `/inspect/photo.png`
gleichzeitig — jeder rendert vollständig. Das gateway-Log darf keine
Zeilen `status=404` für `/api/shard/...` enthalten.

---

## 8. Holografisches Schlüssel-Escrow

**Ziel.** Schwellwertverfahren à la Shamir — eine beliebige Datei in N
Anteile mit Schwellwert K aufteilen und aus beliebigen K rekonstruieren.

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

**Erfolgskriterien.**

- Beliebige **K** von N Anteilen rekonstruieren die Datei exakt
  (byte-genau).
- **K-1** Anteile reichen nicht (recover liefert 400).
- Anteile werden **nicht im Cluster gespeichert** — sie verschwinden mit
  einem gateway-Neustart. Laden Sie sie direkt nach dem Split herunter,
  sonst liefert `/escrow/download/...` ein `410 Gone`.

**UI.** `http://127.0.0.1:8787/escrow` enthält sowohl Split- als auch
Recover-Formular.

---

## 9. In-App-Dokumentationsanzeige (Stufe 10)

**Ziel.** Den Doc-Viewer, Mermaid- und KaTeX-Rendering überprüfen.

1. Öffnen Sie `http://127.0.0.1:8787/help`. Die linke Sidebar enthält
   7 Dokumente, der rechte Bereich zeigt `README.md`.
2. Klicken Sie **Architecture**: `/help/architecture` öffnet sich mit
   einem korrekt gerenderten **Mermaid**-Diagramm (dem
   Crate-Abhängigkeitsgraphen) — SVG, clientseitig über `mermaid.min.js`
   gezeichnet.
3. Klicken Sie **Theory**: viele **KaTeX**-Formeln (`$x^2 + y^2$`,
   `$$E = mc^2$$` usw.) — jede wird gerendert.
4. Am unteren Ende der Sidebar befindet sich ein Sprachumschalter
   (en, ru, de, fr, es). Klicken Sie **Русский** — das Dokument wird neu
   aus `docs/ru/<slug>.md` gerendert. Mermaid und KaTeX funktionieren
   weiterhin (Formeln und Diagramme sind Code, nicht übersetzt).
5. Wenn eine lokalisierte Variante fehlt, liefert das gateway die
   englische (`docs/<slug>.md`) mit `locale: en` in der Metazeile.

**Erfolgskriterien.**

- Alle 7 Dokumente öffnen in allen 5 Sprachen ohne 404.
- Mermaid-Diagramme sind echte SVGs, kein Rohcode in einem `<div>`.
- KaTeX-Formeln erscheinen als gesetzte Mathematik, nicht als TeX-Quelle.
- Die Sidebar hebt das aktive Dokument hervor (Klasse `.active`).

---

## 10. i18n: Sprachumschaltung

**Ziel.** Die UI funktioniert in 5 Sprachen auf jeder Route.

1. Öffnen Sie eine beliebige Seite (`/`, `/help`, `/escrow`).
2. Die rechte Seite der Topbar trägt einen kompakten Umschalter:
   `en · ru · de · fr · es`.
3. Wechseln Sie durch:
   - `?lang=ru` → "каталог", "состояние", "эскроу", "помощь".
   - `?lang=de` → "Katalog", "Zustand", "Treuhand", "Hilfe".
   - `?lang=fr` → "catalogue", "santé", "séquestre", "aide".
   - `?lang=es` → "catálogo", "estado", "depósito", "ayuda".
4. Die URL wird per `rewrite_lang` umgeschrieben — Pfad und andere
   Query-Parameter (`?p=…`, `?a=&b=…`) bleiben erhalten.

**Unbekanntes Locale.** `?lang=ja` oder beliebiges anderes fällt auf
Englisch zurück.

---

## 11. Persistenz und Neustart

**Ziel.** Bestätigen, dass Daten einen Neustart überstehen.

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

**Erfolgskriterien.**

- `catalog.bin` (~KB) und shards (`node_*/<hex>/<hex>.shard`) sind intakt.
- Nach dem Neustart liefert `GET` das Original Byte für Byte zurück.
- Node-Identitäten (`node_*/identity.key`) bleiben stabil — die
  öffentlichen Schlüssel stimmen mit den Werten vor dem Neustart überein.

---

## 12. Multi-Prozess-Cluster

**Ziel.** Den "echten" verteilten Modus erproben — nodes als separate
Prozesse.

```sh
./scripts/spawn-cluster.sh 8 5100 127.0.0.1:8787
```

Das Skript:

1. Startet 8 `holofs-node`-Prozesse mit Storage unter
   `.cluster-data/node-N`.
2. Sammelt deren Ed25519-Pubkeys.
3. Erzeugt ein Admin-Schlüsselpaar und signiert die whitelist.
4. Startet das gateway mit `--whitelist`.

In einem weiteren Terminal:

```sh
curl -X PUT --data-binary @assets/sample.png http://127.0.0.1:8787/test.png

# shards spread across the 8 processes
for i in 0 1 2 3 4 5 6 7; do
    n=$(find .cluster-data/node-$i -name '*.shard' 2>/dev/null | wc -l)
    echo "node-$i: $n shards"
done
```

**Erfolgskriterien.**

- Summe der shards über alle nodes ≈ 444 (×3 Kanäle ×
  Anzahl pro Schicht).
- Ctrl-C im Skript beendet alle 8 nodes und das gateway.
- Erneutes Ausführen desselben Skripts (ohne `.cluster-data/` zu löschen)
  stellt den vorherigen Zustand wieder her — die Daten auf der
  Festplatte sind intakt.

---

## 13. TLS / mTLS auf der Leitung

**Ziel.** Optional aktivierbares TLS für den Verkehr gateway ↔ node
einschalten.

```sh
# embedded mode — a self-signed CA is generated automatically
HOLOFS_TLS=1 cargo run --release --bin holofs-web -- \
    --storage ./holofs-data-tls --no-seed
```

Log: `TLS material self-signed; ca → ./holofs-data-tls/tls/ca.crt`.

Wechselseitige Authentifizierung:

```sh
HOLOFS_TLS=1 HOLOFS_MTLS=1 cargo run --release --bin holofs-web -- \
    --storage ./holofs-data-mtls --no-seed
```

**Was zu überprüfen ist.**

- Der Verkehr auf 9100..9139 ist nicht länger reines TCP — `tcpdump` auf
  dem Loopback zeigt TLS-Handshakes (`16 03 ...`).
- PUT/GET/inspect funktionieren genauso wie ohne TLS.
- Ohne `--tls` bleiben Verbindungen unverschlüsselt — abwärtskompatibel.

PKI-Details und der Ablauf im verteilten Modus mit vom Betreiber
bereitgestellten Zertifikaten finden sich in
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

Etwa alle 3 Sekunden trifft ein Frame `event: health\ndata: {…}\n\n`
mit einem JSON-Snapshot ein — das treibt das Live-`/health`-Dashboard an.

---

## 15. Regressionsprüfungen für Stufe 11

Drei schnelle Sonden für kürzlich behobene Probleme. Führen Sie sie nach
jeder Änderung am gateway oder an der Ingest-Pipeline aus.

### 11.1 Range auf Medien

```sh
curl -i -H "Range: bytes=0-99" http://127.0.0.1:8787/photo.png \
    | head -10
```

Erwartet: `206 Partial Content`, `Content-Range: bytes 0-99/<total>`,
`Content-Length: 100`. Nicht 200, nicht 416.

### 11.2 Inspect verliert keine shards

Öffnen Sie `http://127.0.0.1:8787/inspect/mandala.png` im Browser. Alle
444 Vorschauen müssen rendern. Im Log:

```sh
grep -E "/api/shard/" /tmp/holofs-cluster.log | grep -v "status=200" | wc -l
```

Erwartet: **0**. Vor Stufe 11.2 lag das bei ~41.

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

Beide müssen `200`/`201` liefern, nicht `400 multipart read: Error parsing`.

---

## 16. Regressionsprüfungen Stage 11.16 – 12

### 16.1 Similar-Scope (Stage 11.16)

Drei Scope-Pillen oben auf `/similar/<name>`: **alle Dateien** /
**aktueller Ordner** / **aktueller Ordner (rekursiv)**.

```sh
# unbeschränkt (Legacy — Top-10 über den ganzen Katalog)
curl -s 'http://127.0.0.1:8787/similar/check.txt' \
  | grep -oE 'top similar \(<!>[0-9]+'

# nur Dateien im selben übergeordneten Verzeichnis
curl -s 'http://127.0.0.1:8787/similar/check.txt?scope=folder' \
  | grep -oE 'top similar \(<!>[0-9]+'

# Teilbaum des Elternverzeichnisses (auf Root → ganzer Katalog, wie `all`)
curl -s 'http://127.0.0.1:8787/similar/check.txt?scope=tree' \
  | grep -oE 'top similar \(<!>[0-9]+'
```

Der Scope bleibt klebrig — ein Klick auf einen Nachbarn navigiert zu
dessen `/similar` mit erhaltenem `?scope=` (und `?lang=`).

### 16.2 Katalogfilter + Datei-Löschen (Stage 11.17)

Serverseitiger Filter auf `/` und `/?p=<prefix>` über drei Query-
Parameter: `q` (Namens-Glob, `*` = Wildcard, Basename-Match,
Groß-/Kleinschreibung-egal), `from`, `to` (`YYYY-MM-DD`, Bereich über
`created_at_unix`).

```sh
# alle PNGs
curl -s 'http://127.0.0.1:8787/?q=*.png' \
  | grep -oE 'class="tree-leaf' | wc -l

# kombiniert: Texte aus 2026
curl -s 'http://127.0.0.1:8787/?q=*.txt&from=2026-01-01' \
  | grep -oE 'class="tree-leaf' | wc -l
```

Die Baumansicht behält Vorfahr-Verzeichnisse gefilterter Blätter, damit
Pfade navigierbar bleiben. Legacy-Einträge mit `created_at_unix=0`
(HOLOFSM6/HOLOFSM7) passieren jeden Datumsfilter.

Datei-Löschen ist ein Form-POST-Pendant zu `rmdir_form`:

```sh
curl -s -X PUT 'http://127.0.0.1:8787/tmp-delete-me.txt' \
  -H 'Content-Type: text/plain' --data 'will be deleted'
curl -s -o /dev/null -w '%{http_code}\n' \
  -X POST 'http://127.0.0.1:8787/api/rm' \
  -H 'Content-Type: application/x-www-form-urlencoded' \
  --data 'path=tmp-delete-me.txt&return_to=/'
# erwartet 303 (Redirect auf return_to bei Erfolg)
```

Die Datei-Zeilen im Baum bekommen einen kleinen `✕`-Button mit
Bestätigungs-Prompt.

### 16.3 Lokalisierter Date-Picker (Stage 11.18)

Das native `<input type="date">` trägt jetzt ein `lang`-Attribut
entsprechend der Seitensprache; in Chromium-Browsern ersetzt ein
flatpickr-Overlay (von jsdelivr) den nativen Picker, damit der Kalender
immer die Seitensprache spricht, nicht die OS-Sprache.

`/?lang=de` öffnen, auf ein Datumsfeld klicken — Kalenderkopf auf
Deutsch. Auf `/?lang=fr` wechseln, wiederholen — Französisch. Das
`value=…` bleibt unabhängig von der Locale `YYYY-MM-DD`.

### 16.4 MCP-Server Smoke-Test (Stage 12)

Cluster mit Token starten, damit Schreibwerkzeuge aktiv sind:

```sh
HOLOFS_MCP_TOKEN=devtoken ./holofs-web \
  --storage ./holofs-data --addr 127.0.0.1:8787 \
  --log warn --log-format text
```

MCP-Sitzung initialisieren und alle Werkzeuge auflisten:

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

12 Namen werden erwartet: `diff_objects`, `find_similar`,
`get_cluster_health`, `get_object_health`, `inspect_object`,
`inspect_shard`, `list_catalog`, `mkdir`, `mv_object`,
`put_object_text`, `read_object_text`, `rmdir`.

Auth-Gating:

```sh
# ohne Header → 401
curl -s -o /dev/null -w 'no-auth: %{http_code}\n' \
  -X POST http://127.0.0.1:8787/mcp -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{
    "protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"c","version":"1"}}}'

# gültiger Bearer → 200
curl -s -o /dev/null -w 'with-auth: %{http_code}\n' \
  -X POST http://127.0.0.1:8787/mcp \
  -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{
    "protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"c","version":"1"}}}'
```

Resources-Oberfläche:

```sh
curl -s -X POST http://127.0.0.1:8787/mcp \
  -H "Authorization: Bearer $TOKEN" -H "Mcp-Session-Id: $SID" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":3,"method":"resources/list"}' \
  | grep -oE '"uri":"holofs:///[^"]+"' | head -5
```

Einbindung in Claude Code — siehe [api.md §5](./api.md#5-mcp-server-stage-12).

---

## 17. Wavelet-Operationen (Stage 12.5)

Beide Operationen laufen über denselben MCP-Endpoint (`/mcp`) — die
Session aus §16.4 weiter nutzen. Vor dem Cluster-Start `HOLOFS_MCP_TOKEN`
setzen, damit `save_as` arbeitet.

### 17.1 Wavelet-Mischung

Hybrides PNG aus zwei kompatiblen Bildern bauen und als `hybrid.png`
in den Katalog speichern:

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

# Hybrid herunterladen — sollte ein echtes PNG sein.
curl -s -o /tmp/hybrid.png 'http://127.0.0.1:8787/hybrid.png'
file /tmp/hybrid.png
```

`file /tmp/hybrid.png` sollte ein echtes PNG mit den erwarteten Maßen
melden.

Kompatibilitätsfehler — abweichende Form / k / per-Layer-Parameter
liefern `BadRequest`:

```sh
# Bild + Text mischen → BadRequest aus dem Kind-Check.
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

Audio nur mit Bass (L0) rendern und als neuen Katalogeintrag
speichern:

```sh
# Voraussetzung: ein `track.wav` wurde vorher eingespeist.
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

`keep_layers:[]` oder eine durchgehend-false-Maske → `BadRequest`
(Ergebnis wäre Stille).

### 17.3 Inline-Modus "ohne Kopie"

`save_as` weglassen, um die Bytes als base64-Blob inline zu erhalten —
praktisch, wenn das LLM nur einen Blick auf das Ergebnis werfen soll,
ohne ein Katalog-Artefakt zu hinterlassen:

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

`bytes_len` zeigt die PNG-Größe; `saved_as` sollte fehlen.

---

## Abschluss

Sauberes Stoppen:

```sh
pkill -f 'target/release/holofs-web'
# or Ctrl-C in the terminal running the cluster
```

Sauberes Wischen — gesamter Zustand verwerfen:

```sh
rm -rf ./holofs-data ./.cluster-data
```

Falls sich etwas seltsam verhält, vergleichen Sie mit den obigen
Beschreibungen und konsultieren Sie:

- [docs/operations.md](./operations.md) — Konfiguration und Betrieb
- [docs/architecture.md](./architecture.md) — Datenfluss PUT → GET
- [docs/api.md](./api.md) — HTTP-API, Wire-Protokoll-Format
- [docs/threat-model.md](./threat-model.md) — abgedeckte Bedrohungen

---

## 18+ — Stages 12.6 – 15.0 Szenarien (englische Referenz)

Die folgenden Stages haben jeweils ein eigenes Test-Szenario im
englischen `docs/test-scenarios.md` (Abschnitte 18–25):

- 18 — Quickstart mit dem `tools/test-data/`-Sample-Tree
- 19 — Per-file metrics auf `/health/<name>` (Stage 12.7)
- 20 — CLIP semantic search + Band-Pillen (Stages 12.8/12.9/13.3)
- 21 — Robust-copy-Spalte auf `/similar` (Stage 13.0)
- 22 — Streaming hologram via `/preview/stream/<name>` (Stage 13.1)
- 23 — `/api/spotlight.png?mode=<spatial|coeff>` (Stages 13.2 + 14.1)
- 24 — Per-object versioning + `/api/restore` (Stage 13.4)
- 25 — `POST /api/gc` für Shards + Embeddings (Stages 14.0/3/4)

Deutsche Übersetzungen folgen.  In der Zwischenzeit liefert
`tools/test-data/run-tests.sh` einen sprachlosen End-to-End-Smoke
über alle obigen Punkte (jede Prüfung gibt `✓` / `✗` aus).
