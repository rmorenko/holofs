# Escenarios de prueba

Lista de verificación manual de extremo a extremo para holofs. Cubre cada subsistema importante:
CRUD, catálogo jerárquico, HTTP Range, degradación holográfica,
búsqueda perceptual, escrow, persistencia e i18n. Cada escenario detalla
los comandos, el resultado esperado y los criterios de éxito.

> Orientado al prototipo v0.4.0. Las banderas de CLI, variables de entorno y rutas reflejan el
> código al momento de redactar este documento; si algo varía, consulte
> [docs/operations.md](./operations.md) o `cargo run -p holofs-web -- --help`.

## Contenido

1. [Levantar el clúster](#1-bring-up-the-cluster)
2. [CRUD básico de objetos](#2-basic-object-crud)
3. [Catálogo jerárquico (Etapa 9)](#3-hierarchical-catalog-stage-9)
4. [HTTP Range en GET (Etapa 11.1)](#4-http-range-on-get-stage-111)
5. [Degradación holográfica](#5-holographic-degradation)
6. [Búsqueda perceptual y diff](#6-perceptual-search-and-diff)
7. [Inspect: auditoría visual de shards](#7-inspect-visual-shard-audit)
8. [Holographic Key Escrow](#8-holographic-key-escrow)
9. [Visor de documentación integrado (Etapa 10)](#9-in-app-docs-viewer-stage-10)
10. [i18n: cambio de idioma](#10-i18n-language-switching)
11. [Persistencia y reinicio](#11-persistence-and-restart)
12. [Clúster multiproceso](#12-multi-process-cluster)
13. [TLS / mTLS en el cable](#13-tls--mtls-on-the-wire)
14. [Métricas, registros, SSE](#14-metrics-logs-sse)
15. [Comprobaciones de regresión de la Etapa 11](#15-stage-11-regression-checks)

---

## 1. Levantar el clúster

**Objetivo.** Arrancar un clúster embebido (40 nodes en un único proceso, 4 zonas)
y confirmar que cada node está activo con un catálogo vacío.

```sh
rm -rf ./holofs-data    # fresh start
cargo run --release --bin holofs-web -- \
    --storage ./holofs-data \
    --addr 127.0.0.1:8787 \
    --log info --log-format text \
    --no-seed
```

Líneas de registro esperadas:

```
INFO holofs_web: starting holofs-web version=0.4.0 addr=127.0.0.1:8787
INFO holofs_web::bootstrap: embedded cluster ready n_nodes=40 n_zones=4
INFO holofs_web::bootstrap: catalog loaded objects=0
INFO holofs_web: listening addr=127.0.0.1:8787
```

**Criterios de éxito.**

- `GET http://127.0.0.1:8787/` devuelve el HTML del catálogo (cuadrícula vacía).
- `GET /api/stats` devuelve `{"nodes_total":40,"nodes_live":40,"objects_total":0,…}`.
- 40 puertos escuchando en 9100..9139 (`lsof -nP -iTCP:9100-9139 -sTCP:LISTEN | wc -l` ≥ 40).

Sin `--no-seed` el catálogo se siembra con dos imágenes de demostración
(`photo.png`, `mandala.png`); resulta útil para escenarios posteriores pero
inconveniente para pruebas CRUD limpias.

---

## 2. CRUD básico de objetos

**Objetivo.** Cubrir las cuatro categorías soportadas — imagen / audio / texto / opaco
— más el caso límite perceptual de la deduplicación entre formatos.

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

Cada PUT devuelve JSON `{"name":…,"object_id":…,"shards":…,"put_ms":…}`.

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

**Criterios de éxito.**

- Cada PUT devuelve `201 Created` con un `object_id` distinto de cero.
- GET devuelve los bytes originales del PNG/WAV/texto, byte a byte para imagen
  y opaco (texto permite perder un chunk completo, pero nunca bytes dentro de un chunk).
- `/api/stats.objects_by_kind` refleja los recuentos por categoría.
- `curl -X DELETE http://127.0.0.1:8787/photo.png` devuelve
  `200 {"deleted":"photo.png",…}` y `objects_total` disminuye.

### Deduplicación entre formatos

```sh
# same frame as PNG and BMP — data_cid is identical
convert assets/sample.png /tmp/sample.bmp
curl -X PUT --data-binary @assets/sample.png http://127.0.0.1:8787/a.png
curl -X PUT --data-binary @/tmp/sample.bmp http://127.0.0.1:8787/a.bmp
curl -s http://127.0.0.1:8787/api/stats | python3 -m json.tool | grep dedup
```

`dedup_savings_pct > 0` porque los formatos sin pérdida producen el mismo
`data_cid` → los shards en disco se deduplican.

---

## 3. Catálogo jerárquico (Etapa 9)

**Objetivo.** Verificar mkdir, la navegación a subdirectorios, los rechazos correctos ante
colisiones, rename y rmdir.

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

**Comprobación de la UI.** Abra `http://127.0.0.1:8787/?p=photos/2026/raw` — la ruta de migas
debe leerse `home / photos / 2026 / raw`, el mosaico img.png debe ser clicable
y el formulario "+ folder" debe funcionar.

**Segmentos reservados.** `PUT /health/foo`, `PUT /api/foo`, `PUT /help/foo`,
`PUT /inspect-zoom/foo` devuelven todos `400` — estas rutas no pueden ser ocultadas.

---

## 4. HTTP Range en GET (Etapa 11.1)

**Objetivo.** Confirmar que los GET parciales funcionan — requisito para el scrubbing de audio,
descargas grandes reanudables y el futuro seek de video.

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

**Criterios de éxito.** Los códigos de estado y `Content-Range` coinciden con la tabla
anterior; los bytes recortados son exactos (el patrón `0..255 × 4` devuelve
`00 01 02 03` para `bytes=256-259`).

**Escenario con medios reales.**

```html
<!-- open in a browser, confirm the seek bar works -->
<audio src="http://127.0.0.1:8787/track.wav" controls></audio>
```

El navegador envía `Range` en cada seek. El registro del gateway muestra
respuestas `206 Partial Content`.

---

## 5. Degradación holográfica

**Objetivo.** El truco insignia — cuando una gran fracción del clúster
muere, el archivo aún se decodifica **a menor resolución**. Diríjalo desde la
UI en `/health/<name>`.

1. Arranque con `photo.png` sembrado (omita `--no-seed`).
2. Abra `http://127.0.0.1:8787/health/photo.png`. Obtiene una tabla de márgenes
   por `(channel, layer)`, ejecuciones Monte-Carlo con 10/25/50/75% de pérdida y un
   escenario de fallo de zona completa.
3. Abra `http://127.0.0.1:8787/health`. Una cuadrícula de 40 nodes con botones de **kill**
   / **revive**.
4. Mate los nodes uno por uno y observe `/health/photo.png`:
   - 10–20% de pérdida: margen positivo en todas partes, PSNR ~99 dB.
   - 30–40% de pérdida: el margen de L3 (detalle) → 0, el PSNR cae a ~30 dB — la imagen
     se vuelve más borrosa.
   - 50–60% de pérdida: L2 muere, solo quedan L0+L1 — solo forma gruesa.
   - 75% de pérdida: cada capa muere — la salida colapsa en ruido.
5. Entre pasos obtenga `GET /photo.png` y observe el PNG a ojo.

**Criterios de éxito.**

- Por debajo del umbral K de cada capa — archivo completo.
- Por encima de L3 pero por debajo de L2 — imagen reconocible con las altas frecuencias
  ausentes (más borrosa).
- La tabla de márgenes se actualiza vía SSE (`/api/health/events`) — los números
  cambian tras un kill sin recargar la página.

**Recuperación.** Pulse **revive** en los nodes matados. Tras 1-2 ciclos del
monitor de salud (`HOLOFS_MONITOR_INTERVAL`, por defecto 15s) la autorreparación
se ejecuta y el margen regresa.

### Fallo de zona completa

Cada node lleva una `zone` (0..3). Mate **los 10 nodes** de una zona:
el objeto aún se decodifica hasta L2 gracias a la colocación consciente de zonas
(`ceil(n/z)` shards por zona).

---

## 6. Búsqueda perceptual y diff

**Objetivo.** Encontrar objetos similares mediante un hash perceptual de 16 bytes + observar
la deduplicación a través del diff.

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

**Diff por chunk.**

```
http://127.0.0.1:8787/diff?a=orig.png&b=blurry.png
```

La UI dibuja celdas verdes (chunks coincidentes) y rojas (diferentes). Dos
copias idénticas → 100% verde más un valor alto de `storage_saved_kb`.

---

## 7. Inspect: auditoría visual de shards

**Objetivo.** Confirmar que la cuadrícula muestra los 444 shards (3 canales × 4 capas
× 26..64 por capa) sin huecos. Comprobación de regresión para la Etapa 11.2.

1. Abra `http://127.0.0.1:8787/inspect/mandala.png`.
2. Desplácese — para cada canal (R, G, B) debería ver 4 secciones
   (capas 0..3), cada una con el número correcto de miniaturas:
   - L0 — 64 shards (16 sistemáticos + 48 RLNC)
   - L1 — 40 shards (16 + 24)
   - L2 — 26 shards (16 + 10)
   - L3 — 18 shards (16 + 2)
3. **Las 444 miniaturas deben renderizarse** (sin marcadores rotos de `<img>`).
   Antes de la Etapa 11.2 aproximadamente el 8% se caía bajo carga concurrente.
4. Haga clic en cualquier miniatura → aterrice en `/inspect-zoom/<c_l_idx>/<name>` con
   el PNG grande, los coeficientes en hexadecimal y la carga útil.

**Código de colores.** Los shards sistemáticos (los primeros K=16 de cada capa) tienen
borde verde y llevan carga útil con significado (estructura visible). Los RLNC —
borde naranja, la carga útil parece ruido.

**Prueba de estrés.** Abra 4 pestañas del navegador con `/inspect/photo.png`
simultáneamente — todas se renderizan por completo. El registro del gateway no debe
contener líneas `status=404` para `/api/shard/...`.

---

## 8. Holographic Key Escrow

**Objetivo.** Esquema de umbral al estilo Shamir — dividir un archivo arbitrario en N
shares con umbral K, recuperar a partir de cualesquiera K.

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

**Criterios de éxito.**

- Cualesquiera **K** de N shares reconstruyen el archivo exactamente (byte a byte).
- Con **K-1** shares no se logra (recover devuelve 400).
- Las shares **no se guardan en el clúster** — desaparecen al reiniciar el gateway.
  Descárguelas inmediatamente tras el split; de lo contrario
  `/escrow/download/...` devuelve `410 Gone`.

**UI.** `http://127.0.0.1:8787/escrow` ofrece formularios tanto de split como de recover.

---

## 9. Visor de documentación integrado (Etapa 10)

**Objetivo.** Verificar el visor de documentación y el renderizado de Mermaid y KaTeX.

1. Abra `http://127.0.0.1:8787/help`. La barra lateral izquierda tiene 7 documentos, el
   panel derecho muestra `README.md`.
2. Haga clic en **Architecture**: `/help/architecture` se abre con un diagrama
   **Mermaid** correctamente renderizado (el grafo de dependencias de crates) — SVG
   dibujado del lado del cliente mediante `mermaid.min.js`.
3. Haga clic en **Theory**: muchas fórmulas **KaTeX** (`$x^2 + y^2$`,
   `$$E = mc^2$$`, etc.) — todas renderizadas.
4. Al final de la barra lateral hay un selector de idioma
   (en, ru, de, fr, es). Haga clic en **Русский** — el documento se vuelve a renderizar desde
   `docs/ru/<slug>.md`. Mermaid y KaTeX siguen funcionando (las fórmulas y
   los diagramas son código, no se traducen).
5. Cuando falta una variante localizada, el gateway sirve la versión en inglés
   (`docs/<slug>.md`) con `locale: en` en la línea meta.

**Criterios de éxito.**

- Los 7 documentos se abren en los 5 idiomas sin 404.
- Los diagramas Mermaid son SVG reales, no código sin procesar dentro de un `<div>`.
- Las fórmulas KaTeX aparecen como matemáticas compuestas, no como fuente TeX.
- La barra lateral resalta el documento activo (clase `.active`).

---

## 10. i18n: cambio de idioma

**Objetivo.** La UI funciona en 5 idiomas en cada ruta.

1. Abra cualquier página (`/`, `/help`, `/escrow`).
2. El lado derecho de la barra superior lleva un selector compacto:
   `en · ru · de · fr · es`.
3. Recórralos:
   - `?lang=ru` → "каталог", "состояние", "эскроу", "помощь".
   - `?lang=de` → "Katalog", "Zustand", "Treuhand", "Hilfe".
   - `?lang=fr` → "catalogue", "santé", "séquestre", "aide".
   - `?lang=es` → "catálogo", "estado", "depósito", "ayuda".
4. La URL se reescribe mediante `rewrite_lang` — la ruta y los demás parámetros de consulta
   (`?p=…`, `?a=&b=…`) se preservan.

**Localización desconocida.** `?lang=ja` o cualquier otra recae en inglés.

---

## 11. Persistencia y reinicio

**Objetivo.** Confirmar que los datos sobreviven a un reinicio.

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

**Criterios de éxito.**

- `catalog.bin` (~KB) y los shards (`node_*/<hex>/<hex>.shard`) están intactos.
- Tras el reinicio `GET` devuelve el original byte a byte.
- Las identidades de los nodes (`node_*/identity.key`) son estables — las claves públicas coinciden
  con los valores previos al reinicio.

---

## 12. Clúster multiproceso

**Objetivo.** Ejercitar el modo distribuido "real" — nodes como procesos
separados.

```sh
./scripts/spawn-cluster.sh 8 5100 127.0.0.1:8787
```

El script:

1. Lanza 8 procesos `holofs-node` con almacenamiento bajo
   `.cluster-data/node-N`.
2. Recopila sus claves públicas Ed25519.
3. Genera un par de claves de administrador y firma la whitelist.
4. Inicia el gateway con `--whitelist`.

En otra terminal:

```sh
curl -X PUT --data-binary @assets/sample.png http://127.0.0.1:8787/test.png

# shards spread across the 8 processes
for i in 0 1 2 3 4 5 6 7; do
    n=$(find .cluster-data/node-$i -name '*.shard' 2>/dev/null | wc -l)
    echo "node-$i: $n shards"
done
```

**Criterios de éxito.**

- La suma de shards a través de los nodes ≈ 444 (×3 canales × recuentos por capa).
- Ctrl-C en el script detiene los 8 nodes y el gateway.
- Volver a ejecutar el mismo script (sin borrar `.cluster-data/`) restaura
  el estado previo — los datos en disco están intactos.

---

## 13. TLS / mTLS en el cable

**Objetivo.** Habilitar TLS opcional en el tráfico gateway ↔ node.

```sh
# embedded mode — a self-signed CA is generated automatically
HOLOFS_TLS=1 cargo run --release --bin holofs-web -- \
    --storage ./holofs-data-tls --no-seed
```

Registro: `TLS material self-signed; ca → ./holofs-data-tls/tls/ca.crt`.

Autenticación mutua:

```sh
HOLOFS_TLS=1 HOLOFS_MTLS=1 cargo run --release --bin holofs-web -- \
    --storage ./holofs-data-mtls --no-seed
```

**Qué verificar.**

- El tráfico en 9100..9139 ya no es TCP en claro — `tcpdump` en loopback
  muestra handshakes TLS (`16 03 ...`).
- PUT/GET/inspect funcionan igual que sin TLS.
- Sin `--tls`, las conexiones permanecen en claro — retrocompatible.

Los detalles de PKI y el flujo del modo distribuido con certificados aportados por el operador
viven en [docs/operations.md](./operations.md).

---

## 14. Métricas, registros, SSE

**Prometheus.**

```sh
curl http://127.0.0.1:8787/metrics
```

Esperado:

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

**Registros estructurados.**

```sh
cargo run --release --bin holofs-web -- \
    --storage ./holofs-data --log-format json --log info
```

Cada línea es JSON con `timestamp`, `level`, `target`, `fields`.
Conveniente para journald / fluentd / Vector / Loki.

**Stream SSE de salud.**

```sh
curl -N http://127.0.0.1:8787/api/health/events
```

Aproximadamente cada 3 segundos llega una trama `event: health\ndata: {…}\n\n`
con una instantánea JSON — esto es lo que alimenta el panel en vivo `/health`.

---

## 15. Comprobaciones de regresión de la Etapa 11

Tres sondeos rápidos dirigidos a problemas corregidos recientemente. Ejecútelos tras cualquier
cambio al gateway o al pipeline de ingesta.

### 11.1 Range en medios

```sh
curl -i -H "Range: bytes=0-99" http://127.0.0.1:8787/photo.png \
    | head -10
```

Esperar `206 Partial Content`, `Content-Range: bytes 0-99/<total>`,
`Content-Length: 100`. Ni 200, ni 416.

### 11.2 Inspect no descarta shards

Abra `http://127.0.0.1:8787/inspect/mandala.png` en un navegador. Las 444
miniaturas deben renderizarse. En el registro:

```sh
grep -E "/api/shard/" /tmp/holofs-cluster.log | grep -v "status=200" | wc -l
```

Esperado **0**. Antes de la Etapa 11.2 era ~41.

### 11.3 Subidas multipart grandes

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

Ambos deben devolver `200`/`201`, no `400 multipart read: Error parsing`.

---

## Cierre

Parada limpia:

```sh
pkill -f 'target/release/holofs-web'
# or Ctrl-C in the terminal running the cluster
```

Limpieza total — descartar todo el estado:

```sh
rm -rf ./holofs-data ./.cluster-data
```

Si algo se comporta de forma incorrecta, compárelo con las descripciones anteriores y
consulte:

- [docs/operations.md](./operations.md) — configuración y operaciones
- [docs/architecture.md](./architecture.md) — flujo de datos PUT → GET
- [docs/api.md](./api.md) — API HTTP, formato del protocolo de cable
- [docs/threat-model.md](./threat-model.md) — amenazas cubiertas
