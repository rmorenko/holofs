
# Escenarios de prueba

Lista de comprobación manual end-to-end para holofs. Cubre cada
subsistema principal — CRUD, catálogo jerárquico, HTTP Range,
degradación holográfica, búsqueda perceptual, escrow, persistencia e
i18n. Cada escenario lista comandos, resultado esperado y marcadores
de éxito.

> Apunta a 1.0.0. Los flags CLI, env vars y rutas reflejan
> el código al momento de escritura; si algo deriva, consulta
> [docs/es/operations.md](./operations.md) o
> `cargo run -p holofs-web -- --help`.

## Contenido

1. [Levantar el clúster](#1-levantar-el-clúster)
2. [CRUD básico de objetos](#2-crud-básico-de-objetos)
3. [Catálogo jerárquico](#3-catálogo-jerárquico)
4. [HTTP Range en GET (1)](#4-http-range-en-get)
5. [Degradación holográfica](#5-degradación-holográfica)
6. [Búsqueda perceptual y diff](#6-búsqueda-perceptual-y-diff)
7. [Inspeccionar: auditoría visual de shards](#7-inspeccionar-auditoría-visual-de-shards)
8. [Escrow holográfico de claves](#8-escrow-holográfico-de-claves)
9. [Visor de docs en la app](#9-visor-de-docs-en-la-app)
10. [i18n: cambio de idioma](#10-i18n-cambio-de-idioma)
11. [Persistencia y reinicio](#11-persistencia-y-reinicio)
12. [Clúster multi-proceso](#12-clúster-multi-proceso)
13. [TLS / mTLS en el cable](#13-tls--mtls-en-el-cable)
14. [Métricas, logs, SSE](#14-métricas-logs-sse)
15. [Comprobaciones de regresión](#15-comprobaciones-de-regresión)
16. [Comprobaciones de regresión de Etapa 12](#16-comprobaciones-de-regresión-de-etapa-12)
17. [Operaciones wavelet](#17-operaciones-wavelet)
18. [Quickstart con el árbol de muestra](#18-quickstart-con-el-árbol-de-muestra)
19. [Página de métricas por archivo](#19-página-de-métricas-por-archivo)
20. [Búsqueda semántica CLIP + bandas (Etapas 12.8 / 12.9 / 13.3)](#20-búsqueda-semántica-clip--bandas-etapas-128--129--133)
21. [Columna robust-copy en `/similar`](#21-columna-robust-copy-en-similar)
22. [Holograma en streaming](#22-holograma-en-streaming)
23. [Modos de spotlight holográfico](#23-modos-de-spotlight-holográfico)
24. [Versionado por objeto](#24-versionado-por-objeto)
25. [GC de shard huérfano + GC de embedding (Etapas 14.0 / 14.3 / 14.4)](#25-gc-de-shard-huérfano--gc-de-embedding-etapas-140--143--144)
26. [Escenarios de fiabilidad .x](#26-escenarios-de-fiabilidad-x)
27. [Cierre](#cierre)

---

## 1. Levantar el clúster

**Objetivo.** Arrancar un clúster embebido (40 nodos en un proceso, 4
zonas) y confirmar que cada nodo está vivo con un catálogo vacío.

```sh
rm -rf ./holofs-data    # inicio limpio
cargo run --release --bin holofs-web -- \
    --storage ./holofs-data \
    --addr 127.0.0.1:8787 \
    --log info --log-format text \
    --no-seed
```

Líneas de log esperadas:

```
INFO holofs_web: starting holofs-web version=1.0.0 addr=127.0.0.1:8787
INFO holofs_web::bootstrap: embedded cluster ready n_nodes=40 n_zones=4
INFO holofs_web::bootstrap: catalog loaded objects=0
INFO holofs_web: listening addr=127.0.0.1:8787
```

**Marcadores de éxito.**

- `GET http://127.0.0.1:8787/` devuelve el HTML del catálogo (grid vacía).
- `GET /api/stats` devuelve `{"nodes_total":40,"nodes_live":40,"objects_total":0,…}`.
- 40 puertos escuchando en 9100..9139 (`lsof -nP -iTCP:9100-9139 -sTCP:LISTEN | wc -l` ≥ 40).

Sin `--no-seed` el catálogo se auto-semilla con dos imágenes de demo
(`photo.png`, `mandala.png`); útil para escenarios subsiguientes pero
inconveniente para pruebas CRUD limpias.

---

## 2. CRUD básico de objetos

**Objetivo.** Cubrir los cuatro tipos soportados — image / audio / text
/ opaque — más el caso límite perceptual de dedup cross-formato.

```sh
# imagen (PNG → tipo image, DWT + RLNC en 4 capas)
curl -X PUT --data-binary @assets/sample.png http://127.0.0.1:8787/photo.png

# texto (UTF-8 → tipo text, en chunks + recuperación parcial)
printf "Hello, holofs!\nLine 2\n" | curl -X PUT --data-binary @- \
    http://127.0.0.1:8787/note.txt

# opaco (binario arbitrario → 1 capa RLNC, sin DWT)
dd if=/dev/urandom of=/tmp/blob.bin bs=1024 count=100 2>/dev/null
curl -X PUT --data-binary @/tmp/blob.bin http://127.0.0.1:8787/blob.bin

# audio (WAV → tipo audio, DWT 1D por canal)
# (omitir si no tienes un archivo wav a mano)
curl -X PUT --data-binary @track.wav http://127.0.0.1:8787/track.wav
```

Cada PUT devuelve JSON `{"name":…,"object_id":…,"shards":…,"put_ms":…}`.

```sh
# GET — recuperación byte-perfect (decode de calidad completa)
curl -s -o /tmp/got.png http://127.0.0.1:8787/photo.png
cmp assets/sample.png /tmp/got.png && echo "image roundtrip OK"

# Preview = solo L0
curl -s -o /tmp/prev.png http://127.0.0.1:8787/preview/photo.png
file /tmp/prev.png   # debería ser imagen PNG

# Estadísticas del catálogo
curl -s http://127.0.0.1:8787/api/stats | python3 -m json.tool
```

**Marcadores de éxito.**

- Cada PUT devuelve `201 Created` con un `object_id` no cero.
- GET devuelve los bytes originales PNG/WAV/text, byte-perfect para
  image y opaque (text permite pérdida de chunks enteros, nunca de
  bytes dentro de un chunk).
- `/api/stats.objects_by_kind` refleja los contadores por tipo.
- `curl -X DELETE http://127.0.0.1:8787/photo.png` devuelve
  `200 {"deleted":"photo.png",…}` y `objects_total` cae.

### Dedup cross-formato

```sh
# mismo frame como PNG y BMP — data_cid es idéntico
convert assets/sample.png /tmp/sample.bmp
curl -X PUT --data-binary @assets/sample.png http://127.0.0.1:8787/a.png
curl -X PUT --data-binary @/tmp/sample.bmp http://127.0.0.1:8787/a.bmp
curl -s http://127.0.0.1:8787/api/stats | python3 -m json.tool | grep dedup
```

`dedup_savings_pct > 0` porque los formatos sin pérdidas producen el
mismo `data_cid` → los shards en disco están deduplicados.

---

## 3. Catálogo jerárquico

**Objetivo.** Verificar mkdir, navegación en subdirs, rechazos
correctos en colisión, rename, rmdir.

```sh
# construir un árbol
curl -X POST -d "parent=&name=photos"          http://127.0.0.1:8787/api/mkdir
curl -X POST -d "parent=photos&name=2026"      http://127.0.0.1:8787/api/mkdir
curl -X POST -d "parent=photos/2026&name=raw"  http://127.0.0.1:8787/api/mkdir

# subir en profundidad
curl -X PUT --data-binary @assets/sample.png \
    http://127.0.0.1:8787/photos/2026/raw/img.png

# recuperar
curl -o /tmp/got.png http://127.0.0.1:8787/photos/2026/raw/img.png
cmp assets/sample.png /tmp/got.png && echo "deep path roundtrip OK"

# rechazos
curl -i -X POST -d "parent=does/not/exist&name=x" \
    http://127.0.0.1:8787/api/mkdir         # 400 padre faltante
curl -i -X DELETE http://127.0.0.1:8787/api/rmdir/photos
                                            # 409 directory not empty
curl -i -X DELETE http://127.0.0.1:8787/photos
                                            # 409 is a directory

# renombrar (arrastra cada descendiente)
curl -X POST -d "from=photos&to=archive" http://127.0.0.1:8787/api/mv
curl -s http://127.0.0.1:8787/archive/2026/raw/img.png -o /tmp/r.png
cmp assets/sample.png /tmp/r.png && echo "rename moved descendants"

# rmdir de abajo hacia arriba
curl -X DELETE http://127.0.0.1:8787/archive/2026/raw/img.png
curl -X DELETE http://127.0.0.1:8787/api/rmdir/archive/2026/raw
curl -X DELETE http://127.0.0.1:8787/api/rmdir/archive/2026
curl -X DELETE http://127.0.0.1:8787/api/rmdir/archive
```

**Chequeo de UI.** Abrir `http://127.0.0.1:8787/?p=photos/2026/raw` —
el breadcrumb debería leer `home / photos / 2026 / raw`, la baldosa
img.png es clicable, el formulario "+ folder" funciona.

**Segmentos reservados.** `PUT /health/foo`, `PUT /api/foo`,
`PUT /help/foo`, `PUT /inspect-zoom/foo` todos devuelven `400` —
estas rutas no pueden ser ocultadas.

---

## 4. HTTP Range en GET

**Objetivo.** Confirmar que los GETs parciales funcionan — requerido
para scrubbing de audio, descargas grandes resumibles, futura búsqueda
en video.

```sh
# blob de 1000 bytes
printf 'A%.0s' $(seq 1 1000) > /tmp/blob.bin
curl -X PUT --data-binary @/tmp/blob.bin http://127.0.0.1:8787/range-test

# GET completo — 200, Accept-Ranges: bytes
curl -I http://127.0.0.1:8787/range-test 2>&1 | grep -i accept-ranges

# primeros 10 bytes
curl -i -H "Range: bytes=0-9" http://127.0.0.1:8787/range-test
# esperado 206 Partial Content, content-range: bytes 0-9/1000

# últimos 50 bytes
curl -i -H "Range: bytes=-50" http://127.0.0.1:8787/range-test
# content-range: bytes 950-999/1000

# intervalo abierto
curl -i -H "Range: bytes=900-" http://127.0.0.1:8787/range-test
# content-range: bytes 900-999/1000

# pasado EOF — 416
curl -i -H "Range: bytes=5000-" http://127.0.0.1:8787/range-test
# 416 Range Not Satisfiable, content-range: bytes */1000

# multi-rango no soportado — degrada a 200 (cuerpo completo)
curl -i -H "Range: bytes=0-9,20-29" http://127.0.0.1:8787/range-test
# 200 OK, 1000 bytes
```

**Marcadores de éxito.** Códigos de estado y `Content-Range` coinciden
con la tabla de arriba; los bytes en rebanadas son byte-exactos (el
patrón `0..255 × 4` devuelve `00 01 02 03` para `bytes=256-259`).

**Escenario de medios reales.**

```html
<!-- abrir en un navegador, confirmar que la barra de búsqueda funciona -->
<audio src="http://127.0.0.1:8787/track.wav" controls></audio>
```

El navegador envía `Range` en cada búsqueda. El log del gateway
muestra respuestas `206 Partial Content`.

---

## 5. Degradación holográfica

**Objetivo.** El truco insignia — cuando una gran fracción del clúster
muere, el archivo aún decodifica **a menor resolución**. Impulsalo
desde la UI en `/health/<name>`.

1. Arrancar con `photo.png` sembrado (quitar `--no-seed`).
2. Abrir `http://127.0.0.1:8787/health/photo.png`. Obtienes una tabla
   de margen por `(canal, capa)`, ejecuciones Monte-Carlo al 10/25/50/75%
   de pérdida, y un escenario de fallo de zona completa.
3. Abrir `http://127.0.0.1:8787/health`. Una grid de 40 nodos con
   botones **kill** / **revive**.
4. Matar nodos uno a uno y observar `/health/photo.png`:
   - 10–20% de pérdida: margen positivo en todas partes, PSNR ~99 dB.
   - 30–40% de pérdida: margen de L3 (detalle) → 0, PSNR cae a
     ~30 dB — la imagen se pone más borrosa.
   - 50–60% de pérdida: L2 muere, solo quedan L0+L1 — solo forma gruesa.
   - 75% de pérdida: cada capa muere — el output colapsa a ruido.
5. Entre pasos hacer fetch `GET /photo.png` y mirar el PNG.

**Marcadores de éxito.**

- Por debajo del umbral K de cada capa — archivo completo.
- Por encima de L3 pero por debajo de L2 — imagen reconocible con las
  altas frecuencias ausentes (más borrosa).
- La tabla de margen se actualiza sobre SSE
  (`/api/health/events`) — los números cambian tras un kill sin
  recargado de página.

**Recuperación.** Pulsar **revive** en los nodos matados. Después de
1-2 ciclos del monitor de salud (`HOLOFS_MONITOR_INTERVAL`, por
defecto 15s) la auto-reparación se ejecuta y el margen vuelve.

### Fallo de zona completa

Cada nodo lleva una `zone` (0..3). Matar **los 10 nodos** de una zona:
el objeto aún decodifica hasta L2 gracias al placement con conciencia
de zona (`ceil(n/z)` shards por zona).

---

## 6. Búsqueda perceptual y diff

**Objetivo.** Encontrar objetos similares por un hash perceptual de 16
bytes + observar el dedup a través de diff.

```sh
# subir dos versiones similares de la misma imagen
curl -X PUT --data-binary @assets/sample.png http://127.0.0.1:8787/orig.png
convert assets/sample.png -gaussian-blur 0x2 /tmp/blurry.png
curl -X PUT --data-binary @/tmp/blurry.png http://127.0.0.1:8787/blurry.png

# top-10 vecinos
curl -s 'http://127.0.0.1:8787/api/fingerprint/orig.png' | python3 -m json.tool
# →  {"name":"orig.png","fingerprint":"a3f0…","kind":"image"}

# UI: abrir /similar/orig.png — vecinos ordenados por distancia L1.
```

**Diff por chunk.**

```
http://127.0.0.1:8787/diff?a=orig.png&b=blurry.png
```

La UI dibuja celdas verdes (chunks coincidentes) y rojas (diferentes).
Dos copias idénticas → 100 % verde más un `storage_saved_kb` grande.

---

## 7. Inspeccionar: auditoría visual de shards

**Objetivo.** Confirmar que la grid muestra todos los 444 shards
(3 canales × 4 capas × 26..64 por capa) sin huecos. Comprobación de
regresión para
1. Abrir `http://127.0.0.1:8787/inspect/mandala.png`.
2. Hacer scroll — para cada canal (R, G, B) deberías ver 4 secciones
   (capas 0..3), cada una con el conteo correcto de miniaturas:
   - L0 — 64 shards (16 sistemáticos + 48 RLNC)
   - L1 — 40 shards (16 + 24)
   - L2 — 26 shards (16 + 10)
   - L3 — 18 shards (16 + 2)
3. **Las 444 miniaturas deben renderizarse** (sin marcadores `<img>`
   rotos). Antes del fix ~8 % caían bajo carga concurrente.
4. Hacer click en cualquier miniatura → aterrizar en
   `/inspect-zoom/<c_l_idx>/<name>` con el PNG grande, coeffs hex,
   payload.

**Codificación de color.** Los shards sistemáticos (primeros K=16 de
cada capa) tienen borde verde y llevan payload significativo
(estructura visible). RLNC — borde naranja, el payload parece ruido.

**Prueba de estrés.** Abrir 4 pestañas de navegador de
`/inspect/photo.png` simultáneamente — cada una renderiza
completamente. El log del gateway no debe contener líneas
`status=404` para `/api/shard/...`.

---

## 8. Escrow holográfico de claves

**Objetivo.** Esquema de borrado RLNC con umbral (NO Shamir — véase
theory.md §8) — dividir un archivo arbitrario en N partes con umbral
K, recuperar desde cualquier K. Solo como respaldo de disponibilidad;
K−1 partes filtran ≈(K−1)/K del texto plano, así que
encrypt-then-share para secreto real.

```sh
echo "my secret seed phrase" > /tmp/secret.txt

# dividir 3-de-5
curl -F "file=@/tmp/secret.txt" -F "k=3" -F "n=5" \
    -o /tmp/split.html http://127.0.0.1:8787/escrow/split

# el HTML lleva enlaces /escrow/download/<eid>_<idx>.holoshare
grep -oE '/escrow/download/[a-f0-9]+_[0-9]+\.holoshare' /tmp/split.html \
    | head -5

# descargar cualesquiera 3 partes
for idx in 0 2 4; do
    eid=$(grep -oE '[a-f0-9]{32}' /tmp/split.html | head -1)
    curl -o /tmp/share_${idx}.holoshare \
        "http://127.0.0.1:8787/escrow/download/${eid}_${idx}.holoshare"
done

# recuperar desde 3 partes
curl -F "shares=@/tmp/share_0.holoshare" \
     -F "shares=@/tmp/share_2.holoshare" \
     -F "shares=@/tmp/share_4.holoshare" \
     -o /tmp/recovered.txt http://127.0.0.1:8787/escrow/recover
cmp /tmp/secret.txt /tmp/recovered.txt && echo "escrow roundtrip OK"
```

**Marcadores de éxito.**

- Cualesquiera **K** de N partes reconstruyen el archivo exactamente
  (byte-perfect).
- **K-1** partes no lo hacen (recover devuelve 400).
- Las partes **no se almacenan en el clúster** — desaparecen al
  reiniciar el gateway. Descargar justo después del split; de lo
  contrario `/escrow/download/...` devuelve `410 Gone`.

**UI.** `http://127.0.0.1:8787/escrow` lleva ambos formularios de
split y recover.

---

## 9. Visor de docs en la app

**Objetivo.** Verificar el visor de docs, renderizado de Mermaid y
KaTeX.

1. Abrir `http://127.0.0.1:8787/help`. La barra lateral izquierda
   tiene 7 documentos, el panel derecho muestra `README.md`.
2. Hacer click en **Architecture**: `/help/architecture` se abre con
   un diagrama **Mermaid** correctamente renderizado (el grafo de
   dependencias entre crates) — SVG dibujado del lado del cliente vía
   `mermaid.min.js`.
3. Hacer click en **Theory**: muchas fórmulas **KaTeX**
   (`$x^2 + y^2$`, `$$E = mc^2$$`, etc.) — cada una renderizada.
4. La parte inferior de la barra lateral lleva un cambiador de idioma
   (en, ru, de, fr, es). Hacer click en **Русский** — el doc se
   re-renderiza desde `docs/ru/<slug>.md`. Mermaid y KaTeX siguen
   funcionando (las fórmulas y diagramas son código, no traducidos).
5. Donde falte una variante localizada, el gateway sirve la inglesa
   (`docs/<slug>.md`) con `locale: en` en la línea meta.

**Marcadores de éxito.**

- Los 7 documentos se abren en los 5 idiomas sin 404.
- Los diagramas Mermaid son SVGs reales, no código crudo en un `<div>`.
- Las fórmulas KaTeX aparecen como matemáticas tipografiadas, no
  fuente TeX.
- La barra lateral resalta el documento activo (clase `.active`).

---

## 10. i18n: cambio de idioma

**Objetivo.** La UI funciona en 5 idiomas en cada ruta.

1. Abrir cualquier página (`/`, `/help`, `/escrow`).
2. El lado derecho de la barra superior lleva un cambiador compacto:
   `en · ru · de · fr · es`.
3. Ciclar:
   - `?lang=ru` → "каталог", "состояние", "эскроу", "помощь".
   - `?lang=de` → "Katalog", "Zustand", "Treuhand", "Hilfe".
   - `?lang=fr` → "catalogue", "santé", "séquestre", "aide".
   - `?lang=es` → "catálogo", "estado", "depósito", "ayuda".
4. La URL se reescribe vía `rewrite_lang` — la ruta y otros parámetros
   de query (`?p=…`, `?a=&b=…`) se preservan.

**Locale desconocido.** `?lang=ja` o cualquier otra cosa recae en
inglés.

---

## 11. Persistencia y reinicio

**Objetivo.** Confirmar que los datos sobreviven a un reinicio.

```sh
# 1. sembrar el clúster
cargo run --release --bin holofs-web -- --storage ./holofs-data --no-seed &
SERVER_PID=$!
sleep 5

curl -X POST -d "parent=&name=keep" http://127.0.0.1:8787/api/mkdir
curl -X PUT --data-binary @assets/sample.png \
    http://127.0.0.1:8787/keep/img.png

# 2. parar
kill $SERVER_PID; wait $SERVER_PID 2>/dev/null

# 3. verificar que el estado en disco está en su lugar
ls -la ./holofs-data/catalog.bin
ls ./holofs-data/node_00 | head -5    # debería contener archivos .shard

# 4. reiniciar
cargo run --release --bin holofs-web -- --storage ./holofs-data --no-seed &
sleep 5

# 5. el objeto y el catálogo volvieron
curl -s 'http://127.0.0.1:8787/api/list_dir' \
    -X POST -d '' -H 'Content-Type: application/json' \
    --data-raw '{"prefix":""}'

curl -o /tmp/after-restart.png http://127.0.0.1:8787/keep/img.png
cmp assets/sample.png /tmp/after-restart.png && echo "persistence OK"
```

**Marcadores de éxito.**

- `catalog.bin` (~KB) y shards (`node_*/<hex>/<hex>.shard`) están
  intactos.
- Tras el reinicio `GET` devuelve el original byte-for-byte.
- Las identidades de nodo (`node_*/identity.key`) son estables — las
  pubkeys coinciden con los valores pre-reinicio.

---

## 12. Clúster multi-proceso

**Objetivo.** Ejercitar el modo "real" distribuido — nodos como
procesos separados.

```sh
./scripts/spawn-cluster.sh 8 5100 127.0.0.1:8787
```

El script:

1. Genera 8 procesos `holofs-node` con almacenamiento bajo
   `.cluster-data/node-N`.
2. Recolecta sus pubkeys Ed25519.
3. Genera un par de claves de admin y firma la whitelist.
4. Inicia el gateway con `--whitelist`.

En otra terminal:

```sh
curl -X PUT --data-binary @assets/sample.png http://127.0.0.1:8787/test.png

# los shards se distribuyen entre los 8 procesos
for i in 0 1 2 3 4 5 6 7; do
    n=$(find .cluster-data/node-$i -name '*.shard' 2>/dev/null | wc -l)
    echo "node-$i: $n shards"
done
```

**Marcadores de éxito.**

- La suma de shards entre nodos ≈ 444 (×3 canales × conteos por capa).
- Ctrl-C en el script para los 8 nodos y el gateway.
- Volver a ejecutar el mismo script (sin limpiar `.cluster-data/`)
  restaura el estado previo — los datos en disco están intactos.

---

## 13. TLS / mTLS en el cable

**Objetivo.** Habilitar TLS opt-in en el tráfico gateway ↔ nodo.

```sh
# modo embebido — se genera automáticamente una CA autofirmada
HOLOFS_TLS=1 cargo run --release --bin holofs-web -- \
    --storage ./holofs-data-tls --no-seed
```

Log: `TLS material self-signed; ca → ./holofs-data-tls/tls/ca.crt`.

Auth mutua:

```sh
HOLOFS_TLS=1 HOLOFS_MTLS=1 cargo run --release --bin holofs-web -- \
    --storage ./holofs-data-mtls --no-seed
```

**Qué verificar.**

- El tráfico en 9100..9139 ya no es TCP plano — `tcpdump` en loopback
  muestra handshakes TLS (`16 03 ...`).
- PUT/GET/inspect funcionan igual que sin TLS.
- Sin `--tls`, las conexiones permanecen planas — compatibilidad hacia
  atrás.

Los detalles PKI y el flujo de modo distribuido con certs
suministrados por el operador viven en
[docs/es/operations.md](./operations.md).

---

## 14. Métricas, logs, SSE

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

**Logs estructurados.**

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

Aproximadamente cada 3 segundos llega un frame
`event: health\ndata: {…}\n\n` con un snapshot JSON — esto es lo que
impulsa el dashboard `/health` en vivo.

---

## 15. Comprobaciones de regresión

Tres sondas rápidas que apuntan a issues recientemente arreglados.
Ejecutarlas tras cualquier cambio al gateway o al pipeline de ingesta.

### 15.1 Range en medios

```sh
curl -i -H "Range: bytes=0-99" http://127.0.0.1:8787/photo.png \
    | head -10
```

Esperado `206 Partial Content`, `Content-Range: bytes 0-99/<total>`,
`Content-Length: 100`. No 200, no 416.

### 15.2 Inspect no descarta shards

Abrir `http://127.0.0.1:8787/inspect/mandala.png` en un navegador.
Las 444 miniaturas deben renderizarse. En el log:

```sh
grep -E "/api/shard/" /tmp/holofs-cluster.log | grep -v "status=200" | wc -l
```

Esperado **0**. Antes del fix esto era ~41.

### 15.3 Uploads multipart grandes

```sh
# archivo de 3 MB vía escrow
dd if=/dev/urandom of=/tmp/3mb.bin bs=1024 count=3000 2>/dev/null
curl -i -F "file=@/tmp/3mb.bin" -F "k=3" -F "n=5" \
    http://127.0.0.1:8787/escrow/split 2>&1 | head -1

# 30 MB vía PUT
dd if=/dev/urandom of=/tmp/30mb.bin bs=1024 count=30000 2>/dev/null
curl -i -X PUT --data-binary @/tmp/30mb.bin \
    http://127.0.0.1:8787/big.bin 2>&1 | head -1
```

Ambos deben devolver `200`/`201`, no `400 multipart read: Error parsing`.

---

## 16. Comprobaciones de regresión de Etapa 12

### 16.1 Similar scope

Tres píldoras de scope en la parte superior de `/similar/<name>`:
**todos los archivos** / **carpeta actual** / **carpeta actual
(recursivo)**.

```sh
# sin restricciones (por defecto legacy — top-10 en todo el catálogo)
curl -s 'http://127.0.0.1:8787/similar/check.txt' \
  | grep -oE 'top similar \(<!>[0-9]+'

# solo archivos dentro del mismo directorio padre
curl -s 'http://127.0.0.1:8787/similar/check.txt?scope=folder' \
  | grep -oE 'top similar \(<!>[0-9]+'

# subárbol del padre (raíz → catálogo completo, equivalente a `all`)
curl -s 'http://127.0.0.1:8787/similar/check.txt?scope=tree' \
  | grep -oE 'top similar \(<!>[0-9]+'
```

El scope es pegajoso — hacer click en un vecino navega a su propia URL
`/similar` con el mismo `?scope=` (y `?lang=`) preservado.

### 16.2 Filtro de catálogo + eliminación de archivo

Filtro del lado del servidor en `/` y `/?p=<prefix>` vía tres parámetros
de query: `q` (glob de nombre, `*` = comodín, coincidencia por basename,
insensible a mayúsculas), `from`, `to` (`YYYY-MM-DD`, rango sobre
`created_at_unix`).

```sh
# todos los archivos PNG
curl -s 'http://127.0.0.1:8787/?q=*.png' \
  | grep -oE 'class="tree-leaf' | wc -l

# combinado: archivos de texto añadidos en 2026
curl -s 'http://127.0.0.1:8787/?q=*.txt&from=2026-01-01' \
  | grep -oE 'class="tree-leaf' | wc -l
```

La vista de árbol mantiene los directorios ancestros de cualquier
hoja retenida para que las rutas permanezcan navegables. Las entradas
legacy con `created_at_unix=0` (HOLOFSM6/HOLOFSM7) siempre pasan
cualquier filtro de fecha.

La eliminación de archivo es un espejo form-POST del `rmdir_form`
existente:

```sh
curl -s -X PUT 'http://127.0.0.1:8787/tmp-delete-me.txt' \
  -H 'Content-Type: text/plain' --data 'will be deleted'
curl -s -o /dev/null -w '%{http_code}\n' \
  -X POST 'http://127.0.0.1:8787/api/rm' \
  -H 'Content-Type: application/x-www-form-urlencoded' \
  --data 'path=tmp-delete-me.txt&return_to=/'
# esperado 303 (redirección a return_to en éxito)
```

Las filas hoja del árbol renderizan un pequeño botón `✕` con un
prompt de confirmación.

### 16.3 Date picker localizado

`<input type="date">` nativo en la barra de filtro lleva un atributo
`lang` que coincide con el locale de la página; en navegadores
Chromium un overlay flatpickr (cargado desde jsdelivr) reemplaza al
picker nativo para que el calendario siempre hable el idioma de la
página, no el locale del SO.

Visitar `/?lang=ru`, hacer click en un campo de fecha — la cabecera
del calendario está en ruso. Cambiar a `/?lang=fr`, repetir — francés.
El `value=…` viaja como `YYYY-MM-DD` independientemente del locale.

### 16.4 Prueba de humo del servidor MCP

Iniciar el clúster con un token para que las herramientas de escritura
estén habilitadas:

```sh
HOLOFS_MCP_TOKEN=devtoken ./holofs-web \
  --storage ./holofs-data --addr 127.0.0.1:8787 \
  --log warn --log-format text
```

Inicializar una sesión MCP y listar cada herramienta:

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

Esperado 12 nombres: `diff_objects`, `find_similar`,
`get_cluster_health`, `get_object_health`, `inspect_object`,
`inspect_shard`, `list_catalog`, `mkdir`, `mv_object`,
`put_object_text`, `read_object_text`, `rmdir`.

Puerta de auth:

```sh
# sin cabecera → 401
curl -s -o /dev/null -w 'no-auth: %{http_code}\n' \
  -X POST http://127.0.0.1:8787/mcp -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{
    "protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"c","version":"1"}}}'

# bearer válido → 200
curl -s -o /dev/null -w 'with-auth: %{http_code}\n' \
  -X POST http://127.0.0.1:8787/mcp \
  -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{
    "protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"c","version":"1"}}}'
```

Superficie de recursos:

```sh
curl -s -X POST http://127.0.0.1:8787/mcp \
  -H "Authorization: Bearer $TOKEN" -H "Mcp-Session-Id: $SID" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":3,"method":"resources/list"}' \
  | grep -oE '"uri":"holofs:///[^"]+"' | head -5
```

Para la integración con Claude Code véase [api.md §5](./api.md#5-servidor-mcp).

---

## 17. Operaciones wavelet

Ambas operaciones funcionan sobre el endpoint MCP existente (`/mcp`) —
mantener la misma sesión que en §16.4. Definir `HOLOFS_MCP_TOKEN`
antes de iniciar el clúster para que el formulario `save_as` funcione.

### 17.1 Mezcla wavelet

Construir un PNG híbrido de dos imágenes compatibles, guardarlo al
catálogo como `hybrid.png`:

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

# Traerlo para inspeccionar el híbrido — debería ser un PNG regular.
curl -s -o /tmp/hybrid.png 'http://127.0.0.1:8787/hybrid.png'
file /tmp/hybrid.png
```

`file /tmp/hybrid.png` debería reportar una imagen PNG real con las
dimensiones esperadas.

Errores de compatibilidad — formas incompatibles / k / parámetros por
capa devuelven `BadRequest`:

```sh
# Mezclar imagen con texto → BadRequest de la comprobación de tipo.
curl -s -X POST http://127.0.0.1:8787/mcp \
  -H "Authorization: Bearer $TOKEN" -H "Mcp-Session-Id: $SID" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{
    "name":"wavelet_mix","arguments":{
      "a":"photo.png","b":"check.txt","split":0}}}' \
  | grep -oE '"message":"[^"]*"' | head -1
```

### 17.2 Filtro de capa de audio

Renderizar un objeto de audio con solo los bajos (L0) preservados,
guardar como una nueva entrada de catálogo:

```sh
# Asume algún `track.wav` ingestado anteriormente.
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

`keep_layers:[]` o cada capa descartada → `BadRequest` (el output
sería silencio).

### 17.3 El modo inline "sin copia"

Omitir `save_as` para obtener los bytes de vuelta inline como un blob
base64 — útil cuando quieres que el LLM mire el resultado sin dejar
un artefacto de catálogo atrás:

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

`bytes_len` reporta el tamaño del PNG; `saved_as` debería estar ausente.

---

## 18. Quickstart con el árbol de muestra

`tools/test-data/` envía cuatro piezas que llevan un checkout fresco
directamente a "cada característica ejercitada, cada página poblada"
sin necesidad de fabricar a mano archivos de entrada:

```
tools/test-data/
├── generate-samples.py    # determinista, sin dependencias Python 3.10+
├── clean-cluster.sh       # limpia catálogo + shards + embeddings + versiones
├── upload-samples.sh      # hace PUT del árbol de muestra, preservando jerarquía
└── run-tests.sh           # smoke end-to-end de las etapas 12.6–15.0
```

### 18.1 Generar el árbol

```sh
python3 tools/test-data/generate-samples.py
# → wrote 38 samples (1,862,535 bytes) under <repo>/samples
```

El output vive bajo `./samples/` (gitignored). Todos los bytes son
deterministas — re-ejecutar con los mismos args produce archivos
byte-idénticos, de modo que los tests versionados pueden fijarse
contra los hashes exactos.

Jerarquía:

```
samples/
  photos/{landscapes,abstract,brand-pairs}/*.png
  audio/{music,effects,silence}/*.wav
  docs/{notes,spec,legal}/{*.txt,*.md,*.json}
  binaries/{archives,blobs}/{*.zip,*.tar,*.bin}
```

La carpeta brand-pairs incluye near-duplicates intencionales
(`logo-N.png` + `logo-N-wm.png`) para que la columna robust-copy de
`/similar` produzca hits.

### 18.2 Reinicio limpio

```sh
tools/test-data/clean-cluster.sh
# (FORCE=1 para omitir el prompt de confirmación)

./target/release/holofs-web \
    --storage ./holofs-data \
    --addr 127.0.0.1:8787 \
    --enable-embed \
    --enable-versions &
```

Sin `--enable-embed` la página `/search` renderiza un banner "embed
disabled". Sin `--enable-versions` los PUTs que reemplazan un objeto
existente purgan los shards previos (sin archivo).

### 18.3 Subir el árbol

```sh
tools/test-data/upload-samples.sh
```

El script primero hace mkdir de cada carpeta de prefijo (para que
`/?p=<dir>` funcione directamente), luego hace PUT de cada archivo.
Finalmente consulta `/api/stats` e imprime los totales del nuevo
catálogo — esperar `objects_total = 38 + <directory_markers>` (los
mkdirs del script de subida también se cuentan como entradas de
directorio).

### 18.4 Smoke end-to-end

```sh
tools/test-data/run-tests.sh
```

Por lo que recorre, por etapa:

| Etapa  | Chequeo                                            |
|--------|----------------------------------------------------|
| 9      | `/` + `/?p=<folder>` para cada subdir              |
| 12.6   | `/mix?a=<image>` renderiza el compositor de mezcla wavelet |
| 12.7   | Métricas por archivo `/health/<name>`              |
| 12.7   | Página de marketing `/about`                       |
| 12.8/9 | UI `/search` + `/api/search?band=<any|coarse|mid|full>` |
| 13.0   | `/similar/<brand-pair logo>` incluye columna robust-copy |
| 13.1   | Multipart `/holo/<name>` + `/preview/stream/<name>` |
| 13.2   | `/api/spotlight.png?mode=spatial`                  |
| 14.1   | `/api/spotlight.png?mode=coeff`                    |
| 13.4   | PUT dos veces → `/versions/<name>` muestra fila archivada |
| 14.0/3 | `POST /api/gc` devuelve JSON `GcReport`            |

Cada chequeo imprime `✓` / `✗` y el código de salida del script es
no cero si algún chequeo falla.

---

## 19. Página de métricas por archivo

**Objetivo**: confirmar que el bloque "Métricas únicas" bajo
`/health/<name>` se puebla correctamente.

**Pasos**:

1. Elegir cualquier imagen del árbol de muestra, p. ej.
   `photos/landscapes/mountain.png`.
2. Visitar `http://127.0.0.1:8787/health/photos/landscapes/mountain.png`
   en un navegador, o hacer curl a la API subyacente directamente:

   ```sh
   # POST — el endpoint es una función de servidor leptos, así que el arg name
   # va en el cuerpo del form, no en el query string. Un GET devuelve
   # 405 Method Not Allowed.
   curl -s -X POST -d 'name=photos/landscapes/mountain.png' \
        http://127.0.0.1:8787/api/file_metrics \
        | python3 -m json.tool
   ```

**Payload esperado**: un `FileMetricsView` con:

- `total_shards_in_file` ≈ `unique_shards_in_file` (la deduplicación en
  tiempo de PUT no comprime dentro de la codificación RLNC de un solo
  archivo).
- `catalog_total_shards` ≥ `total_shards_in_file`.
- `originality_pct` en algún lugar en `[0, 100]`; una imagen del árbol
  de muestra sin estructura compartida debería estar cerca de 100.
- `originality_per_layer` es un `Vec<f32>` con `nlayers` entradas.
- `layer_energy` poblado para image / audio; `None` para text / opaque.
- `audio_bands` solo presente cuando `kind == "audio"`.
- `neighbours` está vacío a menos que el catálogo también contenga los
  mismos bytes bajo un nombre diferente.

**Chequeo brand-pair**: contra `photos/brand-pairs/logo-1.png`, el
array `neighbours[]` debería listar `photos/brand-pairs/logo-1-wm.png`
como la entrada **top** (mayor `shared_total`) con un
`shared_per_layer[0]` no cero — es decir, los shards sistemáticos de
capa-0 (LL / gruesa) sobreviven byte-por-byte a pesar de la marca de
agua en la esquina. Otras imágenes del catálogo muestran
`shared_per_layer[0] == 0`. Ese overlap de capa-0 es lo que alimenta
la puntuación 0 de robust-copy. Véase §21 para el caveat de la fórmula
de puntuación en datos de prueba sintéticos.

---

## 20. Búsqueda semántica CLIP + bandas (Etapas 12.8 / 12.9 / 13.3)

**Prerrequisito**: servidor iniciado con `--enable-embed`. En la
primera llamada el gateway descarga ~155 MiB de pesos CLIP desde
HuggingFace a `~/.cache/huggingface/hub`; los reinicios subsecuentes
son instantáneos.

**Indexación en bloque** (solo necesaria una vez tras un reinicio
limpio):

```sh
curl -s -X POST http://127.0.0.1:8787/api/embed_all
# → {"new":<N>,"skipped":<M>}
```

`new` cuenta las entradas de catálogo recién embebidas; `skipped`
cuenta imágenes cuyo `(data_cid, band)` ya estaba en `embeddings.bin`
(mismo contenido subido bajo múltiples rutas).

**Consulta por banda**:

```sh
for band in any coarse mid full; do
  echo "--- band=$band ---"
  curl -s "http://127.0.0.1:8787/api/search?q=mountain&band=$band&limit=3" \
    | python3 -m json.tool
done
```

**Resultados esperados**:

- `band=any` devuelve la banda con mejor puntuación por archivo (dedup
  por nombre).
- `band=coarse` clasifica por silueta / mancha de color — las fotos de
  paisaje con una línea de horizonte deberían burbujear a la cima.
- `band=full` clasifica por textura — los abstractos de ruido /
  bloques de píxeles deberían re-mezclarse.
- `band=mid` se sitúa entre — las imágenes de gradiente deberían
  puntuar bien.

**Superficie de UI**: `/search?q=mountain&band=any` muestra una grid
de tarjetas donde la miniatura gruesa de cada tarjeta hace fundido
cruzado a la resolución completa. La tarjeta lleva una insignia de
banda coloreada (azul = coarse, morado = mid, rosa = full).

---

## 21. Columna robust-copy en `/similar`

**Objetivo**: detectar pares "la estructura coincide, el detalle
difiere" (la firma de marca de agua / re-codificación / retoque
ligero).

**Pasos**:

1. Visitar `/similar/photos/brand-pairs/logo-1.png`.
2. Hacer scroll a la tabla "shard overlaps".

**Esperado**:

- `photos/brand-pairs/logo-1-wm.png` es el **vecino top** (mayor
  `shared shards`) — confirma el mecanismo: la marca de agua
  localizada en la esquina inferior derecha preserva la mayor parte
  de los shards sistemáticos de LL (capa-0), de modo que 39+ de esos
  192 shards de capa-0 hashean idénticamente entre la base y la
  variante con marca de agua. Ninguna imagen no relacionada (mandala,
  gradiente, otra marca) comparte un solo shard de capa-0.
- `low-band %` > 0 (overlap de capa-0).

**Caveat sobre la puntuación** (limitación de los datos de prueba
sintéticos, no un bug en la característica): el numérico
`robust copy?` en el árbol de muestra sembrado es **negativo** para
cada par de marca, y el glifo de aviso de marca de agua +30 nunca se
enciende aquí. La razón es que el gateway hace upscale de las PNGs
de muestra 256×256 a su resolución de trabajo 512×512 antes de
codificar; el upsampling bilineal/bicúbico hace que la banda Haar más
fina (capa 3) sea casi enteramente cero para cada imagen sintética
suave. Los K=16 shards sistemáticos sobre esos ceros hashean al mismo
valor "todo-cero" a través de **cada** imagen del árbol de muestra,
así que cada par obtiene un ~36 % `high-band %` de línea base que
inunda la fórmula de puntuación. En fotografías reales con detalle
rico de alta frecuencia la puntuación cruza +30 limpiamente; en este
conjunto de prueba, tratar la **posición top-rank + overlap no cero
de capa-0** como la señal de éxito, no el número absoluto.

Curl a la función de servidor subyacente vía la página (solo
navegadores):

```sh
curl -s 'http://127.0.0.1:8787/similar/photos/brand-pairs/logo-1.png' \
  | grep -oE 'robust_copy_score":-?[0-9.]+'
```

---

## 22. Holograma en streaming

**Objetivo**: confirmar que `/preview/stream/<name>` devuelve un
cuerpo multipart y la página `/holo/<name>` del lado del navegador
funciona.

**Sonda curl**:

```sh
curl -sI 'http://127.0.0.1:8787/preview/stream/photos/abstract/mandala-a.png'
# Content-Type debería ser: multipart/x-mixed-replace; boundary=hololayer-<date>
```

**Navegador**:

1. Visitar `/holo/photos/abstract/mandala-a.png`.
2. Force-reload (Cmd+Shift+R) para eludir la caché PNG por
   `(name, layer)`.
3. Ver la imagen visiblemente afinarse — primer frame en ~decenas de
   milisegundos, cada frame subsiguiente añade el valor de detalle de
   una capa DWT.

**Caveat**: las visitas subsecuentes golpean la caché y se sienten
instantáneas. El intercambio `<img>` libre de JavaScript se apoya en
`multipart/x-mixed-replace`, que Chrome y Firefox manejan con gracia.

---

## 23. Modos de spotlight holográfico

**Objetivo**: renderizar el mismo ROI de dos formas y comparar
visualmente.

```sh
img=photos/landscapes/mountain.png
for mode in spatial coeff; do
  curl -s -o "/tmp/spot-$mode.png" \
       "http://127.0.0.1:8787/api/spotlight.png?name=$img&x=0.35&y=0.35&w=0.3&h=0.3&mode=$mode"
done
file /tmp/spot-*.png
md5 /tmp/spot-*.png    # esperar hashes distintos
```

**Esperado**: dos PNGs de las mismas dimensiones pero bytes distintos.

- `spatial` mantiene el área fuera del ROI como una reconstrucción L0
  borrosa-pero-visible.
- `coeff` mantiene los píxeles fuera del ROI cerca del negro (el
  mapeo inverso Haar pone en cero cada coeficiente que no toca el
  ROI).

**Cabeceras**:

```sh
curl -sI \
  "http://127.0.0.1:8787/api/spotlight.png?name=$img&x=0.35&y=0.35&w=0.3&h=0.3&mode=coeff" \
  | grep -i 'x-holofs'
```

`x-holofs-roi-px` refleja el ROI de píxel con clamping;
`x-holofs-decode-ms` reporta el trabajo del servidor;
`x-holofs-bytes-downloaded` es informativo (1 lo convertirá en un
número real de ahorro de ancho de banda para `?mode=coeff` en objetos
replicados).

**UI**: `/spotlight?a=<image>` expone el toggle de modo + presets de
ROI + un formulario de coordenadas personalizadas.

---

## 24. Versionado por objeto

**Prerrequisito**: servidor iniciado con `--enable-versions`. Los
PUTs versionados OMITEN la purga usual de shards de modo que el
almacenamiento crece monótonamente mientras el flag está activo.
Ejecutar `/api/gc` (escenario 25) para reclamar.

**Pasos**:

1. Elegir un nombre objetivo, p. ej. `samples/photos/abstract/mandala-a.png`
   que ya hayas subido.
2. Subir una imagen diferente a la misma ruta:

   ```sh
   curl -sf -X PUT \
        --data-binary @samples/photos/abstract/mandala-b.png \
        http://127.0.0.1:8787/photos/abstract/mandala-a.png
   ```

3. Inspeccionar historial:

   ```sh
   open 'http://127.0.0.1:8787/versions/photos/abstract/mandala-a.png'
   ```

   Esperar al menos una fila archivada fechada justo ahora. El prefijo
   del CID debería coincidir con la subida original, no con el
   reemplazo.

4. Hacer click en "restore" en la fila archivada. Confirmar en el
   diálogo.

   ```sh
   # O vía curl:
   curl -X POST \
        -d 'name=photos/abstract/mandala-a.png&id=v<TS>_<CIDSHORT>' \
        http://127.0.0.1:8787/api/restore
   ```

5. Re-fetch de la imagen:

   ```sh
   md5 <(curl -sf http://127.0.0.1:8787/photos/abstract/mandala-a.png)
   ```

**Esperado**: el MD5 post-restore coincide con el MD5 pre-reemplazo;
el reemplazo está ahora él mismo archivado (restore es reversible).

---

## 25. GC de shard huérfano + GC de embedding (Etapas 14.0 / 14.3 / 14.4)

**Objetivo**: confirmar que el gateway reclama shards no referenciados
por ningún manifiesto vivo o archivo de versión, Y limpia embeddings
obsoletos de `embeddings.bin`.

**Pasos**:

1. Disparar un pase de PUT-reemplazo (escenario 24) para que el
   clúster tenga shards huérfano-ables.
2. Eliminar los archivos laterales de versión para ese nombre
   (simula al operador eliminando el historial):

   ```sh
   rm -rf holofs-data/versions/photos__abstract__mandala-a.png
   ```

   (El script `clean-cluster.sh` hace lo mismo en masa.)

3. Ejecutar GC:

   ```sh
   curl -s -X POST http://127.0.0.1:8787/api/gc | python3 -m json.tool
   ```

**Esperado**:

- `purged_total` > 0 (los shards previos están ahora sin referenciar).
- `embeddings_dropped` > 0 si algún CID obsoleto vivía en el índice.
- `embeddings_kept` coincide con el número de registros vivos
  `(data_cid, band)` restantes.
- `ok: true` en cada nodo, ningún campo `error` definido.
- `duration_ms` típicamente < 100 ms en el clúster de dev.

**Comprobación de concurrencia** (opcional): ejecutar un PUT largo +
un GC en paralelo y verificar que ambos tengan éxito. La barrera
RwLock en `Gateway` debería serializarlos — GC esperará a que el PUT
termine, luego se ejecutará solo.

```sh
( curl -sf -X PUT --data-binary @samples/photos/landscapes/ocean.png \
       http://127.0.0.1:8787/race-test.png ) &
sleep 0.2
( curl -sf -X POST http://127.0.0.1:8787/api/gc | python3 -m json.tool ) &
wait
# Ambos deberían completar; el `duration_ms` de GC incluirá el tiempo de espera.
```

---

## 26. Escenarios de fiabilidad .x

### 26.1 Contadores de auto-reparación al leer

Objetivo: verificar que el brazo de reintento de
`decode_with_autorepair` mueve los contadores en `/api/stats` solo
cuando hay algo que reparar.

```sh
# Baseline — clúster fresco, sano.
curl -s http://127.0.0.1:8787/api/stats | jq '.auto_repairs_total, .auto_repair_failures_total'
# 0
# 0

# Degradación ligera — matar 3 de 40 nodos (muy por debajo de la redundancia de capa-3).
for i in 0 1 2; do curl -X POST -d "i=$i" http://127.0.0.1:8787/admin/node; done
curl -s http://127.0.0.1:8787/photo.png -o /dev/null
curl -s http://127.0.0.1:8787/api/stats | jq '.auto_repairs_total'
# Aún 0 — la auto-reparación NO debe dispararse bajo pérdida ligera.

# Degradación fuerte — matar el 60 % del clúster.
for i in $(seq 3 24); do curl -X POST -d "i=$i" http://127.0.0.1:8787/admin/node; done
curl -s http://127.0.0.1:8787/photo.png -o /dev/null
curl -s http://127.0.0.1:8787/api/stats | jq '.auto_repairs_total, .auto_repair_failures_total'
# Al menos un contador DEBE ser ≥ 1.
```

Cubierto automáticamente por
`crates/holofs-e2e/tests/auto_repair_e2e.rs`.

### 26.2 El scrub en segundo plano repara proactivamente

Objetivo: probar que el scrub captura la deriva de placement antes que
los usuarios.

```sh
# Poner scrub a 15 s para la demo (por defecto es 600 s).
HOLOFS_SCRUB_INTERVAL=15 \
  cargo run --release --bin holofs-web
# esperar al primer tick:
sleep 20
curl -s http://127.0.0.1:8787/api/stats | jq '.scrub_runs_total'
# 1+ — scrub_repairs_total permanece 0 en un clúster sano.
```

Una demo más ruidosa está en
`crates/holofs-e2e/tests/reliability_repair.rs::prometheus_metrics_expose_auto_repair_gauges`.

### 26.3 Clúster degradado → 503, no panic

Objetivo: `place_shard` solía aseverar sobre un conjunto vivo vacío,
crasheando el gateway. Ahora PUT contra un clúster totalmente caído
devuelve un 503 limpio.

```sh
# Matar cada nodo.
for i in $(seq 0 39); do curl -X POST -d "i=$i" http://127.0.0.1:8787/admin/node; done
curl -i -X PUT --data-binary @some.png http://127.0.0.1:8787/test.png
# HTTP/1.1 503 Service Unavailable
# content-type: text/plain
# cluster has no live nodes
```

Tras deshacer el kill de los nodos (`POST /admin/node` hace toggle),
el mismo PUT tiene éxito con 2xx.

Cubierto por `crates/holofs-e2e/tests/cluster_degraded.rs`.

### 26.4 Eliminación de versión + tope de retención

Objetivo: el historial por nombre no crece sin límite.

```sh
HOLOFS_VERSIONS_KEEP_LAST=2 \
  cargo run --release --bin holofs-web -- --enable-versions

# PUT de cuatro imágenes diferentes bajo el mismo nombre.
for body in a.png b.png c.png d.png; do
  curl -X PUT --data-binary @$body http://127.0.0.1:8787/test.png
done

# /api/versions_list — como máximo 2 archivos, sin importar cuántos PUTs aterrizaron.
curl -s -X POST -d "name=test.png" http://127.0.0.1:8787/api/versions_list | jq '.versions | length'
# 2

# Eliminación manual de un archivo — el contador cae a 1.
ID=$(curl -s -X POST -d "name=test.png" http://127.0.0.1:8787/api/versions_list | jq -r '.versions[0].id')
curl -X POST -d "name=test.png&id=$ID&return_to=/" http://127.0.0.1:8787/api/versions/delete
```

Cubierto por `crates/holofs-e2e/tests/versions_lifecycle.rs`.

### 26.5 cd-en-carpeta en el árbol del catálogo

Objetivo: hacer click en "open →" en una carpeta muestra SOLO el
contenido de esa carpeta en el nivel superior, con un breadcrumb para
navegar hacia arriba.

```sh
# Sembrar un árbol anidado (el script de subida de muestra estándar):
tools/test-data/upload-samples.sh

# Visitar el catálogo en /. Expandir `photos/`, luego hacer click en "open →" en
# `landscapes-xl`. La URL se convierte en `/?p=photos/landscapes-xl` y el
# árbol ahora muestra los seis JPEGs picsum como entradas de nivel superior — sin
# carpetas hermanas.
xdg-open http://127.0.0.1:8787/?p=photos/landscapes-xl  # linux
open http://127.0.0.1:8787/?p=photos/landscapes-xl      # macos
```

Los formularios inline de upload + mkdir en cada fila `<details>`
aterrizan archivos en la carpeta que estabas mirando; el formulario de
upload de la barra raíz se auto-limita al prefijo `?p=<path>` actual.

Cubierto por el smoke manual en §18 más los tests de renderizado de
catálogo bajo `crates/holofs-e2e/tests/ui_catalog.rs`.

### 26.6 Muestras PNG sintéticas decodifican limpiamente

Objetivo: el bug 22-de-29-rotos ha desaparecido.

```sh
tools/test-data/clean-cluster.sh           # storage fresco
HOLOFS_NO_SEED=true \
  cargo run --release --bin holofs-web &
sleep 4
python3 tools/test-data/generate-samples.py
tools/test-data/upload-samples.sh

# Recorrer cada PNG / JPG bajo samples/ y hacer GET.
broken=0
for f in $(find samples -type f \( -name '*.png' -o -name '*.jpg' \)); do
  code=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:8787/${f#samples/}")
  [ "$code" != "200" ] && broken=$((broken+1))
done
echo "broken=$broken"
# broken=0
```

El jitter LSB determinista inyectado por `write_png` (x) asegura que
los shards DWT de alta frecuencia sean únicos por archivo incluso en
los generadores sintéticos más suaves.

---

## Cierre

Parada limpia:

```sh
pkill -f 'target/release/holofs-web'
# o Ctrl-C en la terminal ejecutando el clúster
```

Limpieza total — eliminar todo el estado:

```sh
tools/test-data/clean-cluster.sh
# o, manualmente:
rm -rf ./holofs-data ./.cluster-data
```

Si algo se comporta mal, comparar contra las descripciones de arriba
y consultar:

- [docs/es/operations.md](./operations.md) — configuración y operaciones
- [docs/es/architecture.md](./architecture.md) — flujo de datos PUT → GET
- [docs/es/api.md](./api.md) — API HTTP, formato del protocolo de cable, nuevos endpoints
- [docs/es/threat-model.md](./threat-model.md) — amenazas cubiertas
- `tools/test-data/README.md` — uso del árbol de muestra + runner de smoke
