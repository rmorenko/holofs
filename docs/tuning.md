# Load-test tuning guide (P2.4)

Operational cookbook for `holofs-web` under load. Sourced from the
July 2026 soak (24 concurrent encoders, 40-node embedded cluster) and
the P0–P2 review rounds. **Every knob here is an env var read once at
`holofs-web` boot** — no runtime `set_var` will move the needle after
axum starts serving.

The defaults in-tree are chosen so a fresh checkout can run
`holofs-web --storage /tmp/holofs-play` on a laptop and behave. Once
you push past ~1000 objects, ~10 nodes, or non-toy PUT bursts, the
sections below tell you which axis to open up.

## Env var reference

Grouped by concern. Defaults marked with **★** are what `holofs-web`
uses if the env var is unset.

### Backpressure (HTTP handler concurrency)

| Var | Default | Effect |
|-----|---------|--------|
| `HOLOFS_MEDIUM_CONCURRENCY` | ★ 64 | Concurrent MEDIUM-bucket handlers (`get_object`, ingest, most reads). Semaphore-gated at boot; overflow returns 503 with `Retry-After`. |
| `HOLOFS_LONG_CONCURRENCY` | ★ 24 | Concurrent LONG-bucket handlers (`semantic_search`, `similar_to`, `spotlight`). These walk the whole catalog + fan out to nodes; too many in parallel serialise on the shared shard cache. |
| `HOLOFS_ENCODE_CONCURRENCY` | ★ 8 | RLNC+DWT encoder workers behind the async-ingest 202 fast-path. Pure CPU — 1× cores is the ceiling; oversubscribing thrashes L2. |
| `HOLOFS_ENCODE_QUEUE_MAX` | ★ 8 × encode_concurrency (≥ 32) | Async-ingest intake ceiling. When live encodes reach this, PUT returns 503 instead of 202. Prevents the queue from growing without bound. |

### Node-side storage

| Var | Default | Effect |
|-----|---------|--------|
| `HOLOFS_NODE_FSYNC` | ★ `1` (on) | Per-shard fsync barrier. **Setting `0` gives ~13× encoder throughput** but a crash between fsync intervals can lose recent shards on that node. RLNC replication across N nodes tolerates the loss of a few unsynced shards, so most production workloads are fine with `0`. Set to `1` when your SLA can't tolerate any per-node dataloss. |
| `HOLOFS_NODE_FLUSH_INTERVAL_MS` | ★ 5 | Group-commit flusher tick period. Silently clamped to ≥ 1 ms. Lower = tighter durability window under `HOLOFS_NODE_FSYNC=0`; higher = fewer fsyncs at the cost of a bigger rollback window on crash. |
| `HOLOFS_AT_REST_ENC` | ★ off | Enable envelope AES-256-GCM at-rest encryption. Adds ~10-15 % per-shard write overhead. See `operations.md` §5.9 for the KEK sources. |
| `HOLOFS_AT_REST_KEK_SOURCE` | `identity` | `identity` / `file` / `env`. Combined with `HOLOFS_AT_REST_KEK_PATH` or `HOLOFS_AT_REST_KEK_HEX`. Only consulted when `HOLOFS_AT_REST_ENC=1`. |

### WAL compaction

| Var | Default | Effect |
|-----|---------|--------|
| `HOLOFS_WAL_COMPACT_INTERVAL_SECS` | ★ 300 (5 min) | Background compactor tick period. `0` disables entirely (segments accumulate). |
| `HOLOFS_WAL_COMPACT_RATIO` | ★ 2.0 | Fire compaction early when `wal_disk_bytes / live_bytes` exceeds this. Higher = compact less often, more churn tolerated. |
| `HOLOFS_WAL_COMPACT_MIN_BYTES` | ★ 4 MiB | Floor on disk usage before either trigger fires. Prevents busy-looping compaction on tiny logs where compacted output can be as large as the input. |

### Background loops

| Var | Default | Effect |
|-----|---------|--------|
| `HOLOFS_MONITOR_INTERVAL` | ★ 1 s | Health-monitor tick period (Ping every node, flip `admin_kills`). |
| `HOLOFS_AUDIT_INTERVAL` | ★ 15 s | PoR auditor tick period (per-shard sample audit). |
| `HOLOFS_SCRUB_INTERVAL` | ★ 60 s | Background scrub cadence. `0` disables scrub (auto-repair-on-read still active). |
| `HOLOFS_CAPACITY_POLL_INTERVAL_SECS` | ★ 60 | P1.4b capacity poller cadence. Floor 5 s. |
| `HOLOFS_REBALANCE_INTERVAL_SECS` | ★ 300 (5 min) | P1.4b auto-rebalance daemon cadence. `0` disables. |
| `HOLOFS_REBALANCE_TRIGGER_PCT` | ★ 85 | Used-pct threshold that triggers a rebalance round. |
| `HOLOFS_REBALANCE_COLD_CEILING_PCT` | ★ 60 | Coldest-node ceiling — if the emptiest known node is above this, rebalance skips the round rather than shuffling between two nearly-full nodes. |

### Networking + limits

| Var | Default | Effect |
|-----|---------|--------|
| `HOLOFS_POOL_PER_NODE` | ★ 16 | Max concurrent TCP connections per node in the RPC pool. |
| `HOLOFS_POOL_IDLE_SECS` | ★ 90 | Pool connection idle timeout. |
| `HOLOFS_POOL_DISABLE` | ★ off | Disable connection pooling (one-shot TCP per RPC). Emergency escape hatch — enables per-RPC TLS handshake cost. |
| `HOLOFS_RPC_TIMEOUT_MS` | ★ 30_000 | Per-RPC read/write deadline. |
| `HOLOFS_RATE_LIMIT_RPS_PER_IP` | ★ off | Per-client-IP request/sec ceiling. `0` disables entirely. |
| `HOLOFS_RATE_LIMIT_BURST` | ★ 2 × rps | Token-bucket burst size. |
| `HOLOFS_UPLOAD_MAX_SIZE` | ★ 50 MiB | Ceiling on any single PUT body. |

### Observability

| Var | Default | Effect |
|-----|---------|--------|
| `HOLOFS_OTLP_ENDPOINT` | unset | OTLP/gRPC collector endpoint. Empty ⇒ tracing stays local-stdout. |
| `HOLOFS_OTLP_SERVICE_NAME` | ★ `holofs-web` | `service.name` resource attribute on emitted spans. |
| `HOLOFS_AUDIT_LOG` | unset | Path to admin audit-log JSONL. Empty ⇒ log to stdout. |

## Workload profiles

### Small dev / demo (3-10 nodes, one host)

Nothing to change. Boot with the defaults — the WAL + async-ingest
combination absorbs a modest workload without any env tweaking. If
you're seeing 503 `Retry-After` on burst PUTs, bump
`HOLOFS_MEDIUM_CONCURRENCY=128` before touching anything else.

### Medium prod (10-40 nodes, moderate ingest)

Recommended baseline:

```
HOLOFS_NODE_FSYNC=0                     # 13× encoder throughput
HOLOFS_ENCODE_CONCURRENCY=16            # up from 8 if you have ≥ 16 cores
HOLOFS_ENCODE_QUEUE_MAX=128
HOLOFS_MEDIUM_CONCURRENCY=128
HOLOFS_LONG_CONCURRENCY=32
HOLOFS_WAL_COMPACT_INTERVAL_SECS=300
HOLOFS_SCRUB_INTERVAL=120               # halve the default read-repair cadence
```

Set `HOLOFS_OTLP_ENDPOINT` to your collector before you touch
anything else. Distributed tracing under P1.5 pays for itself the
first time you have to explain a slow PUT.

### Write-heavy ingest (bulk import, encoder-bound)

RLNC over GF(2⁸) is the hot path. Prioritise CPU:

```
HOLOFS_NODE_FSYNC=0
HOLOFS_ENCODE_CONCURRENCY=<physical_cores>
HOLOFS_ENCODE_QUEUE_MAX=<encode_concurrency * 8>
HOLOFS_MEDIUM_CONCURRENCY=<encode_queue_max>
HOLOFS_WAL_COMPACT_INTERVAL_SECS=60     # rotate hotter to keep boot replay bounded
HOLOFS_WAL_COMPACT_RATIO=1.5            # be more aggressive under sustained writes
HOLOFS_NODE_FLUSH_INTERVAL_MS=10        # coarser fsync batches
```

If soak reports p50 encode time climbing after the first hour,
`HOLOFS_WAL_COMPACT_INTERVAL_SECS` is your first suspect —
un-compacted logs slow every boot and every scan.

### Long-term storage / mostly-read (archival)

Bias for durability + capacity churn:

```
HOLOFS_NODE_FSYNC=1                     # trade throughput for zero-loss on crash
HOLOFS_SCRUB_INTERVAL=30                # find corruption before the operator does
HOLOFS_CAPACITY_POLL_INTERVAL_SECS=30   # tighter observability
HOLOFS_REBALANCE_INTERVAL_SECS=600      # slower reaction — archival load is smooth
HOLOFS_REBALANCE_TRIGGER_PCT=75         # trigger earlier since growth is monotonic
HOLOFS_LONG_CONCURRENCY=64              # semantic-search + similar-to bursts absorb better
```

## Measurement workflow

**Rule zero: measure before tuning.** The defaults are optimised for
laptop dev; production workloads move the optimum in
counter-intuitive directions.

1. **Baseline with defaults.** Boot `holofs-web` with no `HOLOFS_*`
   overrides, run `holofs-soak` for ≥ 30 min. Save the resulting
   `metrics.jsonl` — this is your "before".
2. **Change one axis at a time.** Adjust one env var, restart, re-
   run soak. Compare the two `metrics.jsonl` snapshots via
   `holofs-soak-report`. Two-at-a-time changes make the ledger
   impossible to interpret.
3. **Watch the four counters that matter under load:**
   - `holofs_put_cpu_nanoseconds_sum` / `holofs_put_count_total` —
     per-PUT CPU wall time. Grows when `HOLOFS_ENCODE_CONCURRENCY`
     is too high for the box.
   - `holofs_put_fanout_nanoseconds_sum` / same — network wall time.
     Grows when the node pool is exhausted or a node is slow.
   - `holofs_encode_failed_total` — non-zero under async-ingest =
     cluster degraded (nodes down, disk full, WAL corruption).
   - `holofs_backpressure_rejected_total{bucket="medium"}` — 503s
     from the backpressure semaphore. Non-zero under sustained
     traffic means `HOLOFS_MEDIUM_CONCURRENCY` is too low; under
     bursts means `HOLOFS_ENCODE_QUEUE_MAX` is too low.
4. **Cross-check `/api/capacity`** during the soak. Under P1.4b the
   auto-rebalance daemon migrates data proactively; if soak reports
   growing skew you've hit either an env misconfiguration or a
   cluster topology mismatch (odd zone layout, dead node whitelist
   not synced).
5. **Save the config file, not the env dump.** Boot params surface
   in `holofs-web` logs at startup — grep for `N3 backpressure caps
   applied` to confirm what the process actually loaded.

## Anti-patterns

Common misconfigurations that look like "tuning" but degrade
throughput:

- **Setting `HOLOFS_ENCODE_CONCURRENCY` above physical core count.**
  RLNC saturates `xor` throughput per core; hyperthreads help
  ~15 %. Setting it to `cores × 2` trashes L2, drops throughput
  20–40 %.
- **Setting `HOLOFS_MEDIUM_CONCURRENCY` above 512 on non-huge
  boxes.** Each concurrent handler holds a tokio task + potentially
  a shard-cache entry; 512+ tasks hurt scheduler latency on <16-core
  machines.
- **Disabling WAL compaction (`_INTERVAL_SECS=0`) in prod.** The
  operator "just for now" that never comes back — the log grows
  monotonically and every boot re-plays it end to end. On a busy
  cluster this makes recovery times unbounded.
- **Setting `HOLOFS_NODE_FSYNC=0` on a single-node deployment.** With
  replication ≥ 2 losing a few unsynced shards is recoverable via
  auto-repair; with 1 node it's straight-up dataloss on crash.
- **Cranking `HOLOFS_POOL_PER_NODE` past ~64.** More than 64
  concurrent TCP connections to one node causes the node's tokio
  scheduler to spend most of its time on connection acceptance
  rather than shard I/O. Adjust `HOLOFS_MEDIUM_CONCURRENCY` first;
  the pool ceiling should be roughly 25–40 % of that.
- **Turning `HOLOFS_SCRUB_INTERVAL=0` "for perf".** Scrub is
  read-only against the cluster; disabling it saves nothing
  measurable but denies you the early-warning signal for corrupted
  shards. Corruption you find via read-path 500s is corruption you
  found *late*.
- **Multiple `HOLOFS_OTLP_ENDPOINT` overrides at boot without
  restarting.** Runtime `set_var` is a no-op — the tracing pipeline
  reads once at boot. Same for every other var here.

## When to expect diminishing returns

The July 2026 soak plateau: on a 40-node, 24-encoder,
`HOLOFS_NODE_FSYNC=0` cluster the encoder throughput topped out
around ~6.5 encodes/s (from a ~0.5 baseline with fsync-under-mutex).
Beyond that point the bottleneck moved from CPU/fsync to the
gateway's persist path — the group-commit ticket dance in
`persist_catalog` becomes the serialising step. If your soak is
sitting near that number with all knobs open, the next win is either
gateway sharding (out of scope for 1.x) or a `redb`-catalog-store
tuning pass (out of scope for the current release).
