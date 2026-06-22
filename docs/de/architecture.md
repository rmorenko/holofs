# Architektur

Strukturelle Übersicht von holofs auf Systemebene, gedacht für Maintainer und
Reviewer. Mathematische Grundlagen siehe [theory.md](./theory.md); HTTP- /
Wire-Protokoll-Details siehe [api.md](./api.md).

## Inhalt

1. [Crate-Abhängigkeitsgraph](#1-crate-dependency-graph)
2. [Prozess- / Deployment-Topologien](#2-process--deployment-topologies)
3. [Objektlebenszyklus (PUT → GET)](#3-object-lifecycle-put--get)
4. [Persistenzmodell](#4-persistence-model)
5. [Vertrauensmodell](#5-trust-model)
6. [Nebenläufigkeitsmodell](#6-concurrency-model)
7. [Fehlermodi](#7-failure-modes)

---

## 1. Crate-Abhängigkeitsgraph

Strikte topologische Reihenfolge — Pfeile dürfen niemals nach oben zeigen.

```mermaid
graph BT
    core["holofs-core<br/>GF, DWT, RLNC, SHA-256, Merkle"]
    wire["holofs-wire<br/>tokio framing + Request/Response"]
    model["holofs-model<br/>Manifest, Directory, Placement"]
    codec["holofs-codec<br/>image/audio/text"]
    storage["holofs-storage<br/>Store, Identity, Whitelist"]
    client["holofs-client<br/>PUT/GET/REPAIR/AUDIT"]
    cluster["holofs-cluster<br/>health, monitor, audit, repair"]
    analytics["holofs-analytics<br/>fingerprint, MinHash, escrow"]
    gateway["holofs-gateway<br/>HTTP/1.1 server + admin UI"]
    cli["holofs-cli<br/>holofs-node, -http, -admin, ..."]
    web["holofs-web<br/>Leptos SSR + hydration (WIP)"]

    core --> wire
    core --> model
    core --> codec
    core --> storage
    wire --> storage
    storage --> client
    model --> client
    codec --> client
    wire --> client
    core --> client
    core --> cluster
    model --> cluster
    wire --> cluster
    storage --> cluster
    client --> cluster
    core --> analytics
    model --> analytics
    core --> gateway
    model --> gateway
    wire --> gateway
    codec --> gateway
    storage --> gateway
    client --> gateway
    cluster --> gateway
    analytics --> gateway
    gateway --> cli
    gateway --> web
```

**Faustregel.** Ein Pull Request, der eine aufwärts gerichtete Kante in
diesem Graphen hinzufügt, bedarf einer separaten Diskussion — fast immer
bedeutet das, dass ein Typ oder eine Funktion im falschen Crate liegt.

---

## 2. Prozess- / Deployment-Topologien

### A. Eingebettet, Einzelprozess (Entwicklung / kleine Cluster)

```mermaid
flowchart LR
    user["browser / curl"] -->|HTTP 8787| http["holofs-web process"]
    subgraph http_p["holofs-web process (axum + Leptos SSR)"]
        gw["Gateway"]
        subgraph tokio["tokio runtime"]
            n0["node 00 :9100"]
            n1["node 01 :9101"]
            ndots["..."]
            nN["node 39 :9139"]
        end
        gw -- TCP loopback --> n0
        gw -- TCP loopback --> n1
        gw -- TCP loopback --> nN
    end
    n0 --> disk0["./holofs-data/node_00/<br/>shards + identity.key"]
    n1 --> disk1["./holofs-data/node_01/"]
    nN --> diskN["./holofs-data/node_39/"]
```

Wird für Demos, Entwicklung und Einzelmaschinen-Bare-Metal verwendet. Das
gateway und die nodes teilen sich eine tokio-Runtime, kommunizieren jedoch
über echtes TCP — eine spätere Migration zu Mehrprozess ist einfach.

### B. Multi-Prozess-Bare-Metal-Cluster

```mermaid
flowchart LR
    user["client"] -->|HTTP| gw["holofs-web<br/>(separate process)"]
    gw -->|TCP wire protocol| n1["holofs-node<br/>process 1<br/>--storage /var/lib/holofs/n1"]
    gw -->|TCP| n2["holofs-node<br/>process 2"]
    gw -->|TCP| nM["holofs-node<br/>process M"]
    admin["holofs-admin<br/>(CLI, one-shot)"] -.->|sign whitelist| gw
```

Jeder node ist ein unabhängiger Betriebssystemprozess mit eigenem persistentem
Speicherverzeichnis und eigener Ed25519-Identität. Das gateway ist mit einer
signierten whitelist von `(addr, pubkey, zone)`-Tripeln konfiguriert. Die
Fehlerisolation ist real: das Beenden eines node-Prozesses reißt nichts
anderes mit.

Skript-Variante: `./scripts/spawn-cluster.sh N BASE_PORT GATEWAY_ADDR` bringt
die gesamte Topologie mit einem Befehl hoch.

### C. Kubernetes (StatefulSet)

```mermaid
flowchart TB
    subgraph cluster_k8s["Kubernetes cluster"]
        ing["Ingress / LoadBalancer<br/>(holofs.example.com)"]
        ing -->|HTTPS| svc["Service holofs"]
        svc --> p0["Pod holofs-0<br/>PVC: /data"]
        svc --> p1["Pod holofs-1<br/>PVC: /data"]
        svc --> pN["Pod holofs-N"]
    end
```

`deploy/helm/holofs` stellt StatefulSet- + PersistentVolumeClaim-Templates
bereit. Jeder Pod führt das mehrstufige Docker-Image aus, das eingebettete
nodes gegen sein eigenes `/data` PVC automatisch startet. Für sehr große
Cluster wird in N gateway-Pods + M dedizierte node-Pods aufgeteilt (Helm-Chart
unterstützt `nodeCount` und `gatewayCount` separat).

---

## 3. Objektlebenszyklus (PUT → GET)

```mermaid
sequenceDiagram
    participant C as Client
    participant GW as Gateway
    participant N1 as Node 1
    participant N2 as Node 2
    participant N40 as Node 40

    C->>GW: PUT /my.png (image bytes)
    GW->>GW: detect kind (image / audio / text / opaque)
    GW->>GW: decode → channels f32 (image_io)
    GW->>GW: per channel: Haar DWT × LEVELS
    GW->>GW: split into 4 priority layers
    GW->>GW: encode_layer(K=16, n=RED[ℓ]·K) per (channel, layer)
    GW->>GW: compute CID, manifest, Merkle root
    par for each shard
        GW->>N1: PUT shard (HRW + zone-aware placement)
        GW->>N2: PUT shard
        GW->>N40: PUT shard
    end
    GW->>GW: persist Directory to disk (catalog.bin)
    GW-->>C: 201 + JSON {object_id, data_cid, shards, put_ms}

    Note over C,N40: ... time passes, some nodes die ...

    C->>GW: GET /my.png
    GW->>GW: lookup manifest in catalog
    par gather alive shards
        GW->>N1: GET shards for (c, l)
        GW->>N2: GET shards for (c, l)
    end
    GW->>GW: verify against shard_hashes (reject corrupt)
    GW->>GW: decode_layer (fast / partial / full)
    GW->>GW: inverse DWT, encode PNG
    GW-->>C: 200 image/png
```

### Divergenz nach Art

| Art      | PUT-Pfad                                              | GET-Antwort        |
|----------|-------------------------------------------------------|--------------------|
| image    | DWT 2D × 3 Kanäle × 4 Schichten × RLNC                | neu codiertes PNG  |
| audio    | DWT 1D × 1–2 Kanäle × 4 Schichten × RLNC              | WAV 16-Bit-PCM     |
| text     | UTF-8-grenzentreue Chunks × 1 Schicht × systematisches RLNC | text/plain + Lücken |
| opaque   | ein Byte-Strom × 1 Schicht × RLNC (kein DWT)          | Originalbytes      |

---

## 4. Persistenzmodell

Jeder node besitzt ein Verzeichnis. Drei Arten von Dateien liegen dort:

```
<storage>/
├── identity.key                # 32-byte Ed25519 seed (mode 0600)
├── <hex>/<hex>.shard           # one file per stored shard
└── <hex>/<hex>.shard
```

Das gateway besitzt zusätzlich:

```
<storage>/catalog.bin            # serialised Directory (Manifest map)
                                 # Header magic: "HOLOFSD1"
```

### Layout einer Shard-Datei

```
magic        8  bytes  = "HOLOFSS1"
object_id    8  bytes  big-endian
channel      1  byte
layer        1  byte
coeffs_len   4  bytes  big-endian
payload_len  4  bytes  big-endian
coeffs       coeffs_len bytes
payload      payload_len bytes
```

Der Dateiname ist `hex(sha256(shard))`, aufgeteilt als
`<2 hex chars>/<remaining 62>.shard` (Git-Stil-Fanout, um riesige Verzeichnisse
zu vermeiden).

### Schreib-Atomarität

```
write to <name>.shard.tmp
fsync
rename <name>.shard.tmp → <name>.shard
```

Ein Absturz hinterlässt entweder nichts oder einen vollständigen shard —
niemals eine zerrissene Datei.

### Index-Wiederherstellung

Bei `Store::open(dir)` läuft der node durch seinen Baum und rekonstruiert
den In-Memory-Index `HashMap<(object_id, channel, layer), HashMap<Hash, Shard>>`
durch erneutes Hashen jedes shards. Dies ist die einzige zulässige Wahrheitsquelle
— es gibt keine separate `.idx`-Datei, die veralten könnte.

### Dedup

Shard-Dateinamen sind inhaltsadressiert. Ein doppeltes PUT (gleiche
Koeffizienten + Payload) wird durch `fs::write(... .tmp)` → `rename` über
einer bestehenden Datei erkannt (überschreibt identisch). Die frühere
In-Memory-Index-Prüfung gibt weiterhin `false` von `put()` zurück, sodass der
Aufrufer weiß, dass kein neuer shard erschienen ist.

---

## 5. Vertrauensmodell

| Komponente      | Vertrauensannahme                                      |
|-----------------|--------------------------------------------------------|
| Admin           | absolut — signiert die whitelist, erzeugt Keypaare     |
| Gateway         | vertraut der Admin-Signatur auf der whitelist          |
| Node            | vertraut seinem eigenen `identity.key` (Dateisystem)   |
| Inter-node      | spricht nicht Peer-zu-Peer; nur gateway ↔ node         |
| Client          | vertraut dem gateway (TLS für Produktion empfohlen)    |

Wir sind ausdrücklich **kein** erlaubnisfreies System: es gibt keinen
Proof-of-Replication, keine Sybil-Resistenz. holofs sitzt in derselben
Vertrauensklasse wie Backblaze B2 oder AWS S3, nicht Filecoin oder Storj.
Siehe [threat-model.md](./threat-model.md) für eine strukturierte Analyse.

### Verwendete kryptografische Primitiven

| Zweck                            | Primitive                       | Crate                |
|----------------------------------|---------------------------------|----------------------|
| Shard- / Objektintegrität        | SHA-256 (FIPS 180-4, handgerollt) | holofs-core         |
| Node-Identität                   | Ed25519                         | ed25519-dalek (RFC 8032) |
| Admin-whitelist-Signatur         | Ed25519                         | ed25519-dalek       |
| Handshake-Challenge              | zufällige 32-Byte-nonce + Ed25519 | holofs-storage    |
| Key Escrow / Shamir-Stil         | RLNC über GF(2⁸) mit benutzerdefiniertem K | holofs-analytics |
| Domain-Trennung                  | String-Präfix (`holofs-XXX-vN`) vor Hash- / Sign-Eingabe |

---

## 6. Nebenläufigkeitsmodell

- **Tokio Multi-Thread-Runtime** an der Spitze jedes Binaries
  (`#[tokio::main(flavor = "multi_thread")]`).
- **Eine Task pro Verbindung** im gateway und in jedem node.
- **`tokio::sync::Mutex`** für gemeinsamen Zustand (Katalog, Store, Reputation,
  admin\_kills). Wir halten niemals einen Mutex über eine `.await`-Grenze
  hinweg auf einem heißen Pfad — stattdessen Snapshot-Muster.
- **Kanäle** werden noch nicht verwendet (das System ist Request/Response);
  zukünftige Streaming-Antworten werden `tokio::sync::mpsc` nutzen.

### Hintergrund-Tasks im gateway

| Task               | Takt    | Crate           |
|--------------------|---------|------------------|
| Health-Monitor     | `HOLOFS_MONITOR_INTERVAL` (15 s Standard) | holofs-cluster |
| PoR-Auditor        | `HOLOFS_AUDIT_INTERVAL` (30 s Standard)   | holofs-cluster |
| Katalog-Autosave   | bei jeder Katalogmutation (inline)        | holofs-gateway |

Beide Hintergrund-Tasks werden bei SIGINT über `tokio::select!` abgebrochen.

---

## 7. Fehlermodi

| Fehler                                      | Erkannt durch                | Wiederherstellung                 |
|---------------------------------------------|------------------------------|-----------------------------------|
| Node-Prozess stirbt                         | Health-Monitor (`Ping`)      | Marge neu berechnet; bei `LowMargin` wird Reparatur in die Warteschlange gestellt |
| Node-Betriebssystem startet neu, kommt mit gleicher Identität zurück | Health-Monitor-`revived`-Event | `repair_node` füllt HRW-Anteil neu auf |
| Node liefert falsche Bytes (stille Korruption) | PoR-Audit (Hash-Mismatch)  | Reputation sinkt; node aus `live` ausgeschlossen |
| Node lügt "ich habe es", ohne zu speichern  | PoR-Audit (`MissingShard`)   | Reputation sinkt |
| Ganzes Rack / ganze Zone fällt aus          | Health-Monitor + zone-aware  | Objekt bleibt decodierbar bis L_{n-1}/L_{n-2} |
| Gateway stürzt während PUT ab               | Client-Retry                 | bereits auf nodes vorhandene shards werden beim Retry per Hash dedupliziert |
| Gateway stürzt während DELETE ab            | inkonsistent: einige nodes purged, andere nicht | nächster Health-Durchgang erkennt verwaiste shards (TODO: gc) |
| Plattenkorruption an einer Shard-Datei      | Hash-Verifikation beim Lesen | shard verworfen → Marge sinkt → Reparatur |
| Netzwerkpartition zwischen gateway und node | RPC-Timeout                  | Health-Monitor → ausschließen → reparieren, falls Marge sinkt |
| Whitelist-Signatur ungültig                 | Gateway-Startup-Check        | verweigert den Start (Fail-Fast) |

### Wogegen wir nicht schützen

- **Byzantinisches gateway**: dem gateway wird vertraut. Ein bösartiges
  gateway kann alle Daten korrumpieren.
- **Koordinierte Node-Kollusion**: K bösartige nodes (K-of-N-Schwelle) können
  jedes Objekt rekonstruieren. Reputation ist reaktiv, nicht präventiv.
- **Seitenkanalangriffe auf shard-Transit**: TLS mildert Lauschangriffe; es
  verhindert keine Timing-Angriffe gegen die GF(2⁸)-Tabellen-Lookups (die
  im Bedrohungsmodell von holofs ohnehin öffentlich sind).
