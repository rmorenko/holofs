# Guía de operaciones

Esta guía describe cómo **desplegar**, **monitorizar**, **respaldar**,
**recuperar** y **planificar la capacidad** de un clúster holofs en
producción.

## Contenido

1. [Topologías de despliegue](#1-topologías-de-despliegue)
2. [Instalación bare-metal](#2-instalación-bare-metal)
3. [Docker / Compose](#3-docker--compose)
4. [Kubernetes vía Helm](#4-kubernetes-vía-helm)
5. [Referencia de configuración](#5-referencia-de-configuración)
6. [Monitorización y alertas](#6-monitorización-y-alertas)
7. [Planificación de capacidad](#7-planificación-de-capacidad)
8. [Backup y restauración](#8-backup-y-restauración)
9. [Recuperación ante desastres](#9-recuperación-ante-desastres)
10. [Procedimientos día-2](#10-procedimientos-día-2)

---

## 1. Topologías de despliegue

| Topología        | Caso de uso                                     | Pros                            | Contras                               |
|------------------|-------------------------------------------------|---------------------------------|---------------------------------------|
| Embebida         | Dev, demo, evaluación en un único host          | Un binario, sin orquestación    | Sin tolerancia a fallos a nivel de máquina |
| Multi-proceso    | Host único, fronteras de proceso aisladas       | Reiniciar nodos independientemente | Sigue siendo un punto único de fallo (host) |
| Multi-host       | Producción: 40 nodos entre 5 zonas × 8 hosts    | Durabilidad real, failover de zona | Requiere red, monitorización, ops    |
| Kubernetes       | Nube / on-prem con k8s                          | Basado en Helm, declarativo     | Los stateful sets son más difíciles que los stateless |

**Objetivo recomendado para producción:** ≥ 5 zonas × ≥ 4 hosts × 1–2
nodos por host. Esto sobrevive a **cualquier caída de una zona completa**
más fallos simultáneos de nodos individuales en las zonas restantes
(véase [theory.md §4](./theory.md#4-capas-de-prioridad-y-degradación-holográfica)).

---

## 2. Instalación bare-metal

### 2.1. Prerrequisitos

- Linux (kernel ≥ 5.10), macOS, o Windows Server.
- 2 GB de RAM y 10 GB de disco por nodo mínimo; 8 GB / 100 GB recomendado.
- Puertos TCP abiertos: gateway (`8787`) y puertos de nodos (9100–9139 por defecto).
- Una cuenta de usuario (p. ej. `holofs`) con acceso de escritura al directorio de datos.

### 2.2. Compilar desde el código fuente

```sh
# MSRV fijado: 1.75
rustup install 1.81.0
cargo build --release --workspace
```

Binarios producidos bajo `target/release/`:

| Binario              | Propósito                                     |
|----------------------|-----------------------------------------------|
| `holofs-web`         | Gateway HTTP + clúster embebido (axum + Leptos SSR) |
| `holofs-node`        | Demonio de nodo único (`ADDR --storage DIR`) |
| `holofs-admin`       | Whitelist keygen + firma                     |
| `holofs-cluster`     | Harness de desarrollo local: N nodos in-process + gateway |
| `holofs-fs`          | Playground de filesystem local               |
| `holofs-inspect`     | Inspección de manifiestos / shards           |
| `holofs-bench`       | Benchmarks                                   |
| `holofs-soak`        | Driver de operaciones aleatorias de larga duración contra un gateway vivo |
| `holofs-soak-report` | Renderiza informe HTML + Markdown desde un directorio de run soak |
| `holofs`             | CLI legacy monocomando                       |

### 2.3. Whitelist (requerido en producción)

```sh
# 1. Generar un par de claves admin (offline; sólo la pubkey se
#    distribuye).
holofs-admin gen-key admin.key
holofs-admin pubkey admin.key   # imprime ADMIN_PUBKEY_HEX

# 2. Arrancar cada nodo una vez para que materialice su propio
#    identity.key e imprima su pubkey — recoger estas cadenas hex.
holofs-node 10.0.1.10:9100 --storage /var/lib/holofs/node00
# → holofs-node addr=10.0.1.10:9100 pubkey=NODE0_PUBKEY_HEX

# 3. Firmar la whitelist. Cada --node es ADDR=PUBKEY_HEX:ZONE.
holofs-admin sign-whitelist \
    --admin admin.key \
    --node 10.0.1.10:9100=NODE0_PUBKEY_HEX:0 \
    --node 10.0.1.11:9100=NODE1_PUBKEY_HEX:0 \
    --node 10.0.2.10:9100=NODE2_PUBKEY_HEX:1 \
    --out whitelist.holofs

# 4. Distribuir whitelist.holofs a cada nodo + gateway. Verificar:
holofs-admin verify-whitelist whitelist.holofs --admin-pubkey ADMIN_PUBKEY_HEX
holofs-admin show-whitelist   whitelist.holofs
```

Formato de cable: `HOLOFSW1` (véase [api.md §3.4](./api.md#34-whitelist-holofsw1)).

### 2.4. TLS para el protocolo de cable (`--tls`, `--mtls`)

El protocolo binario gateway↔nodo puede cifrarse con rustls. Dos flags
opt-in controlan el comportamiento:

| Flag       | Efecto |
|------------|--------|
| `--tls`    | Cifra las tramas de cable. El cert del servidor es verificado por el cliente. |
| `--mtls`   | Implica `--tls`. El servidor además requiere + verifica un cert de cliente. |

**Modo embebido (sin `--whitelist`):** el binario genera una CA
autofirmada + certs de hoja al arranque. Útil para dev, demos,
clústeres de un solo host. La CA vive solo en RAM y se regenera en
cada reinicio — los clientes que cachean certs verán emisores frescos
en cada arranque.

**Modo distribuido (`--whitelist`):** suministra PEMs pre-emitidos en
la línea de comandos. Genéralos con `openssl` o tu PKI existente:

```sh
# Emitir una CA + un cert por host (script omitido — usa tu PKI).
holofs-web \
  --whitelist cluster.wl \
  --admin-pubkey "$(holofs-admin pubkey admin.key)" \
  --tls --mtls \
  --tls-ca-cert /etc/holofs/ca.crt \
  --tls-cert   /etc/holofs/gateway.crt \
  --tls-key    /etc/holofs/gateway.key
```

El comando de nodo correspondiente recoge su propia hoja — véase la
unidad systemd en §2.5 para la forma de env-var.

Los archivos de cert deben satisfacer:
- Los SANs del cert de hoja deben cubrir cada host `addr:port` al que
  el gateway se conectará (nombre DNS o literal IP).
- El cert CA es la raíz de confianza en ambos lados — mismo archivo en
  cada nodo y en cada gateway.
- Bajo `--mtls` ambos lados presentan el mismo tipo de hoja firmada por
  esa CA. Añade un cert "gateway" separado si quieres valores CN
  distintos.

### 2.5. Servicio systemd

`/etc/systemd/system/holofs-node@.service`:

```ini
[Unit]
Description=holofs node %i
After=network.target

[Service]
Type=simple
User=holofs
Group=holofs
Environment=HOLOFS_STORAGE_DIR=/var/lib/holofs/node%i
# habilita TLS sobre el protocolo de cable. Elimina las siguientes cuatro
# líneas para clústeres de TCP plano; define HOLOFS_MTLS=1 para autenticación mutua.
Environment=HOLOFS_TLS=1
Environment=HOLOFS_TLS_CA_CERT=/etc/holofs/ca.crt
Environment=HOLOFS_TLS_CERT=/etc/holofs/node%i.crt
Environment=HOLOFS_TLS_KEY=/etc/holofs/node%i.key
ExecStart=/usr/local/bin/holofs-node
Restart=on-failure
RestartSec=5s
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
```

Luego `systemctl enable --now holofs-node@00 holofs-node@01 …`.

---

## 3. Docker / Compose

### 3.1. Descargar imagen

```sh
docker pull ghcr.io/holofs/holofs:1.0.0
```

El Dockerfile es multi-etapa: rust:1.81-slim-bookworm → debian:bookworm-slim.
La imagen de runtime se ejecuta como **uid 10001 no-root**, con `tini`
como PID 1.

### 3.2. Clúster de host único (embebido)

```sh
docker run -d \
  --name holofs \
  -p 8787:8787 \
  -v /srv/holofs:/data \
  ghcr.io/holofs/holofs:1.0.0
```

### 3.3. Multi-proceso vía Compose

```yaml
services:
  holofs:
    image: ghcr.io/holofs/holofs:1.0.0
    volumes: ["/srv/holofs:/data"]
    environment:
      HOLOFS_LOG_FORMAT: json
      HOLOFS_ENABLE_EMBED: "1"
      HOLOFS_ENABLE_VERSIONS: "1"
    ports: ["8787:8787"]
```

---

## 4. Kubernetes vía Helm

El chart Helm vive en `deploy/helm/holofs/`.

```sh
helm install holofs ./deploy/helm/holofs \
  --set nodeCount=40 \
  --set persistence.size=50Gi \
  --set ingress.enabled=true \
  --set ingress.hosts[0].host=holofs.example.com
```

**Recursos clave** (véase `deploy/helm/holofs/templates/`):

- `StatefulSet` para nodos — IDs de red estables, PVC por réplica.
- `Service` (`ClusterIP`) para el gateway.
- `Ingress` (opcional) para HTTPS externo.

**Conciencia de zona:** `values.yaml` expone `nodeAffinity` y
`topologySpreadConstraints`. Mapea tu etiqueta de zona de k8s (p. ej.
`topology.kubernetes.io/zone`) a zonas de holofs vía
`HOLOFS_ZONE_FROM_LABEL=topology.kubernetes.io/zone` (auto-derivado
del `Downward API`).

**Probes:**

```yaml
livenessProbe:  { httpGet: { path: /,        port: http }, periodSeconds: 30 }
readinessProbe: { httpGet: { path: /health,  port: http }, periodSeconds: 10 }
```

**Contexto de seguridad:** se ejecuta como `uid 10001`,
`readOnlyRootFilesystem: true`, `capabilities.drop: [ALL]`.

---

## 5. Referencia de configuración

Toda la configuración es vía variables de entorno (los flags CLI
también se aceptan; los flags ganan).

### 5.1. Gateway (`holofs-web`)

| Variable                    | Por defecto               | Descripción                                  |
|-----------------------------|---------------------------|----------------------------------------------|
| `HOLOFS_STORAGE_DIR`        | `./holofs-data`           | Raíz de almacenamiento para shards, catálogo, manifiestos. |
| `HOLOFS_CATALOG`            | `<storage>/catalog.bin`   | Sobrescribir la ruta del archivo de catálogo. |
| `HOLOFS_CONFIG`             | (unset)                   | Ruta a un archivo TOML de configuración (§5.7). |
| `HOLOFS_LOG`                | `info,holofs_web=debug`   | Spec de filtro `tracing`.                    |
| `HOLOFS_LOG_FORMAT`         | `text`                    | `text` \| `json` (producción: `json`).       |
| `LEPTOS_SITE_ADDR`          | `127.0.0.1:8787`          | Dirección HTTP de escucha (`--addr`).        |
| `HOLOFS_METRICS_LISTEN`     | (unset)                   | Dirección de escucha Prometheus separada opcional. |
| `HOLOFS_SEED_PHOTO`         | (unset)                   | Ruta a un PNG que se siembra como `photo.png` en el primer arranque. |
| `HOLOFS_NO_SEED`            | `false`                   | Omitir el seed de demo de dos PNG en un catálogo vacío. |

Cada variable de esta tabla tiene un flag CLI equivalente
(`--storage`, `--log`, `--addr`, etc.) — `holofs-web --help` es la
lista canónica. Los flags tienen precedencia sobre las env vars.

### 5.2. `holofs-node` standalone

El daemon de nodo standalone solo acepta argumentos posicionales y
**no** lee ninguna env var `HOLOFS_*` — es deliberadamente mínimo
para que el mismo binario funcione bajo systemd, docker o
invocación manual.

```text
holofs-node [ADDR] [--storage DIR]
```

`ADDR` por defecto `127.0.0.1:5000`. `--storage DIR` activa la
identidad persistente + shards en disco; sin él el nodo corre en
memoria y regenera su pubkey en cada arranque (solo dev/demo).

### 5.3. Gateway en modo distribuido (whitelist + TLS)

| Variable                    | Por defecto    | Descripción                                  |
|-----------------------------|----------------|----------------------------------------------|
| `HOLOFS_WHITELIST`          | —              | Ruta a una whitelist firmada (§2.3). Cambia el binario a modo distribuido. |
| `HOLOFS_ADMIN_PUBKEY`       | —              | Hex de 64 caracteres de la pubkey admin que firmó la whitelist. |
| `HOLOFS_TLS`                | (off)          | Cifra el protocolo de cable (gateway↔nodos) con rustls. El modo embebido auto-genera una CA autofirmada. |
| `HOLOFS_MTLS`               | (off)          | Implica `HOLOFS_TLS=1`. El servidor también requiere + verifica un cert de cliente. |
| `HOLOFS_TLS_CERT`           | —              | Modo distribuido: ruta al cert de hoja PEM.  |
| `HOLOFS_TLS_KEY`            | —              | Modo distribuido: ruta a la clave PEM correspondiente. |
| `HOLOFS_TLS_CA_CERT`        | —              | Modo distribuido: ruta a la raíz de confianza CA PEM. |

### 5.4. Clúster embebido

Los tamaños de la topología embebida (`holofs-web` sin
`--whitelist`) son constantes de compilación: `N_NODES = 40`,
`NLAYERS = 4`, `K = 16`, `LEVELS = 3`. Sólo el puerto base y el
comportamiento del seed son ajustables en runtime.

| Variable                    | Por defecto | Descripción                                     |
|-----------------------------|-------------|-------------------------------------------------|
| `HOLOFS_EMBED_BASE_PORT`    | `9100`      | Puerto base estable para los nodos en proceso; cada nodo enlaza `base + idx`. Ponlo para evitar el churn de puertos efímeros. |
| `HOLOFS_NO_SEED`            | `false`     | Omitir el seed de demo de dos PNG en un catálogo vacío. Poner a `true` cuando se re-suba desde un árbol de muestra conocido para que el seed no colisione con tus datos. |
| `HOLOFS_W` / `HOLOFS_H`     | `512`       | Dimensiones del frame (ambos deben ser un múltiplo positivo de `2^LEVELS = 8`). |

### 5.5. Fiabilidad

| Variable                    | Por defecto | Descripción                                              |
|-----------------------------|-------------|----------------------------------------------------------|
| `HOLOFS_RPC_TIMEOUT_MS`     | `8000`      | Presupuesto general por RPC (`tokio::time::timeout`). `0` desactiva el tope; el timeout TCP a nivel de SO (60-75 s) es entonces la única parada. |
| `HOLOFS_SCRUB_INTERVAL`     | `600`       | Período del scrub de shards en segundo plano (segundos). `0` desactiva. El scrub recorre el catálogo, hace diff de `list_node_hashes` vs `place_shard`, repara las discrepancias antes de que los usuarios las encuentren. |
| `HOLOFS_VERSIONS_KEEP_LAST` | `0`         | Tope de historial de versiones por nombre. Descarta los archivos más antiguos en cada PUT. `0` = ilimitado (el `/api/versions/delete` manual es entonces el único camino para recuperar shards). Requiere `--enable-versions`. |
| `HOLOFS_POOL_PER_NODE`      | `8`         | Máximo de conexiones de cable en pool inactivas por dir de nodo. |
| `HOLOFS_POOL_IDLE_SECS`     | `60`        | Descartar entradas de pool inactivas más tiempo que este en `acquire`. |
| `HOLOFS_POOL_DISABLE`       | `false`     | Bypass del pool de keepalive — cada RPC marca fresco. Útil cuando se persiguen bugs de nivel de cable. |

### 5.5.c. Rate limit por IP

Complementa los topes globales de backpressure: los topes evitan que
el proceso explote bajo cualquier ráfaga — esta capa evita que un
único cliente mal-comportado hambre a todos los demás llamadores.
Ambas se aplican a los buckets MEDIUM (decode / PUT / dir ops) y LONG
(search / spotlight / GC); SHORT y los endpoints de streaming
permanecen ilimitados.

| Variable                        | Por defecto | Descripción                                              |
|---------------------------------|-------------|----------------------------------------------------------|
| `HOLOFS_RATE_LIMIT_RPS_PER_IP`  | `0`         | Tasa de recarga del token bucket por IP de cliente. Cero desactiva la capa por completo. |
| `HOLOFS_RATE_LIMIT_BURST`       | `2 × rps`   | Máximo de tokens que un bucket contiene. Con bucket vacío la petición responde 429 con `Retry-After: 1`. |
| `HOLOFS_RATE_LIMIT_IDLE_SECS`   | `300`       | Umbral de expulsión por inactividad para el mapa por IP (memoria acotada bajo poblaciones de cliente de alta rotación). |

**Fuente de IP del cliente.** Detrás de un reverse proxy el middleware
lee el primer salto de `X-Forwarded-For`. Las conexiones directas
usan `ConnectInfo<SocketAddr>` de
`into_make_service_with_connect_info`. Ninguna presente → bucket
compartido `0.0.0.0` para que los hosts ruidosos no obtengan un pase
libre por conexión.

**Métrica.** `holofs_rate_limit_rejected_total` cuenta cada respuesta
429. Una tasa sostenida no cero sugiere o bien un cliente abusivo
(investigar) o un tope infraaprovisionado (subir
`rate_limit_rps_per_ip`).

### 5.5.b. PUT en streaming

| Variable                    | Por defecto | Descripción                                              |
|-----------------------------|-------------|----------------------------------------------------------|
| `HOLOFS_UPLOAD_MAX_SIZE`    | `1 GiB`     | Tope del cuerpo por petición para `PUT /*path`. El cuerpo se transmite directamente a `<storage>/uploads/upload-<pid>-<counter>.tmp` (RAM constante independientemente de la velocidad del cliente / tamaño del cuerpo) y se lee de vuelta en un `Vec<u8>` justo antes de `Gateway::ingest_bytes`. Los cuerpos que exceden el tope devuelven 413 Payload Too Large; el tempfile se elimina en cada camino de salida. |

El streaming mantiene el delta de RSS del gateway acotado por el
buffer de copia (~64 KiB) en lugar de la tasa de subida del cliente —
un cliente lento en una subida de 200 MiB ya no fija 200 MiB de
memoria del gateway durante toda la duración. RSS aún sube al tamaño
del cuerpo brevemente en tiempo de ingesta porque el codec RLNC / DWT
espera `&[u8]`; una ingesta totalmente streaming está fuera del
alcance hasta que el codec la soporte.

### 5.6. Capa de fiabilidad

Cada perilla debajo tiene un valor por defecto seguro; el gateway
arranca con éxito sin ninguna de ellas definida.

| Variable                              | Por defecto | Descripción                                              |
|---------------------------------------|-------------|----------------------------------------------------------|
| `HOLOFS_MEDIUM_CONCURRENCY`           | `64`        | Permisos para el bucket de ruta MEDIUM (decodes, PUT, dir ops). En saturación el middleware del handler devuelve 503 con un cuerpo de diagnóstico en lugar de acumular tareas axum. Sintonizar contra `holofs_backpressure_permits_available{bucket="medium"}`. |
| `HOLOFS_LONG_CONCURRENCY`             | `8`         | Permisos para el bucket LONG (búsqueda semántica, spotlight, `/api/gc`, `/api/embed_all`, escaneos de fingerprint). |
| `HOLOFS_REPUTATION_PERSIST_INTERVAL`  | `30`        | Con qué frecuencia se hace snapshot del estado compartido de `Reputation` a `<storage>/reputation.bin`. El bootstrap lo carga de vuelta en el siguiente arranque; una discrepancia de `n_nodes` o archivo corrupto silenciosamente recae en una tabla fresca. También se escribe una instantánea final en SIGTERM. |
| `HOLOFS_ADMIN_TOKEN`                  | _(unset)_   | Cuando está definido, `POST /admin/node` y `POST /api/gc` requieren `Authorization: Bearer <token>`. Faltante/incorrecto → 401. |
| `HOLOFS_ADMIN_UNAUTHENTICATED`        | _(unset)_   | Override de dev: poner a `1` para dejar la superficie admin abierta cuando `HOLOFS_ADMIN_TOKEN` no está definido. Registra un WARN al arranque. Si ninguna variable está definida la superficie admin está deshabilitada (403). |

Los timeouts están hardcodeados por bucket por diseño (SHORT 10 s,
MEDIUM 60 s, LONG 300 s); los endpoints de streaming (SSE,
multipart/x-mixed-replace) + `/mcp` están intencionadamente sin
presupuesto. Los handlers vencidos emergen como `504 Gateway Timeout`
e incrementan `holofs_handler_timeouts_total{bucket=…}`.

### 5.7. Características opcionales

| Variable                    | Por defecto | Descripción                                              |
|-----------------------------|-------------|----------------------------------------------------------|
| `HOLOFS_ENABLE_VERSIONS`    | `false`     | Espejo de `--enable-versions`. Archiva cada PUT-reemplazo como archivo lateral bajo `<storage>/versions/<sanitized>/v…bin`. |
| `HOLOFS_ENABLE_EMBED`       | `false`     | Espejo de `--enable-embed`. Carga el modelo CLIP-multilingual en el primer PUT o primer `/api/search`, luego mantiene `embeddings.bin`. |
| `HOLOFS_ASYNC_ENCODE`       | `false`     | Cambia el camino PUT RLNC por defecto de sync a async. El handler retorna `202 Accepted` en cuanto se commitea el manifest placeholder; encode + fan-out de shards corren en un tokio-task detached. Los handlers de lectura gatean por `ManifestState` — véase §10.7 para mediciones de throughput y cuándo es apropiado. |
| `HOLOFS_MCP_TOKEN`          | —           | Cuando está definido, el endpoint `/mcp` requiere `Authorization: Bearer <token>` Y activa las herramientas de escritura. Sin la variable el endpoint permanece abierto + solo-lectura. |

### 5.8. Archivo de configuración TOML

Cada env var de arriba (`HOLOFS_*` y `LEPTOS_SITE_ADDR`) también es
configurable a través de un único archivo de configuración TOML pasado
vía `--config /path/to/holofs.toml` o la env var `HOLOFS_CONFIG`.
Un archivo de referencia comentado vive en
[`deploy/holofs.example.toml`](../../deploy/holofs.example.toml).

Escalera de prioridad (la más alta gana):

1. Flag CLI (`--medium-concurrency 128`)
2. Env var (`HOLOFS_MEDIUM_CONCURRENCY=128`)
3. Valor del archivo TOML (`[reliability] medium_concurrency = 128`)
4. Valor por defecto en tiempo de compilación

**Ejemplo**:

```toml
[server]
addr = "0.0.0.0:8787"
storage = "/var/lib/holofs"
log_format = "json"

[tls]
enabled = true
mtls = true
cert = "/etc/holofs/node.crt"
key = "/etc/holofs/node.key"
ca_cert = "/etc/holofs/ca.crt"

[reliability]
medium_concurrency = 128
long_concurrency = 16
scrub_interval_secs = 300

[admin]
# Token inline O referenciar un archivo (recomendado para secretos).
token_file = "/etc/holofs/admin.token"
```

**Secretos.** `[admin] token` y `[mcp] token` aceptan o bien una
cadena inline o una ruta `token_file` que apunta a un archivo cuya
primera línea no vacía es el token. Para producción, prefiere
`token_file` con modo `0400` y propiedad root para que el token no
sea visible en el historial git del archivo de configuración /
chart Helm empaquetado.

**Campos desconocidos**. TOML usa `deny_unknown_fields` en tiempo de
parseo — un typo en `medium_concurency` (sin la 'r') falla ruidosamente
al arranque con el nombre exacto de la clave en el error. Esto es
intencional; un fallback silencioso derrotaría el propósito del
archivo.

### 5.9. Cifrado de shards en reposo

Habilitar con `HOLOFS_AT_REST_ENC=1` (o `[security]
at_rest_encryption = true` en el TOML). Cuando está activo, cada
archivo de shard escrito a disco se sella con AES-256-GCM. La cabecera
permanece en texto plano (para que `Store::open` pueda seguir
indexando sin la clave), pero los coeficientes + payload del chunk
codificado son cifrado.

**Gestión de claves.** La clave AES de 32 bytes se deriva al arranque
del seed de identidad del nodo vía HKDF-SHA256
(`salt = "holofs-shard-salt-v1"`, `info = "holofs-shard-key-v1"`).
Ningún nuevo secreto que rotar — perder `identity.key` ya pierde la
identidad del nodo. La clave permanece en RAM durante la vida del
proceso; root en un nodo en ejecución puede leer texto plano a través
de una ruta de auditoría legítima.

**Formato de cable.** Dos magics de shard coexisten:

| Magic       | Significado                                                 |
|-------------|-------------------------------------------------------------|
| `HOLOFSS1`  | Texto plano. Leído por cada versión.                        |
| `HOLOFSS2`  | Sellado. `[8 B magic][18 B header][12 B nonce][ct+tag]`.    |

La cabecera de 18 bytes es AAD para el tag GCM, de modo que cualquier
reescritura post-hoc de cabecera (object_id, channel, layer, lengths)
invalida el shard al descifrar. Las lecturas olfatean los primeros 8
bytes y despachan — se soportan directorios mixtos v1 + v2 de modo
que habilitar sobre un store existente sella solo escrituras
*nuevas*. Un pase completo de re-cifrado está fuera del alcance; la
migración recomendada es generar un nodo fresco con una identidad
fresca y dejar que el pase de auto-reparación rebalancee shards sobre
él.

**Modelo de amenazas.** En el alcance: un adversario hace snapshot de
los archivos de shard de un nodo apagado (fuga de backup, disco
decomisado, la reconstrucción RAID dejó el disco viejo legible).
Fuera del alcance: root en un nodo en ejecución — una vez que la
clave derivada está en RAM, `read_shard_file` produce texto plano
para auditorías legítimas.

---

## 6. Monitorización y alertas

### 6.1. Endpoint de métricas

El gateway expone `GET /metrics` en formato de exposición de texto
Prometheus (`text/plain; version=0.0.4`). Gauges basados en pull
obtenidos de `Gateway::api_stats` + snapshot de admin-kill más
contadores de fiabilidad.

| Métrica                                     | Tipo    | Etiquetas                    | Significado |
|---------------------------------------------|---------|------------------------------|-------------|
| `holofs_nodes_total`                        | gauge   | —                            | nodos en topología |
| `holofs_nodes_live`                         | gauge   | —                            | nodos no deshabilitados por admin |
| `holofs_objects_total`                      | gauge   | `kind` (image/audio/text/opaque/directory) | tamaño de catálogo por tipo |
| `holofs_shards_total`                       | gauge   | —                            | shards planificados en todo el catálogo |
| `holofs_shards_unique`                      | gauge   | —                            | hashes de shard distintos |
| `holofs_dedup_savings_pct`                  | gauge   | —                            | `(1 − unique/total) × 100` |
| `holofs_bytes_total`                        | gauge   | —                            | bytes almacenados aproximados |
| `holofs_node_admin_killed`                  | gauge   | `node`, `addr`, `zone`       | flag de admin-kill por nodo |
| `holofs_auto_repairs_total`                 | counter | —                            | GETs que dispararon el brazo de reintento de `decode_with_autorepair` |
| `holofs_auto_repair_failures_total`         | counter | —                            | pases de auto-reparación que fallaron ellos mismos |
| `holofs_scrub_runs_total`                   | counter | —                            | ticks del scrub en segundo plano completados (`HOLOFS_SCRUB_INTERVAL`) |
| `holofs_scrub_repairs_total`                | counter | —                            | objetos que el scrub reparó *antes* de que cualquier usuario los encontrara |
| `holofs_catalog_persist_failures_total`     | counter | —                            | Errores de guardado atómico del catálogo en disco. No-cero = el estado en disco está detrás de la memoria; el próximo reinicio pierde escrituras. Alertar inmediatamente. |
| `holofs_handler_timeouts_total`             | counter | `bucket` (short/medium/long) | Respuestas 504 causadas por la deadline por bucket. |
| `holofs_backpressure_rejected_total`        | counter | `bucket` (medium/long)       | Respuestas 503 causadas por el semáforo estando en capacidad. |
| `holofs_backpressure_permits_available`     | gauge   | `bucket` (medium/long)       | Permisos aún libres. Constantemente en 0 = bucket infraaprovisionado; constantemente en máximo = ocioso. |
| `holofs_supervised_task_restarts_total`     | counter | `task` (monitor/auditor/scrub) | Pánicos + salidas inesperadas del bucle supervisado. Cualquier no-cero señala un crash repetido que el operador debe investigar. |
| `holofs_admin_auth_failures_total`          | counter | `outcome` (missing/bad/disabled) | Rechazos de bearer-token admin divididos por razón. `disabled` = superficie rehusada porque ni `HOLOFS_ADMIN_TOKEN` ni `HOLOFS_ADMIN_UNAUTHENTICATED` están definidas. |

Un clúster sano mantiene los contadores de auto-curación en cero o
cerca de cero; una tasa sostenida no cero en
`auto_repair_failures_total` es la señal de alerta del operador de que
el placement / pérdida de disco ha ido más allá de lo que el umbral K
puede absorber.

Los contadores de fiabilidad (fallos de persistencia, timeouts de
handler, rechazos de backpressure, reinicios supervisados, fallos de
admin-auth) juntos forman el "dashboard de alertas de fiabilidad" —
cada uno de ellos debe estar plano en cero en un clúster bien
aprovisionado con un token configurado. Véase las reglas de alerta de
referencia debajo.

Releases futuros añadirán histogramas para RTT de cable, latencia de
decode y reputación por objeto (actualmente registrada solo vía
`tracing`).

### 6.2. Reglas de alerta de referencia

```yaml
groups:
- name: holofs
  rules:
  - alert: HolofsNodeDown
    expr: holofs_node_up == 0
    for: 5m
    annotations:
      summary: "holofs node {{ $labels.node }} is down"

  - alert: HolofsZoneDegraded
    expr: count by (zone) (holofs_node_up == 0) >= 2
    for: 10m
    annotations:
      summary: "zone {{ $labels.zone }} has ≥2 dead nodes (margin loss)"

  - alert: HolofsDiskFillingFast
    expr: predict_linear(holofs_bytes_stored_total[1h], 24*3600) > node_filesystem_size_bytes
    for: 30m
    annotations:
      summary: "node {{ $labels.node }} will fill within 24h"

  - alert: HolofsRepairFailing
    expr: rate(holofs_repair_jobs_total{result="failed"}[15m]) > 0.1
    for: 30m

  - alert: HolofsLowReputation
    expr: holofs_node_reputation < 0.5
    for: 1h
    annotations:
      summary: "node {{ $labels.node }} reputation collapsed (audit mismatches)"

  # Alertas de fiabilidad de la serie N.

  - alert: HolofsCatalogPersistFailing
    expr: rate(holofs_catalog_persist_failures_total[10m]) > 0
    for: 5m
    annotations:
      summary: "gateway is failing to persist the catalog to disk"
      description: |
        holofs_catalog_persist_failures_total is climbing.
        Every increment = one 500 on a PUT/mkdir/rmdir/rename and one
        write that in-memory succeeded but on-disk didn't. Next
        restart will drop those changes. Check disk space + FS mount
        options on the gateway host.

  - alert: HolofsHandlerTimeouts
    expr: rate(holofs_handler_timeouts_total[15m]) > 0.05
    for: 15m
    annotations:
      summary: "handler bucket {{ $labels.bucket }} exceeding deadline"
      description: |
        More than one 504 every ~20 seconds. Slow cluster, slow disk,
        or the deadline is too tight for the traffic pattern.

  - alert: HolofsBackpressureSaturated
    expr: holofs_backpressure_permits_available == 0
    for: 5m
    annotations:
      summary: "bucket {{ $labels.bucket }} has zero permits available"
      description: |
        The MEDIUM/LONG semaphore is at 0 for 5 minutes straight.
        Either the cluster is genuinely overloaded (scale up nodes)
        or the cap is too low for the workload — bump the matching
        HOLOFS_*_CONCURRENCY env var.

  - alert: HolofsSupervisedTaskRestarting
    expr: rate(holofs_supervised_task_restarts_total[30m]) > 0
    for: 15m
    annotations:
      summary: "{{ $labels.task }} is crashing repeatedly"
      description: |
        The supervised background loop is panicking + being restarted
        by supervised_spawn. Read the gateway logs for the panic
        payload and file a bug.

  - alert: HolofsAdminAuthAttempts
    expr: rate(holofs_admin_auth_failures_total{outcome=~"missing|bad"}[10m]) > 0.1
    for: 10m
    annotations:
      summary: "admin surface seeing sustained 401s (possible probe)"
      description: |
        Someone is hitting /admin/node or /api/gc without a valid
        bearer. Missing = no Authorization header at all; bad = wrong
        token. If unexpected, treat as a probe.
```

### 6.3. Trazado

Cuando `HOLOFS_TELEMETRY_OTLP` está definido, el gateway exporta
spans OTLP/HTTP:

| Nombre del span        | Atributos útiles                           |
|------------------------|--------------------------------------------|
| `gateway.put`          | `object.kind`, `bytes.in`, `shards.out`    |
| `gateway.get`          | `object.kind`, `layers`, `bytes.out`, `nodes_contacted` |
| `gateway.repair`       | `object_id`, `channel`, `layer`, `shards_recovered` |
| `wire.send`            | `op`, `target.node`, `bytes`               |

### 6.4. Dashboards

Un dashboard Grafana de referencia JSON se envía en
`deploy/grafana/holofs.json`. Paneles principales: tasa de ingesta,
P99 de decode por tipo, % de dedup, throughput de reparación, mapa de
calor de disponibilidad de nodos por zona.

---

## 7. Planificación de capacidad

### 7.1. Overhead de almacenamiento

El coste de almacenamiento está dominado por la redundancia RLNC entre
capas de prioridad. Para un objeto de tamaño de payload `S`:

$$
\text{stored bytes} \approx S \cdot \sum_{\ell} R_\ell \cdot \frac{N_\ell}{K_\ell}
$$

Para ratios de capa por defecto `R = [4.0, 2.5, 1.6, 1.15]`, el
overhead promedio es aproximadamente **9,25×** (contando metadatos,
~9,4×).

| Tamaño de objeto | Almacenado en clúster | Por nodo (40 nodos) |
|------------------|-----------------------|---------------------|
| 1 MB             | ~9,4 MB               | ~235 KB             |
| 1 GB             | ~9,4 GB               | ~235 MB             |
| 1 TB             | ~9,4 TB               | ~235 GB             |

**Ajustar para almacenamiento más barato:** bajar `R_0` (redundancia
de pérdida catastrófica) a `2.0` y `R_1..3` a `[1.5, 1.2, 1.05]` — el
overhead cae a ~5,75×. Véase [theory.md §4](./theory.md#4-capas-de-prioridad-y-degradación-holográfica)
para el trade-off del margen de supervivencia.

### 7.2. Planificación de CPU

| Operación              | Coste (relativo a memcpy) | Cuello de botella |
|------------------------|---------------------------|-------------------|
| GF(2⁸) multiply        | 4× memcpy (LUT)           | caché L1          |
| Haar 2D forward        | 3× memcpy                 | Ancho de banda RAM |
| RLNC encode K=16, payload 1024 B | 60× memcpy      | CPU               |
| SHA-256 sobre 1 MB     | 2× memcpy (con SIMD)      | CPU               |

Un núcleo x86_64 moderno sostiene ~150 MB/s de codificación RLNC para
K=16. Escala linealmente en multi-core hasta que la I/O de disco se
convierte en el cuello de botella (~500 MB/s en NVMe).

### 7.3. Planificación de red

Ancho de banda de cable en el peor caso por Put:

```
egress = payload × redundancy_factor × layer_count
       ≈ payload × 9.25
```

Para un upload de 100 MB, el gateway emite ~925 MB al pool de nodos.
Planifica **al menos 1 Gbit/s** entre gateway y nodos.

### 7.4. Dimensionamiento correcto del clúster

| Propiedad                 | Elige por                                  |
|---------------------------|--------------------------------------------|
| `N_nodes`                 | ≥ 4 × `K` para que RLNC tenga holgura de placement |
| `N_zones`                 | ≥ 3; 5 recomendado para pérdida de cualquier zona |
| `K`                       | 16 (por defecto) — punto dulce de CPU vs margen |
| `redundancy_per_layer`    | ajustar al margen de supervivencia deseado ≥ 5σ |

---

## 8. Backup y restauración

### 8.1. Qué vive en disco

Por nodo (`HOLOFS_DATA_DIR`):

```
manifests/         manifiestos por objeto (HOLOFSM6)
catalog/HOLOFSD1   el índice de directorio (escritura atómica)
shards/aa/bb……    archivos .shard, direccionados por contenido
identity/secret    clave privada Ed25519
whitelist.holofs   lista de peers firmada por admin
```

### 8.2. Modelo de backup

**holofs es su propio backup** para cualquier objeto *individual* —
perder un nodo dispara reparación RLNC desde hermanos. El backup
importa para:

1. **Pérdida catastrófica del clúster** (p. ej. todas las zonas offline).
2. **Corrupción lógica / borrado accidental** (`Purge` es irreversible).
3. **Material de identidad** (claves Ed25519 + whitelist firmada) — sin
   ellos, los reemplazos no pueden reunirse a un clúster confiable.

### 8.3. Plan de backup recomendado

| Datos                | Frecuencia      | Herramientas              | Dónde               |
|----------------------|-----------------|---------------------------|---------------------|
| Identidad + whitelist | En cada cambio | `restic`, `aws s3 sync`   | Cifrado fuera del sitio |
| Snapshot de catálogo | Cada hora       | `cp catalog/HOLOFSD1 → …` | S3 / NFS / cinta    |
| Directorio de shards | Opcional        | `restic` o snapshots zfs  | Almacenamiento frío |

Un `holofs-admin export <name>` periódico reconstruye un objeto en un
único archivo canónico y lo escribe a un bucket externo. Esta es la
forma recomendada de respaldar **objetos específicos de alto valor**.

### 8.4. Procedimientos de restauración

| Escenario                             | Procedimiento |
|---------------------------------------|---------------|
| Disco de un único nodo perdido        | Limpiar disco; reiniciar nodo; el clúster auto-repara los shards. |
| Múltiples nodos perdidos, < margen    | No se necesita acción — el decode RLNC lo tolera. |
| Catálogo corrupto en gateway          | Copiar `catalog/HOLOFSD1` de un gateway par o del último backup horario; reiniciar. |
| Todo el clúster perdido               | Aprovisionar clúster nuevo; `holofs-admin import` cada exportación externa. |
| Compromiso de clave de whitelist      | Generar nueva clave de admin; re-firmar whitelist; hot-reload (véase [§10.4](#104-hot-reload-de-whitelist)). |

---

## 9. Recuperación ante desastres

### 9.1. Objetivos RTO / RPO

| Fallo                          | RTO       | RPO     | Disparador                             |
|--------------------------------|-----------|---------|----------------------------------------|
| Nodo único                     | < 1 min   | 0       | Auto (monitor + reparación)            |
| Zona única (≤ ⅕ de nodos)      | < 5 min   | 0       | Auto (el margen sigue positivo)        |
| Dos zonas simultáneamente      | < 1 hr    | Horas   | Manual: re-aprovisionar + importar     |
| Todo el clúster                | < 8 hr    | ≤ 1 hr  | Manual: restauración completa desde backups S3 |

### 9.2. Árbol de decisión

```mermaid
flowchart TD
    A[Alert: nodes down] --> B{Margin > 0?}
    B -- Yes --> C[No action — let repair drain]
    B -- No  --> D{Catalog reachable?}
    D -- Yes --> E[Restore lost zones from manifest hints]
    D -- No  --> F[Bootstrap new cluster + import from off-site]
```

### 9.3. Simulacros

Ejecutar trimestralmente. Escenarios sugeridos:

1. **Simulacro de kill de zona** — `kubectl drain` todos los pods en una
   etiqueta de zona; aseverar que ningún objeto se vuelve inaccesible
   y la reparación se completa en < 10 min.
2. **Simulacro de restauración en frío** — en un clúster k8s fresco,
   restaurar `<storage>/` desde el bucket de backup (`restic restore`
   / `rclone copy`), levantar el gateway, verificar `/api/stats` y un
   GET puntual; medir RTO.
3. **Simulacro de rotación de clave** — firmar una nueva whitelist con
   la clave de admin, hot-reload sin downtime.

---

## 10. Procedimientos día-2

### 10.1. Añadir un nodo

```sh
# 1. Arrancar el nuevo nodo una vez para que materialice su identity
#    e imprima su pubkey. El directorio storage debe estar vacío.
holofs-node 10.0.3.10:9100 --storage /var/lib/holofs/node41
# → holofs-node addr=10.0.3.10:9100 pubkey=NEW_PUBKEY_HEX

# 2. Re-firmar la whitelist con el conjunto *completo* de nodos
#    (sign-whitelist siempre regenera el archivo desde cero).
holofs-admin sign-whitelist \
  --admin admin.key \
  --node 10.0.1.10:9100=NODE0_PUBKEY_HEX:0 \
  ... \
  --node 10.0.3.10:9100=NEW_PUBKEY_HEX:4 \
  --out whitelist.holofs

# 3. Distribuir whitelist.holofs a cada nodo + gateway; SIGHUP.
```

El catálogo permanece sin cambios; los placements futuros pueden
elegir el nuevo nodo vía HRW. Los objetos existentes **no** se
rebalancean automáticamente — el scrub en segundo plano
(`HOLOFS_SCRUB_INTERVAL`) y la auto-reparación en lectura migran
los shards gradualmente.

### 10.2. Eliminar (decomisar) un nodo

No hay un comando `drain` dedicado — decomisar es una edición de la
whitelist + apagar el demonio; el bucle de reparación del clúster
restaura los shards perdidos.

```sh
# 1. Re-firmar whitelist sin el nodo que sale.
holofs-admin sign-whitelist \
  --admin admin.key \
  --node 10.0.1.11:9100=NODE1_PUBKEY_HEX:0 \
  ... \
  --out whitelist.holofs

# 2. Distribuir + SIGHUP cada nodo + gateway restante.
# 3. Observar cómo `holofs_repair_completed_total` sube: el scrub
#    reubica los shards del nodo que se fue en los supervivientes.
# 4. Una vez que /api/stats muestre los objetos totalmente reparados,
#    apagar el viejo demonio.
systemctl stop holofs-node@10
```

### 10.3. Reemplazar un disco fallido

1. `systemctl stop holofs-node@N`
2. Reemplazar disco, montar filesystem fresco en `HOLOFS_DATA_DIR`.
3. Restaurar archivos de identidad (`identity/secret`, `whitelist.holofs`)
   desde el backup externo — estos están atados a la dirección del
   nodo, no al disco.
4. `systemctl start holofs-node@N` — el clúster rellenará el disco vía
   reparación dirigida por auditoría en minutos a horas dependiendo
   del tamaño.

### 10.4. Hot-reload de whitelist

```sh
# Colocar nuevo archivo de whitelist en su sitio
cp whitelist.holofs.new /etc/holofs/whitelist.holofs

# Señalar a todos los demonios
killall -SIGHUP holofs-node holofs-web
```

Los demonios re-verifican la firma del admin antes de intercambiar la
nueva lista. Una firma incorrecta se registra y se mantiene la lista
vieja.

### 10.5. Actualización en rolling

Holofs garantiza compatibilidad del protocolo de cable dentro de una
versión menor (`1.x → 1.x+1` es seguro). Para k8s:

```sh
helm upgrade holofs ./deploy/helm/holofs --set image.tag=1.0.0
```

El StatefulSet rueda un pod a la vez, espera a la disponibilidad,
luego procede. Durante el rollout el clúster opera degradado por
exactamente un nodo — muy dentro del margen para cualquier
dimensionamiento por defecto.

### 10.6. Chuleta de comandos de salud

```sh
# Visión general del clúster
curl -s http://gw:8787/api/stats | jq

# Salud por nodo (HTML en navegador; JSON vía cabecera accept)
curl -s -H "accept: application/json" http://gw:8787/health

# Margen por (canal, capa) para un objeto
curl -s http://gw:8787/health/photo.png

# Inspeccionar distribución de shards
curl -s http://gw:8787/inspect/photo.png
```

Véase [api.md](./api.md) para el inventario completo de rutas.

### 10.7. Pruebas de soak

`holofs-soak` lanza tráfico HTTP aleatorio contra un gateway vivo
durante horas, registra todo lo que pasó y sale con un
`summary.json` — el flujo previsto es "bloquear un cambio sospechoso
con un soak nocturno, triaging `errors.jsonl` a la mañana
siguiente".

Tres topologías de cluster vía `--topology`:

| `--topology`     | Qué hace el runner                                                              |
|------------------|---------------------------------------------------------------------------------|
| `external`       | Se conecta a un gateway ya en ejecución en `--base` (por defecto). Sin lifecycle. |
| `embedded`       | Spawnea un proceso `holofs-web` con el cluster in-process de 40 nodos.           |
| `multi-process`  | Spawnea `--nodes` procesos `holofs-node` + un `holofs-web` con whitelist.        |

Para las dos topologías con spawn, el storage-root es un tempdir
scratch bajo `$TMPDIR` (borrado al salir salvo que se dé
`--cluster-storage <dir>`), y el seed post-boot es `deploy/dev-seed.sh`
salvo que `--seed-script <path>` lo sobreescriba. Los binarios se
buscan junto a `holofs-soak`; para otra ubicación usar
`--binary-dir <dir>` (p.ej. `target/release`).

**Feature-flags opcionales del gateway spawneado:**

- `--enable-embed` — activa la búsqueda semántica CLIP en el
  `holofs-web` spawneado y dispara un `POST /api/embed_all` tras el
  seed para que el índice esté poblado antes de que arranquen los
  workers. Sin el flag, el runner sondea `/api/search` en el boot y
  saca la op `search` del mix — sin tormenta de 500 sobre una
  feature no cableada.
- `--enable-versions` — activa el historial de versiones por objeto.
  Si está off, `versions_list` cae del mix del mismo modo.

Ambos flags son `false` por defecto (coincide con `make dev`) para
que los smoke-runs cortos arranquen rápido. Actívelos para soaks
realistas de 8 h.

**Perillas de throttling.** 50 workers × ~0.5 s de think-time dan
por defecto ~100 ops/s — bastante para estresar un cluster embedded
de 40 nodos, lo suficientemente liviano para no autoprovocar una
tormenta de retries. Cuatro flags para afinar:

| Flag                        | Defecto | Efecto                                                                       |
|------------------------------|---------|------------------------------------------------------------------------------|
| `--thinktime <dur>`          | `500ms` | Cota superior del sleep aleatorio que cada worker toma entre ops.             |
| `--error-backoff <dur>`      | `500ms` | Sleep base tras un 5xx / error de transport. Duplica por fallo consecutivo.   |
| `--error-backoff-max <dur>`  | `30s`   | Tope del backoff exponencial.                                                 |
| `--rate-limit <ops/s>`       | `0`     | Token bucket global compartido por todos los workers. `0` = desactivado.      |
| `--op-mix "op=w,..."`        | `""`    | Sobreescribe el peso de cualquier op; `w=0` la elimina del mix.               |

Activar **`--rate-limit`** da un techo duro independientemente del
número de workers — útil para histogramas de latencia reproducibles.
`--op-mix` permite recortar escenarios read-heavy o write-heavy sin
tocar el fuente (p.ej. `--op-mix "put_new=3,put_replace=2"` para un
perfil mayormente lectura, `--op-mix "search=0,similar=0"` para
saltar endpoints analíticos).

Los pesos efectivos y ajustes de throttle también se escriben en
`config.json` para que el análisis post-run sepa exactamente qué mix
produjo los números.

**Perfiles baseline medidos en esta máquina.** Soak de 3 min sobre
`--topology multi-process --nodes 4` (Macbook serie M, build
release):

| Perfil                        | Workers | Op-mix                     | Timeout | RPS   | Err % |
|-------------------------------|--------:|----------------------------|--------:|------:|------:|
| smoke-only                    | 10      | default                    | 30 s    | 1.7   | 3.9 % |
| default (inusable)            | 50      | default                    | 30 s    | 4.4   | 45 %  |
| write-light                   | 50      | `put_new=3,put_replace=2`  | 30 s    | 23.4  | 7.5 % |
| **sweet spot realista**       | **50**  | **`put_new=3,put_replace=1`** | **60 s** | **8.4** | **4.0 %** |
| paciencia de cliente larga    | 50      | `put_new=3,put_replace=1`  | 120 s   | 10.9  | 10.7 % |

**Async ingest (`HOLOFS_ASYNC_ENCODE=1`).** Flag server-side
opcional que cambia el camino PUT RLNC por defecto de sync
(`201 Created` tras fin de encode + fanout) a async: el manifest
placeholder se commitea síncronamente en `ManifestState::Encoding`,
el encode + fan-out de shards corren en un tokio-task detached y el
handler retorna `202 Accepted` con header `Location: /path` + JSON
`{state:"encoding", …}`. Los handlers de lectura gatean por estado —
GET/HEAD sobre `Encoding` devuelve `503 Retry-After: 5`, sobre
`Failed` devuelve `404`. DELETE sobre `Encoding` devuelve `409
Conflict`. La recuperación en arranque baja todo manifest `Encoding`
sobreviviente a `Failed` para que un shutdown sucio no deje
tombstones.

Medido en la topología soak multi-process de 4 nodos, mismo perfil
(`--workers 50 --op-mix "put_new=3,put_replace=1" --thinktime 500ms`):

| Camino            | PUT p50    | RPS total | Notas |
|-------------------|-----------:|----------:|-------|
| Sync (baseline)   | 49 969 ms  | 8.4       | Cliente espera el encode completo. |
| Sync + fan-out    | 34 822 ms  | 5.3       | Wire paralelo; encode aún en el hot path. |
| **Async 202**     | **113 ms** | **24.1**  | Encode completamente fuera del hot path. |

El runner en su forma actual no entiende el polling `202` +
`Retry-After` — trata un GET sobre `Encoding` como un 503 normal —
por eso el run async de arriba reporta una tasa de error inflada de
~45 %. Un cliente polling-aware (o un futuro cambio de runner)
colapsa eso de vuelta a 200 normales.

**Cuándo usar `HOLOFS_ASYNC_ENCODE=1`:** pipelines burst-heavy donde
el llamador tolera un flujo "vuélveme a llamar" — subidas masivas,
jobs de sync/replicación, batch ingest. Sync sigue siendo el defecto
para PUTs interactivos donde el cliente quiere un `201` limpio y un
data_cid final.

Dos hallazgos contra-intuitivos del estudio:

- Subir `--request-timeout` de 60 s a 120 s empeoró las cosas, no
  mejoró: clientes que esperan más mantienen más PUT concurrentes
  en vuelo, los MEDIUM-permits (default 64) se llenan, cascada de
  5xx. 60 s es el sweet spot para un cluster de 4 nodos.
- Subir `HOLOFS_MEDIUM_CONCURRENCY` del gateway de 64 a 128 también
  empeoró — los permits extra dejan correr más PUT, pero PUT es
  CPU-heavy (decodificación JPEG + DWT + fanout RLNC) y mata de
  hambre a los GET concurrentes en el mismo host. GET p50 saltó de
  1 ms a 79 ms, tasa neta de error subió. 64 sigue siendo el
  defecto; solo suba si la carga es demostrablemente read-dominant.

```sh
# 1) External: cluster ya en marcha, p.ej. vía `make dev`.
./target/release/holofs-soak \
    --topology external \
    --base http://127.0.0.1:8787 \
    --workers 50 --duration 8h --out .soak

# 2) Embedded: 40 nodos in-process; lo más simple, coincide con `make dev`.
./target/release/holofs-soak \
    --topology embedded \
    --gateway-port 8787 \
    --enable-embed --enable-versions \
    --workers 50 --duration 8h \
    --binary-dir target/release --out .soak

# 3) Multi-process: N daemons de nodo + gateway con whitelist firmada.
./target/release/holofs-soak \
    --topology multi-process \
    --nodes 8 --node-base-port 5100 --gateway-port 8787 \
    --enable-embed --enable-versions \
    --workers 50 --duration 8h \
    --binary-dir target/release --out .soak
```

Cada run escribe en `.soak/<utc-timestamp>/`:

| Fichero                 | Contenido                                                            |
|-------------------------|----------------------------------------------------------------------|
| `config.json`           | Parámetros usados (seed, duración, workers, base URL, timeouts).     |
| `ops.jsonl`             | Una línea por llamada HTTP: `{t, worker, op, target, http, ms, err?}`. |
| `errors.jsonl`          | Mismo esquema, filtrado a `http >= 500` o errores de transport.       |
| `metrics.jsonl`         | Snapshot `/metrics` + `/api/stats` cada `--metrics-interval`.         |
| `health-events.jsonl`   | Stream SSE crudo de `/api/health/events`.                             |
| `summary.json`          | Conteos por op, latencia p50/p95/p99, histograma de estados HTTP.     |

La selección de ops está pesada hacia lecturas (`get_random` ≈ 30 %,
`put_new` ≈ 15 %, `put_replace` ≈ 10 %, `search` ≈ 8 %, mutaciones
de catálogo ≈ 12 %) para que el runner ejercite los caminos read +
version más fuerte que la superficie admin. Ctrl-C apaga limpiamente
y aún así escribe el summary. Pesos y conjunto de ops están
compilados — parche `crates/holofs-cli/src/bin/holofs-soak.rs` si
necesita un mix distinto para una investigación específica.

El runner es intencionalmente **read-mostly en la superficie
admin**: no llama a `/api/gc`, `/admin/node` ni a los endpoints
escrow — se puede apuntar a un gateway staging vivo sin efectos
secundarios sobre el estado del cluster más allá de los PUT/DELETE
normales.

Shutdown grácil en las tres topologías:

- Ctrl-C o el plazo de `--duration` conmuta un `CancellationToken`;
  workers, writer, colector de métricas y consumidor SSE drenan en
  orden, luego se escribe `summary.json`.
- Para `embedded`/`multi-process`, los hijos spawneados reciben
  SIGTERM (vía `Child::start_kill`) después de que `summary.json`
  esté en disco, cada uno con 5 s de grace-period. Los tempdirs
  scratch se borran al salir.
- Si el run panic antes de `summary.json`, `kill_on_drop(true)` en
  cada `Child` spawneado igual garantiza que ningún proceso gateway
  o node se filtre al siguiente test.

### 10.7.a. Informes

`holofs-soak-report` convierte un directorio de run en un informe
self-contained. HTML es el defecto (CSS inline + gráficos SVG
inline, sin CDN, sin JS — se abre en cualquier navegador y sigue
legible años después); Markdown está disponible para summaries
committable o adjuntos de issue de GitHub. Ambos formatos en un
solo llamado con `--format both`.

```sh
# Último run bajo .soak/, HTML → .soak/<run>/report.html
holofs-soak-report

# Run explícito, ambos formatos, buckets de 30 s para un soak corto
holofs-soak-report .soak/2026-07-07T15-34-41Z --format both --bucket 30s

# Ruta de salida custom (la extensión se añade automática para `both`)
holofs-soak-report --format both --output ~/soak-nightly
# → ~/soak-nightly.html + ~/soak-nightly.md
```

El informe contiene:

1. **Vista general** — total de ops, tasa de error, RPS medio,
   elapsed vs duración configurada, tamaño de bucket.
2. **Timings por operación** — count, errores, skips, p50/p95/p99
   ms, max ms.
3. **Timelines de throughput y errores** — RPS por bucket + errores
   `{4xx, 5xx, transport}` apilados por bucket, más un overlay de
   latencia p95 para las top-5 ops por volumen.
4. **Carga por worker** — bar-charts de ops y errores.
5. **Top errores** — tríos `(op, target, http)` más frecuentes más
   mensajes transport-level deduplicados.
6. **Telemetría del cluster** — timelines de `objects_total`,
   `shards_total`, `bytes_total`, `nodes_live` y los contadores de
   repair directamente desde `/api/stats`; más las métricas
   Prometheus `holofs_backpressure_rejected_total`,
   `holofs_handler_timeouts_total`,
   `holofs_rate_limit_rejected_total` y
   `holofs_backpressure_permits_available{bucket}` parseadas desde
   `metrics.jsonl`.
7. **Muestra de health-events** — primeras 20 tramas SSE literales
   (la cola se elide con un conteo).
8. **Reproducibilidad** — `config.json` completo embebido al final
   para rerun exacto.
