
# Scénarios de test

Liste de vérification manuelle de bout en bout pour holofs. Couvre
chaque sous-système majeur — CRUD, catalogue hiérarchique, HTTP
Range, dégradation holographique, recherche perceptuelle, séquestre,
persistance et i18n. Chaque scénario liste les commandes, le résultat
attendu et les marqueurs de succès.

> Cible 1.0.0. Les drapeaux CLI, variables d'environnement
> et chemins reflètent le code au moment de la rédaction ; si quelque
> chose dérive, consulter [docs/fr/operations.md](./operations.md) ou
> `cargo run -p holofs-web -- --help`.

## Sommaire

1. [Amorcer le cluster](#1-amorcer-le-cluster)
2. [CRUD basique d'objets](#2-crud-basique-dobjets)
3. [Catalogue hiérarchique](#3-catalogue-hiérarchique)
4. [HTTP Range sur GET (1)](#4-http-range-sur-get)
5. [Dégradation holographique](#5-dégradation-holographique)
6. [Recherche perceptuelle et diff](#6-recherche-perceptuelle-et-diff)
7. [Inspect : audit visuel des shards](#7-inspect--audit-visuel-des-shards)
8. [Séquestre holographique de clé](#8-séquestre-holographique-de-clé)
9. [Visionneuse de docs intégrée](#9-visionneuse-de-docs-intégrée)
10. [i18n : changement de langue](#10-i18n--changement-de-langue)
11. [Persistance et redémarrage](#11-persistance-et-redémarrage)
12. [Cluster multi-processus](#12-cluster-multi-processus)
13. [TLS / mTLS sur le fil](#13-tls--mtls-sur-le-fil)
14. [Métriques, logs, SSE](#14-métriques-logs-sse)
15. [Contrôles de régression](#15-contrôles-de-régression)

---

## 1. Amorcer le cluster

**Objectif.** Démarrer un cluster embarqué (40 nœuds dans un
processus, 4 zones) et confirmer que chaque nœud est vivant avec un
catalogue vide.

```sh
rm -rf ./holofs-data    # démarrage propre
cargo run --release --bin holofs-web -- \
    --storage ./holofs-data \
    --addr 127.0.0.1:8787 \
    --log info --log-format text \
    --no-seed
```

Lignes de log attendues :

```
INFO holofs_web: starting holofs-web version=1.0.0 addr=127.0.0.1:8787
INFO holofs_web::bootstrap: embedded cluster ready n_nodes=40 n_zones=4
INFO holofs_web::bootstrap: catalog loaded objects=0
INFO holofs_web: listening addr=127.0.0.1:8787
```

**Marqueurs de succès.**

- `GET http://127.0.0.1:8787/` retourne le HTML du catalogue (grille vide).
- `GET /api/stats` retourne `{"nodes_total":40,"nodes_live":40,"objects_total":0,…}`.
- 40 ports en écoute sur 9100..9139 (`lsof -nP -iTCP:9100-9139 -sTCP:LISTEN | wc -l` ≥ 40).

Sans `--no-seed`, le catalogue s'ensemence lui-même avec deux images
de démo (`photo.png`, `mandala.png`) ; pratique pour les scénarios
suivants mais inconvenant pour des tests CRUD propres.

---

## 2. CRUD basique d'objets

**Objectif.** Couvrir les quatre types supportés — image / audio /
text / opaque — plus le cas limite perceptuel de la dedup
inter-format.

```sh
# image (PNG → type image, DWT + RLNC sur 4 couches)
curl -X PUT --data-binary @assets/sample.png http://127.0.0.1:8787/photo.png

# text (UTF-8 → type text, chunké + récupération partielle)
printf "Hello, holofs!\nLine 2\n" | curl -X PUT --data-binary @- \
    http://127.0.0.1:8787/note.txt

# opaque (binaire arbitraire → 1 couche RLNC, pas de DWT)
dd if=/dev/urandom of=/tmp/blob.bin bs=1024 count=100 2>/dev/null
curl -X PUT --data-binary @/tmp/blob.bin http://127.0.0.1:8787/blob.bin

# audio (WAV → type audio, DWT 1D par canal)
# (à sauter si vous n'avez pas de fichier wav sous la main)
curl -X PUT --data-binary @track.wav http://127.0.0.1:8787/track.wav
```

Chaque PUT retourne le JSON `{"name":…,"object_id":…,"shards":…,"put_ms":…}`.

```sh
# GET — récupération octet-parfaite (décodage pleine qualité)
curl -s -o /tmp/got.png http://127.0.0.1:8787/photo.png
cmp assets/sample.png /tmp/got.png && echo "image roundtrip OK"

# Preview = L0 uniquement
curl -s -o /tmp/prev.png http://127.0.0.1:8787/preview/photo.png
file /tmp/prev.png   # devrait être une image PNG

# Stats du catalogue
curl -s http://127.0.0.1:8787/api/stats | python3 -m json.tool
```

**Marqueurs de succès.**

- Chaque PUT retourne `201 Created` avec un `object_id` non nul.
- GET retourne les octets originaux PNG/WAV/text, octet-parfait pour
  image et opaque (text autorise la perte de chunks entiers, jamais
  d'octets à l'intérieur d'un chunk).
- `/api/stats.objects_by_kind` reflète les comptes par type.
- `curl -X DELETE http://127.0.0.1:8787/photo.png` retourne
  `200 {"deleted":"photo.png",…}` et `objects_total` chute.

### Dedup inter-format

```sh
# même trame en PNG et BMP — data_cid est identique
convert assets/sample.png /tmp/sample.bmp
curl -X PUT --data-binary @assets/sample.png http://127.0.0.1:8787/a.png
curl -X PUT --data-binary @/tmp/sample.bmp http://127.0.0.1:8787/a.bmp
curl -s http://127.0.0.1:8787/api/stats | python3 -m json.tool | grep dedup
```

`dedup_savings_pct > 0` parce que les formats sans perte produisent le
même `data_cid` → les shards sur disque sont dédupliqués.

---

## 3. Catalogue hiérarchique

**Objectif.** Vérifier mkdir, la navigation dans les sous-répertoires,
les refus corrects sur collision, renommage, rmdir.

```sh
# construire une arborescence
curl -X POST -d "parent=&name=photos"          http://127.0.0.1:8787/api/mkdir
curl -X POST -d "parent=photos&name=2026"      http://127.0.0.1:8787/api/mkdir
curl -X POST -d "parent=photos/2026&name=raw"  http://127.0.0.1:8787/api/mkdir

# téléverser en profondeur
curl -X PUT --data-binary @assets/sample.png \
    http://127.0.0.1:8787/photos/2026/raw/img.png

# récupérer
curl -o /tmp/got.png http://127.0.0.1:8787/photos/2026/raw/img.png
cmp assets/sample.png /tmp/got.png && echo "deep path roundtrip OK"

# refus
curl -i -X POST -d "parent=does/not/exist&name=x" \
    http://127.0.0.1:8787/api/mkdir         # 400 parent manquant
curl -i -X DELETE http://127.0.0.1:8787/api/rmdir/photos
                                            # 409 répertoire non vide
curl -i -X DELETE http://127.0.0.1:8787/photos
                                            # 409 est un répertoire

# renommage (emporte tous les descendants)
curl -X POST -d "from=photos&to=archive" http://127.0.0.1:8787/api/mv
curl -s http://127.0.0.1:8787/archive/2026/raw/img.png -o /tmp/r.png
cmp assets/sample.png /tmp/r.png && echo "rename moved descendants"

# rmdir de bas en haut
curl -X DELETE http://127.0.0.1:8787/archive/2026/raw/img.png
curl -X DELETE http://127.0.0.1:8787/api/rmdir/archive/2026/raw
curl -X DELETE http://127.0.0.1:8787/api/rmdir/archive/2026
curl -X DELETE http://127.0.0.1:8787/api/rmdir/archive
```

**Contrôle UI.** Ouvrir `http://127.0.0.1:8787/?p=photos/2026/raw` —
le fil d'Ariane devrait lire `home / photos / 2026 / raw`, la tuile
img.png est cliquable, le formulaire « + folder » fonctionne.

**Segments réservés.** `PUT /health/foo`, `PUT /api/foo`,
`PUT /help/foo`, `PUT /inspect-zoom/foo` retournent tous `400` — ces
routes ne peuvent pas être masquées.

---

## 4. HTTP Range sur GET

**Objectif.** Confirmer que les GET partiels fonctionnent — requis
pour le scrubbing audio, les téléchargements gros et reprenables, le
futur seek vidéo.

```sh
# blob de 1000 octets
printf 'A%.0s' $(seq 1 1000) > /tmp/blob.bin
curl -X PUT --data-binary @/tmp/blob.bin http://127.0.0.1:8787/range-test

# GET complet — 200, Accept-Ranges: bytes
curl -I http://127.0.0.1:8787/range-test 2>&1 | grep -i accept-ranges

# 10 premiers octets
curl -i -H "Range: bytes=0-9" http://127.0.0.1:8787/range-test
# attendre 206 Partial Content, content-range: bytes 0-9/1000

# 50 derniers octets
curl -i -H "Range: bytes=-50" http://127.0.0.1:8787/range-test
# content-range: bytes 950-999/1000

# intervalle ouvert
curl -i -H "Range: bytes=900-" http://127.0.0.1:8787/range-test
# content-range: bytes 900-999/1000

# au-delà de EOF — 416
curl -i -H "Range: bytes=5000-" http://127.0.0.1:8787/range-test
# 416 Range Not Satisfiable, content-range: bytes */1000

# multi-plages non supportées — dégrade en 200 (corps complet)
curl -i -H "Range: bytes=0-9,20-29" http://127.0.0.1:8787/range-test
# 200 OK, 1000 bytes
```

**Marqueurs de succès.** Les codes de statut et `Content-Range`
correspondent au tableau ci-dessus ; les octets tranchés sont exacts
(le motif `0..255 × 4` retourne `00 01 02 03` pour `bytes=256-259`).

**Scénario média réel.**

```html
<!-- ouvrir dans un navigateur, confirmer que la barre de recherche fonctionne -->
<audio src="http://127.0.0.1:8787/track.wav" controls></audio>
```

Le navigateur envoie `Range` à chaque seek. Le log de la passerelle
montre des réponses `206 Partial Content`.

---

## 5. Dégradation holographique

**Objectif.** Le tour phare — quand une grande fraction du cluster
meurt, le fichier se décode encore **à résolution plus faible**.
Piloter depuis l'UI à `/health/<name>`.

1. Démarrer avec `photo.png` ensemencé (retirer `--no-seed`).
2. Ouvrir `http://127.0.0.1:8787/health/photo.png`. On obtient un
   tableau de marge par `(canal, couche)`, des runs Monte-Carlo à
   10/25/50/75% de perte, et un scénario de défaillance de zone entière.
3. Ouvrir `http://127.0.0.1:8787/health`. Une grille de 40 nœuds avec
   des boutons **kill** / **revive**.
4. Tuer les nœuds un par un et observer `/health/photo.png` :
   - 10–20 % de perte : marge positive partout, PSNR ~99 dB.
   - 30–40 % de perte : la marge L3 (détail) → 0, PSNR chute à
     ~30 dB — l'image devient plus floue.
   - 50–60 % de perte : L2 meurt, seuls L0+L1 restent — forme
     grossière seulement.
   - 75 % de perte : chaque couche meurt — la sortie s'effondre en
     bruit.
5. Entre les étapes, récupérer `GET /photo.png` et observer le PNG.

**Marqueurs de succès.**

- Sous le seuil K de chaque couche — fichier complet.
- Au-dessus de L3 mais sous L2 — image reconnaissable avec les hautes
  fréquences disparues (plus floue).
- Le tableau de marge se met à jour par SSE (`/api/health/events`) —
  les nombres changent après un kill sans rechargement de page.

**Récupération.** Cliquer **revive** sur les nœuds tués. Après 1-2
cycles du monitor de santé (`HOLOFS_MONITOR_INTERVAL`, 15 s par
défaut), la réparation automatique s'exécute et la marge revient.

### Défaillance de zone entière

Chaque nœud porte une `zone` (0..3). Tuer **les 10 nœuds** d'une
zone : l'objet se décode encore jusqu'à L2 grâce au placement
conscient des zones (`ceil(n/z)` shards par zone).

---

## 6. Recherche perceptuelle et diff

**Objectif.** Trouver des objets similaires par un hachage perceptuel
de 16 octets + observer la dedup à travers diff.

```sh
# téléverser deux versions similaires de la même image
curl -X PUT --data-binary @assets/sample.png http://127.0.0.1:8787/orig.png
convert assets/sample.png -gaussian-blur 0x2 /tmp/blurry.png
curl -X PUT --data-binary @/tmp/blurry.png http://127.0.0.1:8787/blurry.png

# top-10 voisins
curl -s 'http://127.0.0.1:8787/api/fingerprint/orig.png' | python3 -m json.tool
# →  {"name":"orig.png","fingerprint":"a3f0…","kind":"image"}

# UI : ouvrir /similar/orig.png — voisins triés par distance L1.
```

**Diff par chunk.**

```
http://127.0.0.1:8787/diff?a=orig.png&b=blurry.png
```

L'UI dessine des cellules vertes (chunks correspondants) et rouges
(différents). Deux copies identiques → 100 % vert plus un grand
`storage_saved_kb`.

---

## 7. Inspect : audit visuel des shards

**Objectif.** Confirmer que la grille montre tous les 444 shards
(3 canaux × 4 couches × 26..64 par couche) sans lacunes. Contrôle de
régression pour
1. Ouvrir `http://127.0.0.1:8787/inspect/mandala.png`.
2. Faire défiler — pour chaque canal (R, G, B) vous devriez voir
   4 sections (couches 0..3), chacune avec le bon nombre de
   vignettes :
   - L0 — 64 shards (16 systématiques + 48 RLNC)
   - L1 — 40 shards (16 + 24)
   - L2 — 26 shards (16 + 10)
   - L3 — 18 shards (16 + 2)
3. **Toutes les 444 vignettes doivent se rendre** (pas de placeholders
   `<img>` cassés). Avant 2, ~8 % chutaient sous charge concurrente.
4. Cliquer sur une vignette → atterrir sur
   `/inspect-zoom/<c_l_idx>/<name>` avec le grand PNG, les coeffs hex,
   le payload.

**Codage couleur.** Les shards systématiques (les K=16 premiers de
chaque couche) ont une bordure verte et portent un payload
significatif (structure visible). RLNC — bordure orange, le payload
ressemble à du bruit.

**Test de stress.** Ouvrir 4 onglets de navigateur de
`/inspect/photo.png` simultanément — chacun se rend en entier. Le log
de la passerelle ne doit pas contenir de lignes `status=404` pour
`/api/shard/...`.

---

## 8. Séquestre holographique de clé

**Objectif.** Schéma à seuil de style Shamir — découper un fichier
arbitraire en N parts avec seuil K, récupérer depuis n'importe quelles K.

```sh
echo "my secret seed phrase" > /tmp/secret.txt

# découpage 3-of-5
curl -F "file=@/tmp/secret.txt" -F "k=3" -F "n=5" \
    -o /tmp/split.html http://127.0.0.1:8787/escrow/split

# le HTML porte des liens /escrow/download/<eid>_<idx>.holoshare
grep -oE '/escrow/download/[a-f0-9]+_[0-9]+\.holoshare' /tmp/split.html \
    | head -5

# télécharger n'importe quelles 3 parts
for idx in 0 2 4; do
    eid=$(grep -oE '[a-f0-9]{32}' /tmp/split.html | head -1)
    curl -o /tmp/share_${idx}.holoshare \
        "http://127.0.0.1:8787/escrow/download/${eid}_${idx}.holoshare"
done

# récupérer depuis 3 parts
curl -F "shares=@/tmp/share_0.holoshare" \
     -F "shares=@/tmp/share_2.holoshare" \
     -F "shares=@/tmp/share_4.holoshare" \
     -o /tmp/recovered.txt http://127.0.0.1:8787/escrow/recover
cmp /tmp/secret.txt /tmp/recovered.txt && echo "escrow roundtrip OK"
```

**Marqueurs de succès.**

- N'importe quelles **K** parmi N reconstruisent le fichier
  exactement (octet-parfait).
- **K-1** parts ne le font pas (recover retourne 400).
- Les parts ne sont **pas stockées dans le cluster** — elles
  disparaissent au redémarrage de la passerelle. Télécharger juste
  après le split ; sinon `/escrow/download/...` retourne `410 Gone`.

**UI.** `http://127.0.0.1:8787/escrow` porte les deux formulaires
split et recover.

---

## 9. Visionneuse de docs intégrée

**Objectif.** Vérifier la visionneuse de docs, le rendu Mermaid et
KaTeX.

1. Ouvrir `http://127.0.0.1:8787/help`. La barre latérale gauche a
   7 documents, le volet droit affiche `README.md`.
2. Cliquer **Architecture** : `/help/architecture` s'ouvre avec un
   diagramme **Mermaid** correctement rendu (le graphe de dépendances
   des crates) — SVG dessiné côté client via `mermaid.min.js`.
3. Cliquer **Théorie** : beaucoup de formules **KaTeX** (`$x^2 + y^2$`,
   `$$E = mc^2$$`, etc.) — chacune rendue.
4. Le bas de la barre latérale porte un sélecteur de langue
   (en, ru, de, fr, es). Cliquer **Русский** — le doc se re-rend
   depuis `docs/ru/<slug>.md`. Mermaid et KaTeX continuent de
   fonctionner (les formules et diagrammes sont du code, non traduits).
5. Là où une variante localisée manque, la passerelle sert l'anglaise
   (`docs/<slug>.md`) avec `locale: en` dans la ligne meta.

**Marqueurs de succès.**

- Les 7 documents s'ouvrent dans les 5 langues sans 404.
- Les diagrammes Mermaid sont de vrais SVGs, pas du code brut dans un
  `<div>`.
- Les formules KaTeX apparaissent comme des mathématiques
  typographiées, pas comme du source TeX.
- La barre latérale met en évidence le document actif (classe
  `.active`).

---

## 10. i18n : changement de langue

**Objectif.** L'UI fonctionne en 5 langues sur chaque route.

1. Ouvrir n'importe quelle page (`/`, `/help`, `/escrow`).
2. Le côté droit de la barre supérieure porte un sélecteur compact :
   `en · ru · de · fr · es`.
3. Cycler à travers :
   - `?lang=ru` → « каталог », « состояние », « эскроу », « помощь ».
   - `?lang=de` → « Katalog », « Zustand », « Treuhand », « Hilfe ».
   - `?lang=fr` → « catalogue », « santé », « séquestre », « aide ».
   - `?lang=es` → « catálogo », « estado », « depósito », « ayuda ».
4. L'URL est réécrite via `rewrite_lang` — le chemin et les autres
   paramètres de requête (`?p=…`, `?a=&b=…`) sont préservés.

**Locale inconnue.** `?lang=ja` ou toute autre chose retombe sur
l'anglais.

---

## 11. Persistance et redémarrage

**Objectif.** Confirmer que les données survivent à un redémarrage.

```sh
# 1. ensemencer le cluster
cargo run --release --bin holofs-web -- --storage ./holofs-data --no-seed &
SERVER_PID=$!
sleep 5

curl -X POST -d "parent=&name=keep" http://127.0.0.1:8787/api/mkdir
curl -X PUT --data-binary @assets/sample.png \
    http://127.0.0.1:8787/keep/img.png

# 2. arrêter
kill $SERVER_PID; wait $SERVER_PID 2>/dev/null

# 3. vérifier que l'état sur disque est en place
ls -la ./holofs-data/catalog.bin
ls ./holofs-data/node_00 | head -5    # devrait contenir des fichiers .shard

# 4. redémarrer
cargo run --release --bin holofs-web -- --storage ./holofs-data --no-seed &
sleep 5

# 5. l'objet et le catalogue sont revenus
curl -s 'http://127.0.0.1:8787/api/list_dir' \
    -X POST -d '' -H 'Content-Type: application/json' \
    --data-raw '{"prefix":""}'

curl -o /tmp/after-restart.png http://127.0.0.1:8787/keep/img.png
cmp assets/sample.png /tmp/after-restart.png && echo "persistence OK"
```

**Marqueurs de succès.**

- `catalog.bin` (~Ko) et shards (`node_*/<hex>/<hex>.shard`) sont
  intacts.
- Après redémarrage, `GET` retourne l'original octet par octet.
- Les identités de nœud (`node_*/identity.key`) sont stables — les
  clés publiques correspondent aux valeurs pré-redémarrage.

---

## 12. Cluster multi-processus

**Objectif.** Exercer le « vrai » mode distribué — les nœuds comme
processus séparés.

```sh
./scripts/spawn-cluster.sh 8 5100 127.0.0.1:8787
```

Le script :

1. Spawn 8 processus `holofs-node` avec stockage sous
   `.cluster-data/node-N`.
2. Collecte leurs clés publiques Ed25519.
3. Génère une paire de clés admin et signe la liste blanche.
4. Démarre la passerelle avec `--whitelist`.

Dans un autre terminal :

```sh
curl -X PUT --data-binary @assets/sample.png http://127.0.0.1:8787/test.png

# les shards se répartissent sur les 8 processus
for i in 0 1 2 3 4 5 6 7; do
    n=$(find .cluster-data/node-$i -name '*.shard' 2>/dev/null | wc -l)
    echo "node-$i: $n shards"
done
```

**Marqueurs de succès.**

- La somme des shards à travers les nœuds ≈ 444 (×3 canaux × comptes
  par couche).
- Ctrl-C sur le script arrête les 8 nœuds et la passerelle.
- Réexécuter le même script (sans effacer `.cluster-data/`) restaure
  l'état précédent — les données sur disque sont intactes.

---

## 13. TLS / mTLS sur le fil

**Objectif.** Activer TLS opt-in sur le trafic passerelle ↔ nœud.

```sh
# mode embarqué — une CA auto-signée est générée automatiquement
HOLOFS_TLS=1 cargo run --release --bin holofs-web -- \
    --storage ./holofs-data-tls --no-seed
```

Log : `TLS material self-signed; ca → ./holofs-data-tls/tls/ca.crt`.

Auth mutuelle :

```sh
HOLOFS_TLS=1 HOLOFS_MTLS=1 cargo run --release --bin holofs-web -- \
    --storage ./holofs-data-mtls --no-seed
```

**Quoi vérifier.**

- Le trafic sur 9100..9139 n'est plus TCP en clair — `tcpdump` sur la
  loopback montre des poignées de main TLS (`16 03 ...`).
- PUT/GET/inspect fonctionnent tout comme sans TLS.
- Sans `--tls`, les connexions restent en clair — compatible
  descendant.

Les détails PKI et le flux en mode distribué avec des certificats
fournis par l'opérateur vivent dans
[docs/fr/operations.md](./operations.md).

---

## 14. Métriques, logs, SSE

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

**Logs structurés.**

```sh
cargo run --release --bin holofs-web -- \
    --storage ./holofs-data --log-format json --log info
```

Chaque ligne est du JSON avec `timestamp`, `level`, `target`, `fields`.
Pratique pour journald / fluentd / Vector / Loki.

**Flux SSE de santé.**

```sh
curl -N http://127.0.0.1:8787/api/health/events
```

Environ toutes les 3 secondes, une trame `event: health\ndata: {…}\n\n`
arrive avec un instantané JSON — c'est ce qui pilote le tableau de bord
`/health` en direct.

---

## 15. Contrôles de régression

Trois sondes rapides ciblant des problèmes récemment corrigés. Les
exécuter après tout changement à la passerelle ou au pipeline
d'ingestion.

### 11.1 Range sur média

```sh
curl -i -H "Range: bytes=0-99" http://127.0.0.1:8787/photo.png \
    | head -10
```

Attendre `206 Partial Content`, `Content-Range: bytes 0-99/<total>`,
`Content-Length: 100`. Pas 200, pas 416.

### 11.2 Inspect ne perd pas de shards

Ouvrir `http://127.0.0.1:8787/inspect/mandala.png` dans un navigateur.
Toutes les 444 vignettes doivent se rendre. Dans le log :

```sh
grep -E "/api/shard/" /tmp/holofs-cluster.log | grep -v "status=200" | wc -l
```

Attendu **0**. Avant 2, c'était ~41.

### 11.3 Uploads multipart importants

```sh
# fichier de 3 Mo via séquestre
dd if=/dev/urandom of=/tmp/3mb.bin bs=1024 count=3000 2>/dev/null
curl -i -F "file=@/tmp/3mb.bin" -F "k=3" -F "n=5" \
    http://127.0.0.1:8787/escrow/split 2>&1 | head -1

# 30 Mo via PUT
dd if=/dev/urandom of=/tmp/30mb.bin bs=1024 count=30000 2>/dev/null
curl -i -X PUT --data-binary @/tmp/30mb.bin \
    http://127.0.0.1:8787/big.bin 2>&1 | head -1
```

Les deux doivent retourner `200`/`201`, non
`400 multipart read: Error parsing`.

---

## 16. – 12 contrôles de régression

### 16.1 Scope de similaires

Trois pilules de portée en haut de `/similar/<name>` :
**tous les fichiers** / **dossier courant** / **dossier courant
(récursif)**.

```sh
# sans restriction (défaut hérité — top-10 à travers le catalogue)
curl -s 'http://127.0.0.1:8787/similar/check.txt' \
  | grep -oE 'top similar \(<!>[0-9]+'

# uniquement les fichiers dans le même répertoire parent
curl -s 'http://127.0.0.1:8787/similar/check.txt?scope=folder' \
  | grep -oE 'top similar \(<!>[0-9]+'

# sous-arbre du parent (racine → tout le catalogue, équivalent à `all`)
curl -s 'http://127.0.0.1:8787/similar/check.txt?scope=tree' \
  | grep -oE 'top similar \(<!>[0-9]+'
```

La portée est persistante — cliquer sur un voisin navigue vers sa
propre URL `/similar` avec le même `?scope=` (et `?lang=`) préservés.

### 16.2 Filtre de catalogue + suppression de fichier

Filtre côté serveur sur `/` et `/?p=<prefix>` via trois paramètres de
requête : `q` (glob de nom, `*` = joker, match sur basename,
insensible à la casse), `from`, `to` (`YYYY-MM-DD`, plage sur
`created_at_unix`).

```sh
# tous les fichiers PNG
curl -s 'http://127.0.0.1:8787/?q=*.png' \
  | grep -oE 'class="tree-leaf' | wc -l

# combiné : fichiers texte ajoutés en 2026
curl -s 'http://127.0.0.1:8787/?q=*.txt&from=2026-01-01' \
  | grep -oE 'class="tree-leaf' | wc -l
```

La vue d'arbre garde les répertoires ancêtres de toute feuille
retenue afin que les chemins restent navigables. Les entrées héritées
avec `created_at_unix=0` (HOLOFSM6/HOLOFSM7) passent toujours tout
filtre de date.

La suppression de fichier est un miroir POST-formulaire du
`rmdir_form` existant :

```sh
curl -s -X PUT 'http://127.0.0.1:8787/tmp-delete-me.txt' \
  -H 'Content-Type: text/plain' --data 'will be deleted'
curl -s -o /dev/null -w '%{http_code}\n' \
  -X POST 'http://127.0.0.1:8787/api/rm' \
  -H 'Content-Type: application/x-www-form-urlencoded' \
  --data 'path=tmp-delete-me.txt&return_to=/'
# attendre 303 (redirection vers return_to en cas de succès)
```

Les lignes de feuille de l'arbre rendent un petit bouton `✕` avec une
demande de confirmation.

### 16.3 Sélecteur de date localisé

L'`<input type="date">` natif de la barre de filtre porte un attribut
`lang` correspondant à la locale de la page ; dans les navigateurs
Chromium, un overlay flatpickr (chargé depuis jsdelivr) remplace le
sélecteur natif afin que le calendrier parle toujours la langue de la
page, non la locale de l'OS.

Visiter `/?lang=ru`, cliquer sur un champ de date — l'en-tête du
calendrier est en russe. Passer à `/?lang=fr`, répéter — en français.
La `value=…` fait un aller-retour en `YYYY-MM-DD` indépendamment de
la locale.

### 16.4 Smoke test du serveur MCP

Démarrer le cluster avec un jeton afin que les outils d'écriture
soient activés :

```sh
HOLOFS_MCP_TOKEN=devtoken ./holofs-web \
  --storage ./holofs-data --addr 127.0.0.1:8787 \
  --log warn --log-format text
```

Initialiser une session MCP et lister chaque outil :

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

Attendre 12 noms : `diff_objects`, `find_similar`,
`get_cluster_health`, `get_object_health`, `inspect_object`,
`inspect_shard`, `list_catalog`, `mkdir`, `mv_object`,
`put_object_text`, `read_object_text`, `rmdir`.

Contrôle d'auth :

```sh
# sans en-tête → 401
curl -s -o /dev/null -w 'no-auth: %{http_code}\n' \
  -X POST http://127.0.0.1:8787/mcp -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{
    "protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"c","version":"1"}}}'

# bearer valide → 200
curl -s -o /dev/null -w 'with-auth: %{http_code}\n' \
  -X POST http://127.0.0.1:8787/mcp \
  -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{
    "protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"c","version":"1"}}}'
```

Surface de ressources :

```sh
curl -s -X POST http://127.0.0.1:8787/mcp \
  -H "Authorization: Bearer $TOKEN" -H "Mcp-Session-Id: $SID" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":3,"method":"resources/list"}' \
  | grep -oE '"uri":"holofs:///[^"]+"' | head -5
```

Pour le câblage dans Claude Code, voir
[api.md §5](./api.md#5-serveur-mcp).

---

## 17. Opérations en ondelettes

Les deux opérations fonctionnent sur l'endpoint MCP existant (`/mcp`)
— garder la même session qu'au §16.4. Définir `HOLOFS_MCP_TOKEN`
avant de démarrer le cluster afin que le formulaire `save_as`
fonctionne.

### 17.1 Mélange en ondelettes

Construire un PNG hybride à partir de deux images compatibles, le
sauver au catalogue comme `hybrid.png` :

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

# Le télécharger pour inspecter l'hybride — devrait être un PNG régulier.
curl -s -o /tmp/hybrid.png 'http://127.0.0.1:8787/hybrid.png'
file /tmp/hybrid.png
```

`file /tmp/hybrid.png` devrait rapporter une vraie image PNG avec les
dimensions attendues.

Erreurs de compatibilité — les formes / k / paramètres par couche
incompatibles retournent `BadRequest` :

```sh
# Mélanger image avec text → BadRequest par le contrôle du type.
curl -s -X POST http://127.0.0.1:8787/mcp \
  -H "Authorization: Bearer $TOKEN" -H "Mcp-Session-Id: $SID" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{
    "name":"wavelet_mix","arguments":{
      "a":"photo.png","b":"check.txt","split":0}}}' \
  | grep -oE '"message":"[^"]*"' | head -1
```

### 17.2 Filtre de couche audio

Restituer un objet audio avec seulement les basses (L0) préservées,
sauver comme nouvelle entrée de catalogue :

```sh
# Suppose un `track.wav` ingéré précédemment.
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

`keep_layers:[]` ou chaque couche écartée → `BadRequest` (la sortie
serait du silence).

### 17.3 Le mode inline « pas de copie »

Omettre `save_as` pour obtenir les octets inline comme un blob
base64 — utile quand vous voulez que le LLM regarde le résultat sans
laisser d'artefact de catalogue derrière :

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

`bytes_len` rapporte la taille du PNG ; `saved_as` devrait être absent.

---

## 18. Démarrage rapide avec l'arborescence d'échantillons

`tools/test-data/` livre quatre éléments qui font passer un checkout
frais directement à « chaque fonctionnalité exercée, chaque page
peuplée » sans devoir fabriquer à la main des fichiers d'entrée :

```
tools/test-data/
├── generate-samples.py    # déterministe, Python 3.10+ sans dépendances
├── clean-cluster.sh       # efface catalogue + shards + embeddings + versions
├── upload-samples.sh      # PUT l'arborescence, préservant la hiérarchie
└── run-tests.sh           # smoke test de bout en bout sur les Stages 12.6–15.0
```

### 18.1 Générer l'arborescence

```sh
python3 tools/test-data/generate-samples.py
# → wrote 38 samples (1,862,535 bytes) under <repo>/samples
```

La sortie vit sous `./samples/` (gitignored). Tous les octets sont
déterministes — réexécuter avec les mêmes args produit des fichiers
octet-identiques, afin que les tests versionnés puissent s'ancrer
contre les hachages exacts.

Hiérarchie :

```
samples/
  photos/{landscapes,abstract,brand-pairs}/*.png
  audio/{music,effects,silence}/*.wav
  docs/{notes,spec,legal}/{*.txt,*.md,*.json}
  binaries/{archives,blobs}/{*.zip,*.tar,*.bin}
```

Le dossier brand-pairs inclut des quasi-doublons intentionnels
(`logo-N.png` + `logo-N-wm.png`) afin que la colonne robust-copy de
`/similar` produise des hits.

### 18.2 Redémarrage propre

```sh
tools/test-data/clean-cluster.sh
# (FORCE=1 pour sauter la demande de confirmation)

./target/release/holofs-web \
    --storage ./holofs-data \
    --addr 127.0.0.1:8787 \
    --enable-embed \
    --enable-versions &
```

Sans `--enable-embed`, la page `/search` rend une bannière « embed
disabled ». Sans `--enable-versions`, les PUT qui remplacent un objet
existant purgent les shards précédents (pas d'archive).

### 18.3 Envoyer l'arborescence

```sh
tools/test-data/upload-samples.sh
```

Le script mkdirs chaque dossier de préfixe d'abord (afin que
`/?p=<dir>` fonctionne immédiatement), puis PUTs chaque fichier.
Finalement, il interroge `/api/stats` et imprime les nouveaux totaux
du catalogue — attendre `objects_total = 38 + <marqueurs_répertoires>`
(les mkdirs du script d'upload sont aussi comptés comme entrées de
répertoire).

### 18.4 Smoke test de bout en bout

```sh
tools/test-data/run-tests.sh
```

Ce qu'il parcourt, par stage :

| Stage  | Contrôle                                             |
|--------|------------------------------------------------------|
| 9      | `/` + `/?p=<folder>` pour chaque sous-répertoire    |
| 12.6   | `/mix?a=<image>` rend le compositeur de mélange en ondelettes |
| 12.7   | `/health/<name>` métriques par fichier              |
| 12.7   | `/about` page marketing                             |
| 12.8/9 | UI `/search` + `/api/search?band=<any|coarse|mid|full>` |
| 13.0   | `/similar/<brand-pair logo>` inclut la colonne robust-copy |
| 13.1   | `/holo/<name>` + `/preview/stream/<name>` multipart |
| 13.2   | `/api/spotlight.png?mode=spatial`                   |
| 14.1   | `/api/spotlight.png?mode=coeff`                     |
| 13.4   | PUT deux fois → `/versions/<name>` montre la ligne archivée |
| 14.0/3 | `POST /api/gc` retourne un JSON `GcReport`          |

Chaque contrôle imprime `✓` / `✗` et le code de sortie du script est
non nul si un contrôle échoue.

---

## 19. Page de métriques par fichier

**Objectif** : confirmer que le bloc « Unique metrics » sous
`/health/<name>` se remplit correctement.

**Étapes** :

1. Choisir n'importe quelle image de l'arborescence d'échantillons,
   p. ex. `photos/landscapes/mountain.png`.
2. Visiter
   `http://127.0.0.1:8787/health/photos/landscapes/mountain.png` dans
   un navigateur, ou curl directement l'API sous-jacente :

   ```sh
   # POST — l'endpoint est une fonction serveur leptos, donc l'argument name
   # voyage dans le corps du formulaire, non dans la chaîne de requête. Un GET retourne
   # 405 Method Not Allowed.
   curl -s -X POST -d 'name=photos/landscapes/mountain.png' \
        http://127.0.0.1:8787/api/file_metrics \
        | python3 -m json.tool
   ```

**Payload attendu** : un `FileMetricsView` avec :

- `total_shards_in_file` ≈ `unique_shards_in_file` (la dedup au moment
  du PUT ne comprime pas au sein de l'encodage RLNC d'un fichier).
- `catalog_total_shards` ≥ `total_shards_in_file`.
- `originality_pct` quelque part dans `[0, 100]` ; une image de
  l'arborescence d'échantillons sans structure partagée devrait être
  proche de 100.
- `originality_per_layer` est un `Vec<f32>` avec `nlayers` entrées.
- `layer_energy` renseigné pour image / audio ; `None` pour text /
  opaque.
- `audio_bands` uniquement présent quand `kind == "audio"`.
- `neighbours` est vide sauf si le catalogue contient aussi les mêmes
  octets sous un autre nom.

**Contrôle brand-pair** : contre
`photos/brand-pairs/logo-1.png`, le tableau `neighbours[]` devrait
lister `photos/brand-pairs/logo-1-wm.png` comme l'entrée **top**
(le plus haut `shared_total`) avec un `shared_per_layer[0]` non nul —
c.-à-d. les shards systématiques de couche-0 (LL / grossière)
survivent octet par octet malgré le filigrane de coin. D'autres images
du catalogue montrent `shared_per_layer[0] == 0`. Ce recouvrement de
couche-0 est ce qui nourrit le score robust-copy de 0. Voir §21 pour
la réserve sur la formule de score sur les données de test
synthétiques.

---

## 20. Recherche sémantique CLIP + bandes (Stages 12.8 / 12.9 / 13.3)

**Prérequis** : serveur démarré avec `--enable-embed`. Au premier
appel, la passerelle télécharge ~155 Mio de poids CLIP depuis
HuggingFace dans `~/.cache/huggingface/hub` ; les redémarrages
suivants sont instantanés.

**Index en bloc** (nécessaire une seule fois après un redémarrage
propre) :

```sh
curl -s -X POST http://127.0.0.1:8787/api/embed_all
# → {"new":<N>,"skipped":<M>}
```

`new` compte les entrées de catalogue nouvellement embedées ;
`skipped` compte les images dont la paire `(data_cid, band)` était
déjà dans `embeddings.bin` (même contenu téléversé sous plusieurs
chemins).

**Requête par bande** :

```sh
for band in any coarse mid full; do
  echo "--- band=$band ---"
  curl -s "http://127.0.0.1:8787/api/search?q=mountain&band=$band&limit=3" \
    | python3 -m json.tool
done
```

**Résultats attendus** :

- `band=any` retourne la bande au meilleur score par fichier (dedup
  par nom).
- `band=coarse` classe par silhouette / bloc de couleur — les photos
  de paysage avec une ligne d'horizon devraient remonter en tête.
- `band=full` classe par texture — les abstraits de bruit / blocs de
  pixels devraient se réorganiser.
- `band=mid` se situe entre — les images de gradient devraient bien
  scorer.

**Surface UI** : `/search?q=mountain&band=any` montre une grille de
cartes où la vignette grossière de chaque carte fait un cross-fade
vers la pleine résolution. La carte porte un badge de bande coloré
(bleu = grossier, violet = médium, rose = plein).

---

## 21. Colonne robust-copy sur `/similar`

**Objectif** : détecter les paires « la structure correspond, le
détail diffère » (signature filigrane / ré-encodage / retouche
légère).

**Étapes** :

1. Visiter `/similar/photos/brand-pairs/logo-1.png`.
2. Faire défiler jusqu'au tableau « shard overlaps ».

**Attendu** :

- `photos/brand-pairs/logo-1-wm.png` est le **voisin de tête**
  (`shared shards` le plus élevé) — confirme le mécanisme : le
  filigrane localisé en bas à droite préserve la majeure partie des
  shards systématiques LL (couche-0), donc 39+ de ces 192 shards de
  couche-0 hachent identiquement entre la base et la variante
  filigranée. Aucune image sans rapport (mandala, gradient, autre
  marque) ne partage un seul shard de couche-0.
- `low-band %` > 0 (recouvrement couche-0).

**Réserve sur le score** (limitation des données de test
synthétiques, non un bug de la fonctionnalité) : le numérique
`robust copy?` sur l'arborescence d'échantillons ensemencée est
**négatif** pour chaque brand pair, et le glyphe d'avertissement
filigrane +30 ne s'allume jamais ici. La raison est que la passerelle
upscale les échantillons PNG 256×256 à sa résolution de travail
512×512 avant encodage ; l'upsampling bilinéaire/bicubique rend la
bande Haar la plus fine (couche 3) presque entièrement nulle pour
chaque image synthétique lisse. Les K=16 shards systématiques sur
ces zéros hachent à la même valeur « tout à zéro » à travers
**chaque** image de l'arborescence d'échantillons, donc chaque paire
obtient une baseline de ~36 % `high-band %` qui submerge la formule
de score. Sur de vraies photographies avec un détail haute-fréquence
riche, le score passe +30 proprement ; sur cet ensemble de tests,
traiter le **top-rank + recouvrement non nul de couche-0** comme le
signal de succès, non le nombre absolu.

Curl la fonction serveur sous-jacente via la page (navigateurs
uniquement) :

```sh
curl -s 'http://127.0.0.1:8787/similar/photos/brand-pairs/logo-1.png' \
  | grep -oE 'robust_copy_score":-?[0-9.]+'
```

---

## 22. Hologramme streamé

**Objectif** : confirmer que `/preview/stream/<name>` retourne un
corps multipart et que la page côté navigateur `/holo/<name>` marche.

**Sonde curl** :

```sh
curl -sI 'http://127.0.0.1:8787/preview/stream/photos/abstract/mandala-a.png'
# Content-Type devrait être : multipart/x-mixed-replace; boundary=hololayer-<date>
```

**Navigateur** :

1. Visiter `/holo/photos/abstract/mandala-a.png`.
2. Forcer un rechargement (Cmd+Shift+R) pour contourner le cache PNG
   par (name, layer).
3. Regarder l'image se raffiner visiblement — première trame en
   ~dizaines de millisecondes, chaque trame suivante ajoute la valeur
   de détail d'une couche DWT.

**Réserve** : les visites suivantes atteignent le cache et semblent
instantanées. L'échange `<img>` sans JavaScript s'appuie sur
`multipart/x-mixed-replace`, que Chrome et Firefox gèrent
gracieusement.

---

## 23. Modes de spotlight holographique

**Objectif** : restituer le même ROI de deux manières et comparer
visuellement.

```sh
img=photos/landscapes/mountain.png
for mode in spatial coeff; do
  curl -s -o "/tmp/spot-$mode.png" \
       "http://127.0.0.1:8787/api/spotlight.png?name=$img&x=0.35&y=0.35&w=0.3&h=0.3&mode=$mode"
done
file /tmp/spot-*.png
md5 /tmp/spot-*.png    # attendre des hachages distincts
```

**Attendu** : deux PNGs des mêmes dimensions mais avec des octets
distincts.

- `spatial` garde la zone hors du ROI comme une reconstruction L0
  floue-mais-visible.
- `coeff` garde les pixels hors ROI près du noir (la carte inverse de
  Haar met à zéro chaque coefficient qui ne touche pas le ROI).

**En-têtes** :

```sh
curl -sI \
  "http://127.0.0.1:8787/api/spotlight.png?name=$img&x=0.35&y=0.35&w=0.3&h=0.3&mode=coeff" \
  | grep -i 'x-holofs'
```

`x-holofs-roi-px` renvoie le ROI pixel clamped ;
`x-holofs-decode-ms` rapporte le travail serveur ;
`x-holofs-bytes-downloaded` est informationnel (1 le transformera en
un vrai nombre d'économie de bande passante pour `?mode=coeff` sur
les objets répliqués).

**UI** : `/spotlight?a=<image>` expose la bascule de mode + les
préréglages de ROI + un formulaire de coordonnées personnalisées.

---

## 24. Versionnement par objet

**Prérequis** : serveur démarré avec `--enable-versions`. Les PUT
versionnés SAUTENT la purge de shards habituelle afin que le stockage
croisse monotonement tant que le drapeau est activé. Exécuter
`/api/gc` (scénario 25) pour récupérer.

**Étapes** :

1. Choisir un nom cible, p. ex.
   `samples/photos/abstract/mandala-a.png` que vous avez déjà
   téléversé.
2. Téléverser une image différente sur le même chemin :

   ```sh
   curl -sf -X PUT \
        --data-binary @samples/photos/abstract/mandala-b.png \
        http://127.0.0.1:8787/photos/abstract/mandala-a.png
   ```

3. Inspecter l'historique :

   ```sh
   open 'http://127.0.0.1:8787/versions/photos/abstract/mandala-a.png'
   ```

   Attendre au moins une ligne archivée datée de maintenant. Le
   préfixe CID devrait correspondre à l'upload original, non au
   remplaçant.

4. Cliquer « restore » sur la ligne archivée. Confirmer au dialogue.

   ```sh
   # Ou via curl :
   curl -X POST \
        -d 'name=photos/abstract/mandala-a.png&id=v<TS>_<CIDSHORT>' \
        http://127.0.0.1:8787/api/restore
   ```

5. Re-récupérer l'image :

   ```sh
   md5 <(curl -sf http://127.0.0.1:8787/photos/abstract/mandala-a.png)
   ```

**Attendu** : le MD5 post-restore correspond au MD5 pré-remplacement ;
le remplaçant est maintenant lui-même archivé (le restore est
réversible).

---

## 25. GC de shards orphelins + GC d'embeddings (Stages 14.0 / 14.3 / 14.4)

**Objectif** : confirmer que la passerelle récupère les shards non
référencés par aucun manifeste vivant ou archive de version, ET
nettoie les embeddings obsolètes de `embeddings.bin`.

**Étapes** :

1. Déclencher une passe de PUT-remplacement (scénario 24) afin que le
   cluster ait des shards orphelinables.
2. Supprimer les fichiers annexes de version pour ce nom (simule
   l'opérateur retirant l'historique) :

   ```sh
   rm -rf holofs-data/versions/photos__abstract__mandala-a.png
   ```

   (Le script `clean-cluster.sh` fait la même chose en gros.)

3. Exécuter GC :

   ```sh
   curl -s -X POST http://127.0.0.1:8787/api/gc | python3 -m json.tool
   ```

**Attendu** :

- `purged_total` > 0 (les shards précédents sont maintenant non
  référencés).
- `embeddings_dropped` > 0 si des CIDs obsolètes vivaient dans
  l'index.
- `embeddings_kept` correspond au nombre d'enregistrements
  `(data_cid, band)` vivants restants.
- Chaque nœud a `ok: true`, aucun champ `error` défini.
- `duration_ms` typiquement < 100 ms sur le cluster dev.

**Contrôle de concurrence** (optionnel) : exécuter un long PUT + un GC
en parallèle et vérifier que les deux réussissent. La barrière
RwLock dans `Gateway` devrait les sérialiser — le GC attendra que le
PUT finisse, puis s'exécutera seul.

```sh
( curl -sf -X PUT --data-binary @samples/photos/landscapes/ocean.png \
       http://127.0.0.1:8787/race-test.png ) &
sleep 0.2
( curl -sf -X POST http://127.0.0.1:8787/api/gc | python3 -m json.tool ) &
wait
# Les deux devraient se compléter ; `duration_ms` du GC incluera le temps d'attente.
```

---

## 26. Scénarios de fiabilité .x

### 26.1 Compteurs de réparation auto à la lecture

Objectif : vérifier que le bras de retry de `decode_with_autorepair`
ne bouge les compteurs dans `/api/stats` que quand il y a quelque
chose à réparer.

```sh
# Baseline — cluster frais, sain.
curl -s http://127.0.0.1:8787/api/stats | jq '.auto_repairs_total, .auto_repair_failures_total'
# 0
# 0

# Dégradation légère — tuer 3 de 40 nœuds (bien sous la redondance couche-3).
for i in 0 1 2; do curl -X POST -d "i=$i" http://127.0.0.1:8787/admin/node; done
curl -s http://127.0.0.1:8787/photo.png -o /dev/null
curl -s http://127.0.0.1:8787/api/stats | jq '.auto_repairs_total'
# Toujours 0 — la réparation auto ne DOIT PAS se déclencher sous perte légère.

# Dégradation lourde — tuer 60 % du cluster.
for i in $(seq 3 24); do curl -X POST -d "i=$i" http://127.0.0.1:8787/admin/node; done
curl -s http://127.0.0.1:8787/photo.png -o /dev/null
curl -s http://127.0.0.1:8787/api/stats | jq '.auto_repairs_total, .auto_repair_failures_total'
# Au moins un compteur DOIT être ≥ 1.
```

Couvert automatiquement par
`crates/holofs-e2e/tests/auto_repair_e2e.rs`.

### 26.2 Le scrub d'arrière-plan répare proactivement

Objectif : prouver que le scrub attrape la dérive de placement avant
les utilisateurs.

```sh
# Régler le scrub à 15 s pour la démo (le défaut est 600 s).
HOLOFS_SCRUB_INTERVAL=15 \
  cargo run --release --bin holofs-web
# attendre le premier tick :
sleep 20
curl -s http://127.0.0.1:8787/api/stats | jq '.scrub_runs_total'
# 1+ — scrub_repairs_total reste 0 sur un cluster sain.
```

Une démo plus bruyante est dans
`crates/holofs-e2e/tests/reliability_repair.rs::prometheus_metrics_expose_auto_repair_gauges`.

### 26.3 Cluster dégradé → 503, non panic

Objectif : `place_shard` avait l'habitude d'asserter sur un ensemble
vivant vide, faisant planter la passerelle. Maintenant, un PUT contre
un cluster entièrement hors service retourne un 503 propre.

```sh
# Tuer chaque nœud.
for i in $(seq 0 39); do curl -X POST -d "i=$i" http://127.0.0.1:8787/admin/node; done
curl -i -X PUT --data-binary @some.png http://127.0.0.1:8787/test.png
# HTTP/1.1 503 Service Unavailable
# content-type: text/plain
# cluster has no live nodes
```

Après avoir dé-tué les nœuds (`POST /admin/node` bascule), le même
PUT réussit avec 2xx.

Couvert par `crates/holofs-e2e/tests/cluster_degraded.rs`.

### 26.4 Suppression de version + plafond de rétention

Objectif : l'historique par nom ne croît pas sans borne.

```sh
HOLOFS_VERSIONS_KEEP_LAST=2 \
  cargo run --release --bin holofs-web -- --enable-versions

# PUT quatre images différentes sous le même nom.
for body in a.png b.png c.png d.png; do
  curl -X PUT --data-binary @$body http://127.0.0.1:8787/test.png
done

# /api/versions_list — au plus 2 archives, peu importe combien de PUT sont arrivés.
curl -s -X POST -d "name=test.png" http://127.0.0.1:8787/api/versions_list | jq '.versions | length'
# 2

# Suppression manuelle d'une archive — le compteur tombe à 1.
ID=$(curl -s -X POST -d "name=test.png" http://127.0.0.1:8787/api/versions_list | jq -r '.versions[0].id')
curl -X POST -d "name=test.png&id=$ID&return_to=/" http://127.0.0.1:8787/api/versions/delete
```

Couvert par `crates/holofs-e2e/tests/versions_lifecycle.rs`.

### 26.5 cd-dans-le-dossier dans l'arbre du catalogue

Objectif : cliquer « open → » sur un dossier montre UNIQUEMENT le
contenu de ce dossier au niveau supérieur, avec un fil d'Ariane pour
remonter.

```sh
# Ensemencer une arborescence imbriquée (le script d'upload d'échantillons standard) :
tools/test-data/upload-samples.sh

# Visiter le catalogue à /. Déployer `photos/`, puis cliquer « open → » sur
# `landscapes-xl`. L'URL devient `/?p=photos/landscapes-xl` et
# l'arbre montre maintenant les six JPEGs picsum comme entrées de niveau supérieur — pas
# de dossiers frères.
xdg-open http://127.0.0.1:8787/?p=photos/landscapes-xl  # linux
open http://127.0.0.1:8787/?p=photos/landscapes-xl      # macos
```

Les formulaires inline d'upload + mkdir sur chaque ligne `<details>`
atterrissent les fichiers dans le dossier que vous regardiez ; le
formulaire d'upload de la barre d'outils racine se scope au préfixe
`?p=<path>` courant.

Couvert par le smoke test manuel au §18 plus les tests de rendu du
catalogue sous `crates/holofs-e2e/tests/ui_catalog.rs`.

### 26.6 Les échantillons PNG synthétiques se décodent proprement

Objectif : le bug 22-sur-29 cassés est parti.

```sh
tools/test-data/clean-cluster.sh           # stockage frais
HOLOFS_NO_SEED=true \
  cargo run --release --bin holofs-web &
sleep 4
python3 tools/test-data/generate-samples.py
tools/test-data/upload-samples.sh

# Parcourir chaque PNG / JPG sous samples/ et le GET.
broken=0
for f in $(find samples -type f \( -name '*.png' -o -name '*.jpg' \)); do
  code=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:8787/${f#samples/}")
  [ "$code" != "200" ] && broken=$((broken+1))
done
echo "broken=$broken"
# broken=0
```

Le jitter LSB déterministe injecté par `write_png` (x) garantit que
les shards DWT haute-fréquence sont uniques par fichier même sur les
générateurs synthétiques les plus lisses.

---

## Conclusion

Arrêt propre :

```sh
pkill -f 'target/release/holofs-web'
# ou Ctrl-C dans le terminal exécutant le cluster
```

Effacement propre — laisser tomber tout état :

```sh
tools/test-data/clean-cluster.sh
# ou, manuellement :
rm -rf ./holofs-data ./.cluster-data
```

Si quelque chose se comporte mal, comparer aux descriptions ci-dessus
et consulter :

- [docs/fr/operations.md](./operations.md) — configuration et opérations
- [docs/fr/architecture.md](./architecture.md) — flux de données PUT → GET
- [docs/fr/api.md](./api.md) — API HTTP, format du protocole filaire, nouveaux endpoints
- [docs/fr/threat-model.md](./threat-model.md) — menaces couvertes
- `tools/test-data/README.md` — usage de l'arborescence d'échantillons + smoke-runner
