# Scénarios de test

Liste de contrôle manuelle de bout en bout pour holofs. Couvre tous les
sous-systèmes majeurs — CRUD, catalogue hiérarchique, HTTP Range,
dégradation holographique, recherche perceptuelle, escrow, persistance
et i18n. Chaque scénario indique les commandes, le résultat attendu et
les critères de réussite.

> Cible : prototype v0.4.0. Les options CLI, variables d'environnement
> et chemins reflètent le code au moment de la rédaction ; en cas
> d'écart, consultez [docs/operations.md](./operations.md) ou
> `cargo run -p holofs-web -- --help`.

## Sommaire

1. [Démarrer le cluster](#1-bring-up-the-cluster)
2. [CRUD d'objets basique](#2-basic-object-crud)
3. [Catalogue hiérarchique (étape 9)](#3-hierarchical-catalog-stage-9)
4. [HTTP Range sur GET (étape 11.1)](#4-http-range-on-get-stage-111)
5. [Dégradation holographique](#5-holographic-degradation)
6. [Recherche perceptuelle et diff](#6-perceptual-search-and-diff)
7. [Inspect : audit visuel des shards](#7-inspect-visual-shard-audit)
8. [Holographic Key Escrow](#8-holographic-key-escrow)
9. [Visualiseur de docs intégré (étape 10)](#9-in-app-docs-viewer-stage-10)
10. [i18n : changement de langue](#10-i18n-language-switching)
11. [Persistance et redémarrage](#11-persistence-and-restart)
12. [Cluster multi-processus](#12-multi-process-cluster)
13. [TLS / mTLS sur le réseau](#13-tls--mtls-on-the-wire)
14. [Métriques, journaux, SSE](#14-metrics-logs-sse)
15. [Vérifications de non-régression étape 11](#15-stage-11-regression-checks)

---

## 1. Démarrer le cluster

**Objectif.** Démarrer un cluster intégré (40 nodes dans un seul
processus, 4 zones) et confirmer que chaque node est vivant avec un
catalogue vide.

```sh
rm -rf ./holofs-data    # fresh start
cargo run --release --bin holofs-web -- \
    --storage ./holofs-data \
    --addr 127.0.0.1:8787 \
    --log info --log-format text \
    --no-seed
```

Lignes de journal attendues :

```
INFO holofs_web: starting holofs-web version=0.4.0 addr=127.0.0.1:8787
INFO holofs_web::bootstrap: embedded cluster ready n_nodes=40 n_zones=4
INFO holofs_web::bootstrap: catalog loaded objects=0
INFO holofs_web: listening addr=127.0.0.1:8787
```

**Critères de réussite.**

- `GET http://127.0.0.1:8787/` renvoie le HTML du catalogue (grille vide).
- `GET /api/stats` renvoie `{"nodes_total":40,"nodes_live":40,"objects_total":0,…}`.
- 40 ports à l'écoute sur 9100..9139 (`lsof -nP -iTCP:9100-9139 -sTCP:LISTEN | wc -l` ≥ 40).

Sans `--no-seed`, le catalogue s'amorce avec deux images de démonstration
(`photo.png`, `mandala.png`) ; pratique pour les scénarios suivants mais
peu commode pour des tests CRUD propres.

---

## 2. CRUD d'objets basique

**Objectif.** Couvrir les quatre types pris en charge — image / audio /
texte / opaque — ainsi que le cas limite perceptuel de la déduplication
inter-formats.

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

Chaque PUT renvoie un JSON `{"name":…,"object_id":…,"shards":…,"put_ms":…}`.

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

**Critères de réussite.**

- Chaque PUT renvoie `201 Created` avec un `object_id` non nul.
- GET renvoie les octets PNG/WAV/texte d'origine, identiques bit à bit
  pour les images et les opaques (le texte tolère la perte d'un chunk
  entier, jamais d'octets à l'intérieur d'un chunk).
- `/api/stats.objects_by_kind` reflète les compteurs par type.
- `curl -X DELETE http://127.0.0.1:8787/photo.png` renvoie
  `200 {"deleted":"photo.png",…}` et `objects_total` diminue.

### Déduplication inter-formats

```sh
# same frame as PNG and BMP — data_cid is identical
convert assets/sample.png /tmp/sample.bmp
curl -X PUT --data-binary @assets/sample.png http://127.0.0.1:8787/a.png
curl -X PUT --data-binary @/tmp/sample.bmp http://127.0.0.1:8787/a.bmp
curl -s http://127.0.0.1:8787/api/stats | python3 -m json.tool | grep dedup
```

`dedup_savings_pct > 0` car les formats sans perte produisent le même
`data_cid` → les shards sur disque sont dédupliqués.

---

## 3. Catalogue hiérarchique (étape 9)

**Objectif.** Vérifier mkdir, la navigation dans les sous-répertoires,
les refus corrects en cas de collision, le renommage, rmdir.

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

**Vérification UI.** Ouvrez `http://127.0.0.1:8787/?p=photos/2026/raw` —
le fil d'Ariane doit afficher `home / photos / 2026 / raw`, la tuile
img.png est cliquable, le formulaire « + folder » fonctionne.

**Segments réservés.** `PUT /health/foo`, `PUT /api/foo`, `PUT /help/foo`,
`PUT /inspect-zoom/foo` renvoient tous `400` — ces routes ne peuvent
pas être occultées.

---

## 4. HTTP Range sur GET (étape 11.1)

**Objectif.** Confirmer que les GET partiels fonctionnent — requis pour
la lecture audio avec curseur, les téléchargements volumineux
reprenables, et la future recherche dans la vidéo.

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

**Critères de réussite.** Les codes de statut et le `Content-Range`
correspondent au tableau ci-dessus ; les octets découpés sont exacts au
bit près (le motif `0..255 × 4` renvoie `00 01 02 03` pour
`bytes=256-259`).

**Scénario média réel.**

```html
<!-- open in a browser, confirm the seek bar works -->
<audio src="http://127.0.0.1:8787/track.wav" controls></audio>
```

Le navigateur envoie un en-tête `Range` à chaque déplacement. Le journal
du gateway affiche des réponses `206 Partial Content`.

---

## 5. Dégradation holographique

**Objectif.** L'astuce phare — quand une grande partie du cluster
disparaît, le fichier se décode toujours **à plus basse résolution**.
Pilotage depuis l'UI à `/health/<name>`.

1. Démarrer avec un `photo.png` amorcé (sans `--no-seed`).
2. Ouvrir `http://127.0.0.1:8787/health/photo.png`. Vous obtenez un
   tableau de marge par `(channel, layer)`, des exécutions Monte-Carlo
   à 10/25/50/75 % de perte, et un scénario de défaillance complète
   d'une zone.
3. Ouvrir `http://127.0.0.1:8787/health`. Une grille de 40 nodes avec
   des boutons **kill** / **revive**.
4. Tuez les nodes un par un et observez `/health/photo.png` :
   - 10–20 % de perte : marge positive partout, PSNR ~99 dB.
   - 30–40 % de perte : la marge L3 (détail) → 0, PSNR tombe à ~30 dB —
     l'image devient plus floue.
   - 50–60 % de perte : L2 meurt, seuls L0+L1 subsistent — forme
     grossière uniquement.
   - 75 % de perte : toutes les couches meurent — la sortie dégénère
     en bruit.
5. Entre les étapes, lancez `GET /photo.png` et inspectez le PNG à l'œil.

**Critères de réussite.**

- Sous le seuil K de chaque couche — fichier complet.
- Au-dessus de L3 mais sous L2 — image reconnaissable, les hautes
  fréquences disparues (plus floue).
- Le tableau de marge se met à jour via SSE (`/api/health/events`) —
  les chiffres bougent après un kill sans recharger la page.

**Récupération.** Appuyez sur **revive** sur les nodes tués. Après
1-2 cycles du moniteur de santé (`HOLOFS_MONITOR_INTERVAL`, 15 s par
défaut), l'auto-réparation s'exécute et la marge revient.

### Défaillance d'une zone entière

Chaque node porte une `zone` (0..3). Tuez les **10 nodes** d'une zone :
l'objet se décode toujours jusqu'à L2 grâce au placement sensible aux
zones (`ceil(n/z)` shards par zone).

---

## 6. Recherche perceptuelle et diff

**Objectif.** Trouver des objets similaires par un hash perceptuel de
16 octets et observer la déduplication via diff.

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

**Diff par chunk.**

```
http://127.0.0.1:8787/diff?a=orig.png&b=blurry.png
```

L'UI dessine des cellules vertes (chunks identiques) et rouges
(différents). Deux copies identiques → 100 % en vert plus un
`storage_saved_kb` important.

---

## 7. Inspect : audit visuel des shards

**Objectif.** Confirmer que la grille affiche les 444 shards (3 canaux
× 4 couches × 26..64 par couche) sans lacune. Vérification de
non-régression pour l'étape 11.2.

1. Ouvrir `http://127.0.0.1:8787/inspect/mandala.png`.
2. Faire défiler — pour chaque canal (R, G, B) vous devez voir 4
   sections (couches 0..3), chacune avec le bon nombre de miniatures :
   - L0 — 64 shards (16 systematic + 48 RLNC)
   - L1 — 40 shards (16 + 24)
   - L2 — 26 shards (16 + 10)
   - L3 — 18 shards (16 + 2)
3. **Les 444 miniatures doivent toutes s'afficher** (aucun placeholder
   `<img>` cassé). Avant l'étape 11.2, environ 8 % tombaient sous
   charge concurrente.
4. Cliquez sur n'importe quelle miniature → atterrissez sur
   `/inspect-zoom/<c_l_idx>/<name>` avec le grand PNG, les coefficients
   hex, et la charge utile.

**Code couleur.** Les shards systematic (les K=16 premiers de chaque
couche) ont une bordure verte et portent une charge utile significative
(structure visible). Les RLNC — bordure orange, la charge utile
ressemble à du bruit.

**Test de stress.** Ouvrir 4 onglets de navigateur sur
`/inspect/photo.png` simultanément — chacun s'affiche entièrement. Le
journal du gateway ne doit pas contenir de lignes `status=404` pour
`/api/shard/...`.

---

## 8. Holographic Key Escrow

**Objectif.** Schéma à seuil de type Shamir — diviser un fichier
arbitraire en N parts avec un seuil K, restaurer depuis n'importe
quelles K.

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

**Critères de réussite.**

- N'importe quelles **K** parts sur N reconstruisent le fichier
  exactement (bit à bit).
- **K-1** parts n'y parviennent pas (recover renvoie 400).
- Les parts **ne sont pas stockées dans le cluster** — elles
  disparaissent au redémarrage du gateway. Téléchargez juste après le
  split ; sinon `/escrow/download/...` renvoie `410 Gone`.

**UI.** `http://127.0.0.1:8787/escrow` propose à la fois les
formulaires de split et de recover.

---

## 9. Visualiseur de docs intégré (étape 10)

**Objectif.** Vérifier le visualiseur de docs, le rendu Mermaid et
KaTeX.

1. Ouvrir `http://127.0.0.1:8787/help`. La barre latérale gauche
   contient 7 documents, le panneau de droite affiche `README.md`.
2. Cliquer sur **Architecture** : `/help/architecture` s'ouvre avec un
   diagramme **Mermaid** correctement rendu (le graphe de dépendances
   des crates) — SVG dessiné côté client via `mermaid.min.js`.
3. Cliquer sur **Theory** : nombreuses formules **KaTeX**
   (`$x^2 + y^2$`, `$$E = mc^2$$`, etc.) — toutes rendues.
4. Le bas de la barre latérale propose un sélecteur de langue
   (en, ru, de, fr, es). Cliquez sur **Русский** — le document se
   ré-affiche depuis `docs/ru/<slug>.md`. Mermaid et KaTeX continuent
   de fonctionner (les formules et diagrammes sont du code, non
   traduits).
5. Lorsqu'une variante localisée manque, le gateway sert la version
   anglaise (`docs/<slug>.md`) avec `locale: en` dans la méta-ligne.

**Critères de réussite.**

- Les 7 documents s'ouvrent dans les 5 langues sans 404.
- Les diagrammes Mermaid sont de vrais SVG, pas du code brut dans
  un `<div>`.
- Les formules KaTeX apparaissent comme des mathématiques composées,
  pas du source TeX.
- La barre latérale met en surbrillance le document actif (classe
  `.active`).

---

## 10. i18n : changement de langue

**Objectif.** L'UI fonctionne en 5 langues sur chaque route.

1. Ouvrir n'importe quelle page (`/`, `/help`, `/escrow`).
2. Le côté droit de la barre supérieure propose un sélecteur compact :
   `en · ru · de · fr · es`.
3. Parcourir :
   - `?lang=ru` → « каталог », « состояние », « эскроу », « помощь ».
   - `?lang=de` → « Katalog », « Zustand », « Treuhand », « Hilfe ».
   - `?lang=fr` → « catalogue », « santé », « séquestre », « aide ».
   - `?lang=es` → « catálogo », « estado », « depósito », « ayuda ».
4. L'URL est réécrite via `rewrite_lang` — le chemin et les autres
   paramètres de requête (`?p=…`, `?a=&b=…`) sont préservés.

**Locale inconnue.** `?lang=ja` ou autre chose retombe sur l'anglais.

---

## 11. Persistance et redémarrage

**Objectif.** Confirmer que les données survivent à un redémarrage.

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

**Critères de réussite.**

- `catalog.bin` (~Ko) et les shards (`node_*/<hex>/<hex>.shard`) sont
  intacts.
- Après redémarrage, `GET` renvoie l'original octet pour octet.
- Les identités des nodes (`node_*/identity.key`) sont stables — les
  clés publiques correspondent aux valeurs antérieures au redémarrage.

---

## 12. Cluster multi-processus

**Objectif.** Exercer le mode distribué « réel » — des nodes en tant
que processus séparés.

```sh
./scripts/spawn-cluster.sh 8 5100 127.0.0.1:8787
```

Le script :

1. Lance 8 processus `holofs-node` avec leur stockage sous
   `.cluster-data/node-N`.
2. Collecte leurs clés publiques Ed25519.
3. Génère une paire de clés d'administration et signe la whitelist.
4. Démarre le gateway avec `--whitelist`.

Dans un autre terminal :

```sh
curl -X PUT --data-binary @assets/sample.png http://127.0.0.1:8787/test.png

# shards spread across the 8 processes
for i in 0 1 2 3 4 5 6 7; do
    n=$(find .cluster-data/node-$i -name '*.shard' 2>/dev/null | wc -l)
    echo "node-$i: $n shards"
done
```

**Critères de réussite.**

- La somme des shards sur les nodes ≈ 444 (×3 canaux × compteurs par
  couche).
- Ctrl-C sur le script arrête les 8 nodes et le gateway.
- Relancer le même script (sans effacer `.cluster-data/`) restaure
  l'état précédent — les données sur disque sont intactes.

---

## 13. TLS / mTLS sur le réseau

**Objectif.** Activer TLS optionnel sur le trafic gateway ↔ node.

```sh
# embedded mode — a self-signed CA is generated automatically
HOLOFS_TLS=1 cargo run --release --bin holofs-web -- \
    --storage ./holofs-data-tls --no-seed
```

Journal : `TLS material self-signed; ca → ./holofs-data-tls/tls/ca.crt`.

Authentification mutuelle :

```sh
HOLOFS_TLS=1 HOLOFS_MTLS=1 cargo run --release --bin holofs-web -- \
    --storage ./holofs-data-mtls --no-seed
```

**Ce qu'il faut vérifier.**

- Le trafic sur 9100..9139 n'est plus du TCP en clair — `tcpdump` sur
  la loopback montre des handshakes TLS (`16 03 ...`).
- PUT/GET/inspect fonctionnent comme sans TLS.
- Sans `--tls`, les connexions restent en clair — rétro-compatible.

Les détails PKI et le flux en mode distribué avec des certificats
fournis par l'opérateur figurent dans
[docs/operations.md](./operations.md).

---

## 14. Métriques, journaux, SSE

**Prometheus.**

```sh
curl http://127.0.0.1:8787/metrics
```

Attendu :

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

**Journaux structurés.**

```sh
cargo run --release --bin holofs-web -- \
    --storage ./holofs-data --log-format json --log info
```

Chaque ligne est un JSON avec `timestamp`, `level`, `target`, `fields`.
Pratique pour journald / fluentd / Vector / Loki.

**Flux SSE de santé.**

```sh
curl -N http://127.0.0.1:8787/api/health/events
```

Environ toutes les 3 secondes, une trame `event: health\ndata: {…}\n\n`
arrive avec un instantané JSON — c'est ce qui alimente le tableau de
bord en direct `/health`.

---

## 15. Vérifications de non-régression étape 11

Trois sondes rapides ciblant des problèmes récemment corrigés.
Exécutez-les après tout changement du gateway ou du pipeline
d'ingestion.

### 11.1 Range sur les médias

```sh
curl -i -H "Range: bytes=0-99" http://127.0.0.1:8787/photo.png \
    | head -10
```

Attendu : `206 Partial Content`, `Content-Range: bytes 0-99/<total>`,
`Content-Length: 100`. Ni 200, ni 416.

### 11.2 Inspect ne perd pas de shards

Ouvrir `http://127.0.0.1:8787/inspect/mandala.png` dans un navigateur.
Les 444 miniatures doivent toutes s'afficher. Dans le journal :

```sh
grep -E "/api/shard/" /tmp/holofs-cluster.log | grep -v "status=200" | wc -l
```

Attendu : **0**. Avant l'étape 11.2, c'était ~41.

### 11.3 Uploads multipart volumineux

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

Les deux doivent renvoyer `200`/`201`, pas
`400 multipart read: Error parsing`.

---

## Pour conclure

Arrêt propre :

```sh
pkill -f 'target/release/holofs-web'
# or Ctrl-C in the terminal running the cluster
```

Effacement complet — supprimer tout l'état :

```sh
rm -rf ./holofs-data ./.cluster-data
```

En cas de dysfonctionnement, comparez avec les descriptions ci-dessus
et consultez :

- [docs/operations.md](./operations.md) — configuration et exploitation
- [docs/architecture.md](./architecture.md) — flux de données PUT → GET
- [docs/api.md](./api.md) — API HTTP, format du protocole filaire
- [docs/threat-model.md](./threat-model.md) — menaces couvertes
