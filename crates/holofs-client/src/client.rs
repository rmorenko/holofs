//! Distributed client: PUT/GET/REPAIR over a cluster of nodes.
//!
//! Nodes are dumb stores. The client owns the [`holofs_model::manifest::Manifest`],
//! computes placement via [`holofs_model::placement`], and drives all
//! encoding / decoding itself.

use std::collections::HashSet;
use std::io;

use crate::pool;

use holofs_codec::text_codec::{assemble_text_with_holes, split_text_into_k_chunks};
use holofs_core::gf::Gf;
use holofs_core::merkle::{data_cid, merkle_root, shard_hash, Hash};
use holofs_core::repair::{mix_donors, RepairStats};
use holofs_core::rlnc::{decode_layer, decode_layer_with_holes, encode_layer, Shard};
use holofs_core::rng::Rng;
use holofs_core::transform::{haar_forward, haar_inverse};
use holofs_core::transform::{haar_forward_1d, haar_inverse_1d};
use holofs_model::manifest::{Manifest, ObjectEncoding};
use holofs_model::placement::{place_replicas, ShardKey};
use holofs_storage::identity::{fresh_nonce, verify_challenge, PubKey};
use holofs_wire::{read_frame, write_frame, Request, Response};

#[derive(Debug)]
pub enum ClientError {
    Io(io::Error),
    /// The remote node accepted our request and explicitly reported a
    /// failure (`Response::Error`). The string is the node's message.
    /// This is the "expected" failure mode for things like missing
    /// shards on a peer.
    RemoteError(String),
    /// The remote responded with a `Response` variant the caller
    /// didn't expect (e.g. asking for a Shard and getting an Ack).
    /// Indicates a wire-protocol mismatch, not a remote-side failure.
    UnexpectedResponse {
        expected: &'static str,
        got: String,
    },
    LayerLost {
        channel: u8,
        layer: u8,
    },
    /// Two manifests handed to a mix/blend operation don't agree on
    /// the fields the DWT-aware decoder needs to interleave their
    /// shards (dimensions, channel count, layer count, k, etc.).
    Incompatible(String),
    /// Placement was asked to pick a node from an empty live set —
    /// the cluster is fully down or hasn't been discovered yet.
    /// Callers should treat this as transient cluster degradation,
    /// not a per-object failure.
    NoLiveNodes,
}

impl From<io::Error> for ClientError {
    fn from(e: io::Error) -> Self {
        ClientError::Io(e)
    }
}
impl From<holofs_model::placement::NoLiveNodes> for ClientError {
    fn from(_: holofs_model::placement::NoLiveNodes) -> Self {
        ClientError::NoLiveNodes
    }
}
impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Io(e) => write!(f, "io: {e}"),
            ClientError::RemoteError(s) => write!(f, "remote: {s}"),
            ClientError::UnexpectedResponse { expected, got } => {
                write!(f, "unexpected response: expected {expected}, got {got}")
            }
            ClientError::LayerLost { channel, layer } => {
                write!(f, "layer (c={channel}, l={layer}) is not decodable")
            }
            ClientError::Incompatible(s) => write!(f, "incompatible manifests: {s}"),
            ClientError::NoLiveNodes => write!(f, "no live nodes in cluster"),
        }
    }
}
impl std::error::Error for ClientError {}

impl ClientError {
    /// True iff the error came from a timed-out RPC. Lets callers
    /// (and our /metrics counters) distinguish "node slow / hung"
    /// from other IO trouble without re-parsing strings.
    pub fn is_timeout(&self) -> bool {
        matches!(self, ClientError::Io(e) if e.kind() == io::ErrorKind::TimedOut)
    }
}

/// List of "live" node indices in the cluster (within `manifest.nodes`).
pub type LiveNodes = Vec<usize>;

// === RPC helper ============================================================

/// Per-RPC overall budget. Without this a flapping node can wedge
/// any caller for minutes because `read_frame` will happily await
/// forever. The default (8 s) is generous enough for the heaviest
/// PutBatch / Gather operations we ship today, but caps the
/// damage from a half-dead peer at one user-visible 8 s instead
/// of a TCP-stack timeout (typically 75 s+ on Linux, 60+ on macOS).
///
/// Override via `HOLOFS_RPC_TIMEOUT_MS`. Set to 0 to disable.
fn rpc_timeout() -> Option<std::time::Duration> {
    static CACHED: std::sync::OnceLock<Option<std::time::Duration>> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        let ms: u64 = std::env::var("HOLOFS_RPC_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(8_000);
        if ms == 0 {
            None
        } else {
            Some(std::time::Duration::from_millis(ms))
        }
    })
}

async fn rpc(addr: &str, req: Request) -> io::Result<Response> {
    let encoded = req.encode();
    match rpc_attempt(addr, &encoded).await {
        Ok(resp) => Ok(resp),
        // A pooled connection might have been silently closed by the
        // peer or the OS while idle; the first IO on it then surfaces
        // EPIPE / ECONNRESET / UnexpectedEof. Every wire op the
        // protocol exposes is idempotent at the application layer
        // (PUT/Audit/Gather/Purge/PutBatch all key on shard hash, Ping
        // is harmless), so a single retry against a freshly-dialed
        // connection is safe and lets the keepalive pool degrade
        // gracefully without bubbling spurious failures up to callers.
        // The same applies to a timeout — a hung peer might recover
        // before the second attempt, OR the second attempt gets a
        // fresh socket (pooled stream was poisoned on the timeout
        // path) and reaches a different ephemeral port quickly.
        Err(e) if is_likely_transient(&e) => rpc_attempt_fresh(addr, &encoded).await,
        Err(e) => Err(e),
    }
}

async fn rpc_attempt(addr: &str, encoded_req: &[u8]) -> io::Result<Response> {
    let mut s = pool::acquire(addr).await?;
    let response = rpc_over_stream(&mut s, addr, encoded_req).await;
    if response.is_ok() {
        s.mark_clean();
    }
    response
}

/// Retry path: dial a fresh connection instead of grabbing the next
/// idle sibling from the pool. Without this the LIFO queue would hand
/// out another same-vintage keepalive socket that is probably in the
/// same "peer dead but OS hasn't reaped it yet" state as the one that
/// just failed, causing spurious back-to-back errors.
async fn rpc_attempt_fresh(addr: &str, encoded_req: &[u8]) -> io::Result<Response> {
    let mut s = pool::acquire_fresh(addr).await?;
    let response = rpc_over_stream(&mut s, addr, encoded_req).await;
    // Only a fully-decoded response on a clean frame boundary is
    // safe to recycle. `pool::Pooled` defaults to "discard on
    // drop" — including any cancellation from an outer
    // `tokio::time::timeout` or `select!` — so all we need to do
    // here is opt in explicitly on the happy path. Every error
    // (timeout, mid-frame IO, decode desync) falls through and
    // the stream is closed.
    if response.is_ok() {
        s.mark_clean();
    }
    response
}

/// Framed write + read + decode over an already-acquired pooled
/// stream. Split out of `rpc_attempt` so the timeout branch can
/// drop the inner future without leaving `&mut s` borrowed for
/// the ambient poison-on-Err handling above.
async fn rpc_over_stream(
    s: &mut pool::Pooled,
    addr: &str,
    encoded_req: &[u8],
) -> io::Result<Response> {
    let inner = async {
        write_frame(s, encoded_req).await?;
        let buf = read_frame(s).await?;
        Response::decode(&buf)
    };
    match rpc_timeout() {
        Some(timeout) => match tokio::time::timeout(timeout, inner).await {
            Ok(r) => r,
            Err(_) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("rpc to {addr} exceeded {:?}", timeout),
            )),
        },
        None => inner.await,
    }
}

fn is_likely_transient(e: &io::Error) -> bool {
    use io::ErrorKind::*;
    matches!(
        e.kind(),
        UnexpectedEof
            | BrokenPipe
            | ConnectionReset
            | ConnectionAborted
            | NotConnected
            | TimedOut
    )
}

// === Encode and dispatch ===================================================

/// Process-wide counters split PUT wall time into the RLNC/DWT CPU phase and
/// the network fan-out phase. Two `AtomicU64::fetch_add` per PUT — far below
/// any file/lock instrumentation we tried. Read via `Gateway::api_stats` /
/// `/health` for honest cpu-vs-fanout ratios under concurrent load.
pub static PUT_CPU_NS_SUM: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static PUT_FANOUT_NS_SUM: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static PUT_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Deterministically encode channels (RNG seeded by data_cid), dispatch shards
/// across nodes, compute hashes and the Merkle root. The manifest is mutated:
/// `data_cid`, `object_id`, `merkle_root`, and `shard_hashes` are written into
/// it. Returns the updated manifest.
///
/// Deduplication: each node keeps a HashMap keyed by shard hash, so a repeated
/// PUT of the same object from the same client does not produce new entries.
pub async fn put_object(
    gf: &Gf,
    manifest: &mut Manifest,
    live: &LiveNodes,
    channels: &[Vec<f32>],
) -> Result<(), ClientError> {
    let w = manifest.width as usize;
    let h = manifest.height as usize;
    let levels = manifest.levels as usize;
    let nlayers = manifest.nlayers as usize;

    // 1. Source CID — determined only by contents and base parameters. We
    //    derive object_id (first 8 bytes BE) and the RNG seed from it.
    let cid = data_cid(channels, w, h, manifest.levels, manifest.k);
    manifest.data_cid = cid;
    manifest.object_id = u64::from_be_bytes(cid[0..8].try_into().unwrap());
    let seed = u64::from_be_bytes(cid[8..16].try_into().unwrap());
    let mut rng = Rng::new(seed);

    // 2. Encode + hash all shards, staging one dispatch queue per node.
    //    The pre-P1 hot loop `for shard { rpc(node, req).await? }` sent
    //    every shard sequentially — on a 4-node topology with ~480
    //    total shards per PUT that was 480 × TCP-round-trip in series,
    //    dominating the 46 s p50 the soak study measured. Now we
    //    encode fully, batch by target node, and fan out with
    //    `try_join_all` so the wire step scales with pool depth
    //    instead of shard count.
    let mut shard_hashes: Vec<Vec<Vec<Hash>>> =
        vec![vec![Vec::new(); nlayers]; manifest.channels as usize];
    let mut leaves_flat: Vec<Hash> = Vec::new();
    let mut per_node_reqs: Vec<Vec<Request>> = vec![Vec::new(); manifest.nodes.len()];

    let t_cpu = std::time::Instant::now();
    for c in 0..manifest.channels as usize {
        let mut plane = channels[c].clone();
        haar_forward(&mut plane, w, h, levels);
        for l in 0..nlayers {
            let positions = &manifest.layer_positions[l];
            let mut bytes = Vec::with_capacity(positions.len() * 4);
            for &p in positions {
                bytes.extend_from_slice(&plane[p as usize].to_le_bytes());
            }
            let n = manifest.n_per_layer[l] as usize;
            let (sl, shards) = encode_layer(gf, &bytes, n, &mut rng);
            assert_eq!(
                sl, manifest.sym_len[l] as usize,
                "sym_len in manifest disagrees with the actual encoding"
            );

            for (idx, shard) in shards.into_iter().enumerate() {
                let h = shard_hash(&shard);
                shard_hashes[c][l].push(h);
                leaves_flat.push(h);

                let node = manifest.place_shard(c as u8, l as u8, idx as u32, live)?;
                per_node_reqs[node].push(Request::Put {
                    object_id: manifest.object_id,
                    channel: c as u8,
                    layer: l as u8,
                    shard,
                });
            }
        }
    }
    let cpu_ns = t_cpu.elapsed().as_nanos() as u64;

    let t_fanout = std::time::Instant::now();
    fanout_puts(&manifest.nodes, per_node_reqs).await?;
    let fanout_ns = t_fanout.elapsed().as_nanos() as u64;

    use std::sync::atomic::Ordering;
    PUT_CPU_NS_SUM.fetch_add(cpu_ns, Ordering::Relaxed);
    PUT_FANOUT_NS_SUM.fetch_add(fanout_ns, Ordering::Relaxed);
    PUT_COUNT.fetch_add(1, Ordering::Relaxed);

    manifest.shard_hashes = shard_hashes;
    manifest.merkle_root = merkle_root(&leaves_flat);
    Ok(())
}

/// Dispatch a per-node queue of `Request::Put` frames concurrently.
/// One task per node so the fan-out fully utilises the keepalive
/// pool without ballooning tasks per shard. Any single failed RPC
/// aborts the fan-out and surfaces the underlying `ClientError`.
async fn fanout_puts(
    nodes: &[String],
    per_node_reqs: Vec<Vec<Request>>,
) -> Result<(), ClientError> {
    use futures_util::future::try_join_all;

    // Concurrent frames per node. Coalescing already merges consecutive
    // Put frames that share (object_id, channel, layer) into one PutBatch,
    // but the tail — one PutBatch per layer × channel — still totals
    // ~15 frames per PUT on an 8-node cluster. The July 2026 soak study
    // showed each of those frames paying its own group-commit fsync tick
    // on the node (55 ms wal_wait × 15 = 800 ms of the 1.4 s fanout wall).
    // Firing them at once through the keepalive pool lets one WAL tick
    // absorb the whole batch instead of paying it 15 times in series.
    const PER_NODE_INFLIGHT: usize = 8;

    let tasks = per_node_reqs
        .into_iter()
        .enumerate()
        .filter(|(_, reqs)| !reqs.is_empty())
        .map(|(node_idx, reqs)| {
            let addr = nodes[node_idx].clone();
            let batched = coalesce_puts(reqs);
            async move {
                use futures_util::stream::StreamExt;
                let inflight = PER_NODE_INFLIGHT.min(batched.len().max(1));
                let mut stream = futures_util::stream::iter(batched.into_iter().map({
                    let addr = addr.clone();
                    move |req| {
                        let addr = addr.clone();
                        async move { rpc(&addr, req).await }
                    }
                }))
                .buffer_unordered(inflight);
                while let Some(res) = stream.next().await {
                    match res? {
                        Response::Ack => {}
                        Response::Error(msg) => return Err(ClientError::RemoteError(msg)),
                        other => {
                            return Err(ClientError::UnexpectedResponse {
                                expected: "Ack",
                                got: format!("{other:?}"),
                            })
                        }
                    }
                }
                Ok::<(), ClientError>(())
            }
        })
        .collect::<Vec<_>>();
    try_join_all(tasks).await?;
    Ok(())
}

/// Turn a per-node queue of `Request::Put` frames into a mixed
/// queue where every consecutive run sharing `(object_id, channel,
/// layer)` is merged into one `Request::PutBatch`. Non-`Put` frames
/// (or single `Put`s with no run) pass through unchanged.
///
/// Why bother: each node's WAL group-commit flusher runs every
/// ~5 ms and waits at least one full tick per accepted frame. A
/// 12-shard-per-node layer fanout used to cost 12 × ~5 ms of
/// flush waits per node; one `PutBatch` costs one. Under a
/// 24-encoder soak the sequential-Put path had shard fanout
/// dominating encoder wall clock (~1 s per finalise); batching
/// drops it below 200 ms.
fn coalesce_puts(reqs: Vec<Request>) -> Vec<Request> {
    let mut out = Vec::with_capacity(reqs.len());
    let mut run: Option<(u64, u8, u8, Vec<holofs_core::rlnc::Shard>)> = None;
    for req in reqs {
        match req {
            Request::Put {
                object_id,
                channel,
                layer,
                shard,
            } => {
                if let Some((oid, c, l, shards)) = run.as_mut() {
                    if *oid == object_id && *c == channel && *l == layer {
                        shards.push(shard);
                        continue;
                    }
                    // key changed — flush the accumulated run.
                    out.push(Request::PutBatch {
                        object_id: *oid,
                        channel: *c,
                        layer: *l,
                        shards: std::mem::take(shards),
                    });
                }
                run = Some((object_id, channel, layer, vec![shard]));
            }
            other => {
                if let Some((oid, c, l, shards)) = run.take() {
                    out.push(Request::PutBatch {
                        object_id: oid,
                        channel: c,
                        layer: l,
                        shards,
                    });
                }
                out.push(other);
            }
        }
    }
    if let Some((oid, c, l, shards)) = run {
        // A single-shard tail stays as a plain `Put` — no reason to
        // pay the `PutBatch` framing overhead for one shard.
        if shards.len() == 1 {
            let shard = shards.into_iter().next().unwrap();
            out.push(Request::Put {
                object_id: oid,
                channel: c,
                layer: l,
                shard,
            });
        } else {
            out.push(Request::PutBatch {
                object_id: oid,
                channel: c,
                layer: l,
                shards,
            });
        }
    }
    out
}

/// per-block replicated encoder.
///
/// For each `(channel, layer)`, groups the layer's DWT coefficients
/// into contiguous blocks of `block_size` coefficients (last block
/// short as needed), replicates each block to `replication` cluster
/// nodes chosen by HRW, and dispatches them via
/// [`Request::PutBatch`] — one batch per `(channel, layer, node)`.
///
/// Manifest mutations (mirroring [`put_object`]): `data_cid`,
/// `object_id`, `merkle_root`, and `shard_hashes` land in the
/// manifest. Additionally this function sets:
///
///   * `n_per_layer[l] = ceil(layer_positions[l].len() / block_size)`
///     — number of blocks in the layer.
///   * `sym_len[l] = block_size * 4` — bytes per full block payload
///     (last block may be shorter but the manifest carries the max).
///   * `encoding = ObjectEncoding::Replicated { replication,
///     block_size }`.
///
/// Sizing rule: total shards dispatched per PUT ≈
/// `channels × (Σ layer_lengths / block_size) × replication`.
/// Pick `block_size` conservatively — 64 for a 512×512 RGB image
/// yields ~36 k shards, well within the disk store's budget, while
/// still keeping the smallest ROI (single 8×8 tile) at ≤4 blocks
/// per touched layer.
pub async fn put_object_replicated_blocks(
    manifest: &mut Manifest,
    live: &LiveNodes,
    channels: &[Vec<f32>],
    block_size: u32,
    replication: u8,
) -> Result<(), ClientError> {
    assert!(block_size >= 1, "block_size must be >= 1");
    assert!(replication >= 1, "replication must be >= 1");
    let w = manifest.width as usize;
    let h = manifest.height as usize;
    let levels = manifest.levels as usize;
    let nlayers = manifest.nlayers as usize;
    let ch = manifest.channels as usize;

    // 1. Deterministic object identity — same as put_object.
    let cid = data_cid(channels, w, h, manifest.levels, manifest.k);
    manifest.data_cid = cid;
    manifest.object_id = u64::from_be_bytes(cid[0..8].try_into().unwrap());

    // 2. Set the encoding-driven manifest fields *before* the encode
    //    loop so we can index into `n_per_layer` uniformly.
    let bs = block_size as usize;
    let mut n_per_layer: Vec<u32> = Vec::with_capacity(nlayers);
    for l in 0..nlayers {
        let n = manifest.layer_positions[l].len();
        n_per_layer.push(n.div_ceil(bs) as u32);
    }
    manifest.n_per_layer = n_per_layer;
    manifest.sym_len = vec![block_size * 4; nlayers];
    manifest.encoding = ObjectEncoding::Replicated {
        replication,
        block_size,
    };

    // 3. Encode each channel, group per-(c, l, node) shard sets so
    //    every node receives one PutBatch per (c, l) — RPC count is
    //    channels * nlayers * min(replication, live.len()).
    let mut shard_hashes: Vec<Vec<Vec<Hash>>> = vec![vec![Vec::new(); nlayers]; ch];
    let mut leaves_flat: Vec<Hash> = Vec::new();

    for c in 0..ch {
        let mut plane = channels[c].clone();
        haar_forward(&mut plane, w, h, levels);
        for l in 0..nlayers {
            let positions = &manifest.layer_positions[l];
            let n_blocks = manifest.n_per_layer[l] as usize;
            // Build shards + placements first.
            // batches[node] -> Vec<Shard> to send to that node.
            let mut batches: std::collections::HashMap<usize, Vec<Shard>> =
                std::collections::HashMap::new();
            let mut layer_hashes: Vec<Hash> = Vec::with_capacity(n_blocks);
            for b in 0..n_blocks {
                let start = b * bs;
                let end = (start + bs).min(positions.len());
                let mut payload = Vec::with_capacity((end - start) * 4);
                for &p in &positions[start..end] {
                    payload.extend_from_slice(&plane[p as usize].to_le_bytes());
                }
                // Replicated shards carry no RLNC coefficient
                // vector — the payload IS the block's bytes and
                // every replica is byte-identical.
                let shard = Shard {
                    coeffs: Vec::new(),
                    payload,
                };
                let h = shard_hash(&shard);
                layer_hashes.push(h);
                leaves_flat.push(h);
                let key = ShardKey {
                    object_id: manifest.object_id,
                    channel: c as u8,
                    layer: l as u8,
                    shard_idx: b as u32,
                };
                for node in place_replicas(key, replication, live)? {
                    batches.entry(node).or_default().push(shard.clone());
                }
            }
            shard_hashes[c][l] = layer_hashes;

            // Dispatch one PutBatch per node covering all blocks
            // this node holds for (c, l). Parallel via
            // `join_all` — on 40 nodes with R=3 this collapses
            // ~40 sequential RPCs into one round-trip, taking
            // the 512×512 PUT under the MEDIUM-bucket timeout
            //sequential dispatch made
            // large clusters + Replicated encoding 504 out).
            let dispatches = batches.into_iter().filter_map(|(node, shards)| {
                let addr = manifest.nodes.get(node)?.clone();
                let req = Request::PutBatch {
                    object_id: manifest.object_id,
                    channel: c as u8,
                    layer: l as u8,
                    shards,
                };
                Some(async move { rpc(&addr, req).await })
            });
            for result in futures_util::future::join_all(dispatches).await {
                match result? {
                    Response::Ack => {}
                    Response::Error(msg) => return Err(ClientError::RemoteError(msg)),
                    other => {
                        return Err(ClientError::UnexpectedResponse {
                            expected: "Ack from PutBatch",
                            got: format!("{other:?}"),
                        })
                    }
                }
            }
        }
    }

    manifest.shard_hashes = shard_hashes;
    manifest.merkle_root = merkle_root(&leaves_flat);
    Ok(())
}

// === Gather and decode =====================================================

/// Progressive read: decode only layers `0..=max_layer`; anything above stays
/// zero in the DWT domain. After inverse-DWT this yields a coarse approximation
/// of the object — an instant preview for a fraction of the bandwidth.
///
/// Returns (channels, bandwidth_bytes). When `max_layer >= nlayers - 1` this
/// is equivalent to a full [`get_object`].
pub async fn get_object_up_to_layer(
    gf: &Gf,
    manifest: &Manifest,
    live: &LiveNodes,
    max_layer: u8,
) -> Result<(Vec<Vec<f32>>, u64), ClientError> {
    use futures_util::stream::StreamExt;

    let w = manifest.width as usize;
    let h = manifest.height as usize;
    let levels = manifest.levels as usize;
    let nlayers = manifest.nlayers as usize;
    let channels = manifest.channels as usize;

    // Fan out (channel, layer) fetches — combined with the parallel
    // `gather_layer` (per-node fan-out) this collapses the pre-fix
    // O(C·L·N) sequential RTT into a small number of ~max-RTT hops.
    //
    // Concurrency cap: unbounded `try_join_all` blew up total in-flight
    // RPCs to `C·L·N` per GET (RGB × 7 layers × 8 nodes = 168 per
    // request). Under 50 workers that saturated the connection pool
    // and the LONG backpressure semaphore, sending the 3-min soak
    // error rate from 9.8 % to 20 % on 503 rejects. Capping at 4
    // in-flight (channel, layer) fetches at a time still hides most
    // network latency (each fetch itself parallelises over N nodes
    // internally) without stampeding the pool.
    const CL_INFLIGHT: usize = 4;

    let mut targets: Vec<(usize, usize)> = Vec::with_capacity(channels * nlayers);
    for c in 0..channels {
        for l in 0..nlayers {
            if (l as u8) <= max_layer {
                targets.push((c, l));
            }
        }
    }
    let mut fetch_stream = futures_util::stream::iter(targets.into_iter().map(|(c, l)| async move {
        let raw = gather_layer(manifest, live, c as u8, l as u8).await?;
        Ok::<(usize, usize, Vec<Shard>), ClientError>((c, l, raw))
    }))
    .buffer_unordered(CL_INFLIGHT);
    let mut fetched: Vec<(usize, usize, Vec<Shard>)> = Vec::new();
    while let Some(res) = fetch_stream.next().await {
        fetched.push(res?);
    }

    // CPU chunk stays sequential — decode + IDWT are CPU-bound and
    // running them in parallel wouldn't help on the single tokio
    // worker anyway. Sort so each channel's layers are processed
    // in order (haar_inverse needs L0 before it can lay down L1).
    let mut per_channel: Vec<Vec<(usize, Vec<Shard>)>> = vec![Vec::new(); channels];
    for (c, l, raw) in fetched {
        per_channel[c].push((l, raw));
    }
    let mut out = vec![vec![0f32; w * h]; channels];
    let mut bytes_used: u64 = 0;
    for c in 0..channels {
        let mut plane = vec![0f32; w * h];
        // Layers already come in ascending order (loop above emitted
        // them that way), but sort defensively — a future rewrite
        // that changes emission order (e.g. buffer_unordered) must
        // stay correct.
        per_channel[c].sort_by_key(|(l, _)| *l);
        for (l, raw) in per_channel[c].drain(..) {
            let expected: HashSet<Hash> = manifest.shard_hashes[c][l].iter().copied().collect();
            let verified: Vec<Shard> = raw
                .into_iter()
                .filter(|s| expected.contains(&shard_hash(s)))
                .collect();
            let sl = manifest.sym_len[l] as usize;
            bytes_used += verified.len() as u64 * (manifest.k as u64 + sl as u64);
            let refs: Vec<&Shard> = verified.iter().collect();
            let bytes = decode_layer(gf, &refs, sl).ok_or(ClientError::LayerLost {
                channel: c as u8,
                layer: l as u8,
            })?;
            for (idx, &p) in manifest.layer_positions[l].iter().enumerate() {
                let off = idx * 4;
                let arr = [bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]];
                plane[p as usize] = f32::from_le_bytes(arr);
            }
        }
        haar_inverse(&mut plane, w, h, levels);
        out[c] = plane;
    }
    Ok((out, bytes_used))
}

/// variant of [`get_object_up_to_layer`] that decodes
/// every layer normally but **places only those coefficients whose
/// flat plane index is in `allowed`**; positions outside the mask
/// stay at zero before the inverse Haar runs.
///
/// Result: pixels inside the spatial ROI covered by `allowed` get
/// the full-detail reconstruction; pixels outside collapse toward
/// zero (i.e., black on RGB output). The mask is built upstream from
/// `holofs_core::transform::spatial_to_dwt_positions` so the caller
/// just hands an `HashSet<usize>` of DWT plane indices.
///
/// **Bandwidth note:** this saves NO network bytes vs the plain
/// `get_object_up_to_layer` — RLNC encodes the whole layer's
/// coefficient set into every shard, so you still need K shards per
/// layer to decode anything. The win is in the spatial reconstruction
/// (sharp ROI, dark elsewhere) — the visible alternative to the
/// "two-pass spatial composite" of .
pub async fn get_object_with_coeff_mask(
    gf: &Gf,
    manifest: &Manifest,
    live: &LiveNodes,
    allowed: &HashSet<usize>,
) -> Result<(Vec<Vec<f32>>, u64), ClientError> {
    let w = manifest.width as usize;
    let h = manifest.height as usize;
    let levels = manifest.levels as usize;
    let nlayers = manifest.nlayers as usize;
    let mut out = vec![vec![0f32; w * h]; manifest.channels as usize];
    let mut bytes_used: u64 = 0;

    for c in 0..manifest.channels as usize {
        let mut plane = vec![0f32; w * h];
        for l in 0..nlayers {
            let raw = gather_layer(manifest, live, c as u8, l as u8).await?;
            let expected: HashSet<Hash> = manifest.shard_hashes[c][l].iter().copied().collect();
            let verified: Vec<Shard> = raw
                .into_iter()
                .filter(|s| expected.contains(&shard_hash(s)))
                .collect();
            let sl = manifest.sym_len[l] as usize;
            bytes_used += verified.len() as u64 * (manifest.k as u64 + sl as u64);
            let refs: Vec<&Shard> = verified.iter().collect();
            let bytes = decode_layer(gf, &refs, sl).ok_or(ClientError::LayerLost {
                channel: c as u8,
                layer: l as u8,
            })?;
            for (idx, &p) in manifest.layer_positions[l].iter().enumerate() {
                let pos = p as usize;
                if !allowed.contains(&pos) {
                    continue;
                }
                let off = idx * 4;
                let arr = [bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]];
                plane[pos] = f32::from_le_bytes(arr);
            }
        }
        haar_inverse(&mut plane, w, h, levels);
        out[c] = plane;
    }
    Ok((out, bytes_used))
}

/// wavelet mix. For each `(channel, layer)`, pull shards from
/// manifest `a` when `layer <= split`, from manifest `b` otherwise. The
/// IDWT runs on the hybrid coefficient plane so structure (low layers)
/// comes from one source and detail (high layers) from the other.
///
/// Both manifests must agree on the fields the decoder interleaves
/// (`width`, `height`, `channels`, `nlayers`, `levels`, `k`, plus the
/// per-layer `sym_len` and `layer_positions`). Mismatch returns
/// `ClientError::Incompatible` without touching the network.
pub async fn mix_images_at_split(
    gf: &Gf,
    a: &Manifest,
    b: &Manifest,
    live: &LiveNodes,
    split: u8,
) -> Result<(Vec<Vec<f32>>, u64), ClientError> {
    if a.width != b.width
        || a.height != b.height
        || a.channels != b.channels
        || a.nlayers != b.nlayers
        || a.levels != b.levels
        || a.k != b.k
    {
        return Err(ClientError::Incompatible(format!(
            "shape mismatch: a={}×{} ch={} layers={} levels={} k={}, \
             b={}×{} ch={} layers={} levels={} k={}",
            a.width, a.height, a.channels, a.nlayers, a.levels, a.k,
            b.width, b.height, b.channels, b.nlayers, b.levels, b.k,
        )));
    }
    if a.sym_len != b.sym_len {
        return Err(ClientError::Incompatible(
            "per-layer sym_len differs between manifests".into(),
        ));
    }
    if a.layer_positions != b.layer_positions {
        return Err(ClientError::Incompatible(
            "per-layer position maps differ between manifests".into(),
        ));
    }

    let w = a.width as usize;
    let h = a.height as usize;
    let levels = a.levels as usize;
    let nlayers = a.nlayers as usize;
    let mut out = vec![vec![0f32; w * h]; a.channels as usize];
    let mut bytes_used: u64 = 0;

    for c in 0..a.channels as usize {
        let mut plane = vec![0f32; w * h];
        for l in 0..nlayers {
            // Which source owns this layer? Layers `0..=split` from `a`,
            // strictly higher from `b`. Each layer's hashes / nodes /
            // sym_len are taken from the chosen manifest.
            let from = if (l as u8) <= split { a } else { b };
            let raw = gather_layer(from, live, c as u8, l as u8).await?;
            let expected: HashSet<Hash> = from.shard_hashes[c][l].iter().copied().collect();
            let verified: Vec<Shard> = raw
                .into_iter()
                .filter(|s| expected.contains(&shard_hash(s)))
                .collect();
            let sl = from.sym_len[l] as usize;
            bytes_used += verified.len() as u64 * (from.k as u64 + sl as u64);
            let refs: Vec<&Shard> = verified.iter().collect();
            let bytes = decode_layer(gf, &refs, sl).ok_or(ClientError::LayerLost {
                channel: c as u8,
                layer: l as u8,
            })?;
            for (idx, &p) in from.layer_positions[l].iter().enumerate() {
                let off = idx * 4;
                let arr = [bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]];
                plane[p as usize] = f32::from_le_bytes(arr);
            }
        }
        haar_inverse(&mut plane, w, h, levels);
        out[c] = plane;
    }
    Ok((out, bytes_used))
}

/// Gather and decode the full object. Each shard read is hashed and compared
/// against the expected hash list from the manifest; shards with an unknown
/// hash (bit-rot / substitution / garbage) are dropped BEFORE decode.
pub async fn get_object(
    gf: &Gf,
    manifest: &Manifest,
    live: &LiveNodes,
) -> Result<Vec<Vec<f32>>, ClientError> {
    let w = manifest.width as usize;
    let h = manifest.height as usize;
    let levels = manifest.levels as usize;
    let nlayers = manifest.nlayers as usize;
    let mut out = vec![vec![0f32; w * h]; manifest.channels as usize];

    for c in 0..manifest.channels as usize {
        let mut plane = vec![0f32; w * h];
        for l in 0..nlayers {
            let raw = gather_layer(manifest, live, c as u8, l as u8).await?;
            let expected: HashSet<Hash> = manifest.shard_hashes[c][l].iter().copied().collect();
            let verified: Vec<Shard> = raw
                .into_iter()
                .filter(|s| expected.contains(&shard_hash(s)))
                .collect();
            let refs: Vec<&Shard> = verified.iter().collect();
            let sl = manifest.sym_len[l] as usize;
            let bytes = decode_layer(gf, &refs, sl).ok_or(ClientError::LayerLost {
                channel: c as u8,
                layer: l as u8,
            })?;
            for (idx, &p) in manifest.layer_positions[l].iter().enumerate() {
                let off = idx * 4;
                let arr = [bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]];
                plane[p as usize] = f32::from_le_bytes(arr);
            }
        }
        haar_inverse(&mut plane, w, h, levels);
        out[c] = plane;
    }
    Ok(out)
}

/// companion to [`get_object_up_to_layer`]: fetches only
/// the specific block ids requested per layer instead of the whole
/// layer, then places those blocks' coefficients into the DWT plane
/// (leaving un-fetched positions at zero) and runs inverse-Haar.
///
/// `layer_block_ids[l]` lists which block ids to fetch for layer
/// `l`; an empty vec means "skip this layer entirely" (its plane
/// coefficients stay zero, the same as `get_object_up_to_layer` for
/// layers above `max_layer`).
///
/// Bandwidth = sum of downloaded payload bytes across all fetched
/// blocks — the bandwidth-aware `/spotlight` numerator. Each block
/// contributes `block_size * 4` bytes (or less for the final short
/// block in a layer).
///
/// Fetch strategy: one `Request::Audit` per (channel, block) against
/// the first replica by HRW; falls back to the next replica on RPC
/// error / hash-mismatch / not-found. Returns `LayerLost` when every
/// replica fails for at least one requested block — that's a
/// data-loss signal for the caller.
pub async fn get_object_blocks(
    manifest: &Manifest,
    live: &LiveNodes,
    layer_block_ids: &[Vec<u32>],
) -> Result<(Vec<Vec<f32>>, u64), ClientError> {
    let w = manifest.width as usize;
    let h = manifest.height as usize;
    let levels = manifest.levels as usize;
    let nlayers = manifest.nlayers as usize;
    let (replication, block_size) = match manifest.encoding {
        ObjectEncoding::Replicated {
            replication,
            block_size,
        } => (replication, block_size as usize),
        ObjectEncoding::Rlnc => {
            return Err(ClientError::Incompatible(
                "get_object_blocks called on an Rlnc-encoded object; \
                 use get_object_up_to_layer / get_object_with_coeff_mask instead"
                    .into(),
            ))
        }
    };
    assert_eq!(
        layer_block_ids.len(),
        nlayers,
        "layer_block_ids must have one entry per layer"
    );
    let mut out = vec![vec![0f32; w * h]; manifest.channels as usize];
    let mut bytes_used: u64 = 0;

    for c in 0..manifest.channels as usize {
        let mut plane = vec![0f32; w * h];
        for l in 0..nlayers {
            let ids = &layer_block_ids[l];
            if ids.is_empty() {
                continue;
            }
            let positions = &manifest.layer_positions[l];
            for &b in ids {
                let expected_hash = *manifest.shard_hashes[c][l]
                    .get(b as usize)
                    .ok_or(ClientError::LayerLost {
                        channel: c as u8,
                        layer: l as u8,
                    })?;
                let key = ShardKey {
                    object_id: manifest.object_id,
                    channel: c as u8,
                    layer: l as u8,
                    shard_idx: b,
                };
                let replicas = place_replicas(key, replication, live)?;
                // Walk replicas in HRW order — first hit wins.
                let mut fetched: Option<Shard> = None;
                for node in &replicas {
                    // B4 (v2 completion): every other raw
                    // `manifest.nodes[..]` in this crate is already
                    // `.get(..)`-guarded; this Replicated-audit
                    // callsite is the one v2 flagged. A monitor
                    // tick racing an in-progress add_node could hand
                    // an index past `manifest.nodes.len()` here too.
                    let Some(addr) = manifest.nodes.get(*node) else {
                        continue;
                    };
                    let req = Request::Audit {
                        object_id: manifest.object_id,
                        channel: c as u8,
                        layer: l as u8,
                        shard_hash: expected_hash,
                    };
                    match rpc(addr, req).await {
                        Ok(Response::AuditResp { shard: Some(s) })
                            if shard_hash(&s) == expected_hash =>
                        {
                            fetched = Some(s);
                            break;
                        }
                        Ok(_) => continue,
                        Err(_) => continue,
                    }
                }
                let shard = fetched.ok_or(ClientError::LayerLost {
                    channel: c as u8,
                    layer: l as u8,
                })?;
                bytes_used += shard.payload.len() as u64;
                // Scatter payload bytes → coefficients → plane
                // positions. Block b spans positions
                // [b*bs .. (b+1)*bs], truncated by the layer end.
                let start = (b as usize) * block_size;
                let end = (start + block_size).min(positions.len());
                if shard.payload.len() != (end - start) * 4 {
                    return Err(ClientError::UnexpectedResponse {
                        expected: "block payload size = (end-start)*4",
                        got: format!(
                            "block {b} in layer {l}: payload len {} != expected {}",
                            shard.payload.len(),
                            (end - start) * 4
                        ),
                    });
                }
                for (i, &p) in positions[start..end].iter().enumerate() {
                    let off = i * 4;
                    let arr = [
                        shard.payload[off],
                        shard.payload[off + 1],
                        shard.payload[off + 2],
                        shard.payload[off + 3],
                    ];
                    plane[p as usize] = f32::from_le_bytes(arr);
                }
            }
        }
        haar_inverse(&mut plane, w, h, levels);
        out[c] = plane;
    }
    Ok((out, bytes_used))
}


/// Encode UTF-8 text into a cluster object. The text is split into K chunks
/// along UTF-8 boundaries, then encoded as a single layer (channels=1,
/// nlayers=1). The first K shards are systematic — each literally contains
/// its chunk.
///
/// The manifest is mutated: `data_cid`, `object_id`, `merkle_root`,
/// `shard_hashes`, **`chunk_lens`**, `kind=Text`, `content_type` are filled in.
pub async fn put_text_object(
    gf: &Gf,
    manifest: &mut Manifest,
    live: &LiveNodes,
    text: &str,
) -> Result<(), ClientError> {
    use holofs_model::manifest::ObjectKind;
    assert_eq!(
        manifest.kind,
        ObjectKind::Text,
        "put_text_object: kind must be Text"
    );
    assert_eq!(
        manifest.channels, 1,
        "put_text_object: channels must be 1"
    );
    assert_eq!(
        manifest.nlayers, 1,
        "put_text_object: nlayers must be 1"
    );

    let split = split_text_into_k_chunks(text);
    manifest.chunk_lens = split.chunk_lens;
    manifest.sym_len = vec![split.sym_len as u32];

    // MinHash for fuzzy similar-text search.
    manifest.text_minhash = holofs_codec::text_codec::compute_minhash(text);

    // CID = SHA-256 of raw text + metadata (kind=Text, content_type).
    let cid = {
        let mut hasher = holofs_core::hash::Sha256::new();
        hasher.update(b"holofs-text-v1");
        hasher.update(text.as_bytes());
        hasher.update(manifest.content_type.as_bytes());
        hasher.finalize()
    };
    manifest.data_cid = cid;
    manifest.object_id = u64::from_be_bytes(cid[0..8].try_into().unwrap());
    let seed = u64::from_be_bytes(cid[8..16].try_into().unwrap());
    let mut rng = Rng::new(seed);

    let n = manifest.n_per_layer[0] as usize;
    let (_sl, shards) = encode_layer(gf, &split.padded, n, &mut rng);

    let mut hashes: Vec<Hash> = Vec::with_capacity(shards.len());
    let mut per_node_reqs: Vec<Vec<Request>> = vec![Vec::new(); manifest.nodes.len()];
    for (idx, shard) in shards.into_iter().enumerate() {
        let h = shard_hash(&shard);
        hashes.push(h);
        let node = manifest.place_shard(0, 0, idx as u32, live)?;
        per_node_reqs[node].push(Request::Put {
            object_id: manifest.object_id,
            channel: 0,
            layer: 0,
            shard,
        });
    }
    fanout_puts(&manifest.nodes, per_node_reqs).await?;
    manifest.shard_hashes = vec![vec![hashes.clone()]];
    manifest.merkle_root = merkle_root(&hashes);
    Ok(())
}

/// Gather text from nodes. Returns `(text_with_holes, n_holes)`. If too few
/// nodes are available it recovers what it can and inserts a
/// `[--missing chunk #N--]` marker in place of the rest. This is **partial recovery**.
pub async fn get_text_object_with_holes(
    gf: &Gf,
    manifest: &Manifest,
    live: &LiveNodes,
) -> Result<(Vec<u8>, usize), ClientError> {
    use holofs_model::manifest::ObjectKind;
    assert_eq!(manifest.kind, ObjectKind::Text);
    let raw = gather_layer(manifest, live, 0, 0).await?;
    let expected: HashSet<Hash> = manifest.shard_hashes[0][0].iter().copied().collect();
    let verified: Vec<Shard> = raw
        .into_iter()
        .filter(|s| expected.contains(&shard_hash(s)))
        .collect();
    let refs: Vec<&Shard> = verified.iter().collect();
    let sl = manifest.sym_len[0] as usize;
    let chunks = decode_layer_with_holes(gf, &refs, sl);
    Ok(assemble_text_with_holes(&chunks, &manifest.chunk_lens))
}


/// Encode an audio object: 1D Haar DWT per channel, priority-layer placement
/// (like images but without the second axis). Channels (1 or 2) become
/// `manifest.channels`; `manifest.width` = sample_count, `manifest.height` = 1.
pub async fn put_audio_object(
    gf: &Gf,
    manifest: &mut Manifest,
    live: &LiveNodes,
    channels: &[Vec<f32>],
) -> Result<(), ClientError> {
    use holofs_model::manifest::ObjectKind;
    assert_eq!(
        manifest.kind,
        ObjectKind::Audio,
        "put_audio_object: kind must be Audio"
    );
    let levels = manifest.levels as usize;
    let nlayers = manifest.nlayers as usize;
    let n_samples = manifest.width as usize;
    assert!(
        n_samples % (1 << levels) == 0,
        "sample_count {n_samples} must be multiple of 2^{levels}"
    );

    // CID — a deterministic hash of "raw audio + params".
    let cid = {
        let mut hasher = holofs_core::hash::Sha256::new();
        hasher.update(b"holofs-audio-v1");
        hasher.update(&(manifest.k as u32).to_be_bytes());
        hasher.update(&(n_samples as u32).to_be_bytes());
        hasher.update(&[manifest.channels]);
        hasher.update(&manifest.content_type.as_bytes());
        for ch in channels {
            for s in ch {
                hasher.update(&s.to_le_bytes());
            }
        }
        hasher.finalize()
    };
    manifest.data_cid = cid;
    manifest.object_id = u64::from_be_bytes(cid[0..8].try_into().unwrap());
    let seed = u64::from_be_bytes(cid[8..16].try_into().unwrap());
    let mut rng = Rng::new(seed);

    let mut shard_hashes: Vec<Vec<Vec<Hash>>> =
        vec![vec![Vec::new(); nlayers]; manifest.channels as usize];
    let mut leaves_flat: Vec<Hash> = Vec::new();
    let mut per_node_reqs: Vec<Vec<Request>> = vec![Vec::new(); manifest.nodes.len()];

    for c in 0..manifest.channels as usize {
        let mut plane = channels[c].clone();
        haar_forward_1d(&mut plane, levels);
        for l in 0..nlayers {
            let positions = &manifest.layer_positions[l];
            let mut bytes = Vec::with_capacity(positions.len() * 4);
            for &p in positions {
                bytes.extend_from_slice(&plane[p as usize].to_le_bytes());
            }
            let n = manifest.n_per_layer[l] as usize;
            let (sl, shards) = encode_layer(gf, &bytes, n, &mut rng);
            assert_eq!(
                sl, manifest.sym_len[l] as usize,
                "sym_len in manifest disagrees with the actual encoding"
            );

            for (idx, shard) in shards.into_iter().enumerate() {
                let h = shard_hash(&shard);
                shard_hashes[c][l].push(h);
                leaves_flat.push(h);

                let node = manifest.place_shard(c as u8, l as u8, idx as u32, live)?;
                per_node_reqs[node].push(Request::Put {
                    object_id: manifest.object_id,
                    channel: c as u8,
                    layer: l as u8,
                    shard,
                });
            }
        }
    }

    fanout_puts(&manifest.nodes, per_node_reqs).await?;

    manifest.shard_hashes = shard_hashes;
    manifest.merkle_root = merkle_root(&leaves_flat);
    Ok(())
}

/// Progressive audio read: decode only layers `0..=max_layer`; the rest stay
/// zero. `max_layer = nlayers - 1` → full quality;
/// `max_layer = 0` → bass-only preview.
pub async fn get_audio_object_up_to_layer(
    gf: &Gf,
    manifest: &Manifest,
    live: &LiveNodes,
    max_layer: u8,
) -> Result<(Vec<Vec<f32>>, u64), ClientError> {
    use holofs_model::manifest::ObjectKind;
    assert_eq!(manifest.kind, ObjectKind::Audio);
    let levels = manifest.levels as usize;
    let nlayers = manifest.nlayers as usize;
    let n_samples = manifest.width as usize;
    let mut out = vec![vec![0f32; n_samples]; manifest.channels as usize];
    let mut bytes_used: u64 = 0;

    for c in 0..manifest.channels as usize {
        let mut plane = vec![0f32; n_samples];
        for l in 0..nlayers {
            if (l as u8) > max_layer {
                continue;
            }
            let raw = gather_layer(manifest, live, c as u8, l as u8).await?;
            let expected: HashSet<Hash> = manifest.shard_hashes[c][l].iter().copied().collect();
            let verified: Vec<Shard> = raw
                .into_iter()
                .filter(|s| expected.contains(&shard_hash(s)))
                .collect();
            let sl = manifest.sym_len[l] as usize;
            bytes_used += verified.len() as u64 * (manifest.k as u64 + sl as u64);
            let refs: Vec<&Shard> = verified.iter().collect();
            let bytes = decode_layer(gf, &refs, sl).ok_or(ClientError::LayerLost {
                channel: c as u8,
                layer: l as u8,
            })?;
            for (idx, &p) in manifest.layer_positions[l].iter().enumerate() {
                let off = idx * 4;
                let arr = [bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]];
                plane[p as usize] = f32::from_le_bytes(arr);
            }
        }
        haar_inverse_1d(&mut plane, levels);
        out[c] = plane;
    }
    Ok((out, bytes_used))
}

/// audio layer filtering. Decode an audio object but only
/// place coefficients from layers where `keep[layer] == true` —
/// dropped layers contribute zero before the inverse Haar. Each layer
/// roughly maps to a frequency band (L0 = bass envelope, ascending),
/// so `keep=[true,false,...]` is a low-pass / "underwater" filter,
/// `keep=[false,...,true]` keeps only highs, etc.
pub async fn get_audio_filtered(
    gf: &Gf,
    manifest: &Manifest,
    live: &LiveNodes,
    keep: &[bool],
) -> Result<(Vec<Vec<f32>>, u64), ClientError> {
    use holofs_model::manifest::ObjectKind;
    assert_eq!(manifest.kind, ObjectKind::Audio);
    let levels = manifest.levels as usize;
    let nlayers = manifest.nlayers as usize;
    let n_samples = manifest.width as usize;
    let mut out = vec![vec![0f32; n_samples]; manifest.channels as usize];
    let mut bytes_used: u64 = 0;

    for c in 0..manifest.channels as usize {
        let mut plane = vec![0f32; n_samples];
        for l in 0..nlayers {
            // Layer outside the keep mask: skip the fetch entirely
            // and leave its coefficients at zero in the plane.
            let kept = keep.get(l).copied().unwrap_or(false);
            if !kept {
                continue;
            }
            let raw = gather_layer(manifest, live, c as u8, l as u8).await?;
            let expected: HashSet<Hash> = manifest.shard_hashes[c][l].iter().copied().collect();
            let verified: Vec<Shard> = raw
                .into_iter()
                .filter(|s| expected.contains(&shard_hash(s)))
                .collect();
            let sl = manifest.sym_len[l] as usize;
            bytes_used += verified.len() as u64 * (manifest.k as u64 + sl as u64);
            let refs: Vec<&Shard> = verified.iter().collect();
            let bytes = decode_layer(gf, &refs, sl).ok_or(ClientError::LayerLost {
                channel: c as u8,
                layer: l as u8,
            })?;
            for (idx, &p) in manifest.layer_positions[l].iter().enumerate() {
                let off = idx * 4;
                let arr = [bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]];
                plane[p as usize] = f32::from_le_bytes(arr);
            }
        }
        haar_inverse_1d(&mut plane, levels);
        out[c] = plane;
    }
    Ok((out, bytes_used))
}

/// per-layer DWT coefficient energy, summed across channels.
///
/// Decodes every layer the same way the real GET path does, but instead
/// of placing the coefficients into a plane + running the inverse Haar,
/// it accumulates `sum(coef^2)` per layer. This is the natural "how
/// much information lives at this scale" signal — coarse layers carry
/// average brightness/loudness, fine layers carry edges/transients.
/// Works for both image (`ObjectKind::Image`) and audio
/// (`ObjectKind::Audio`); the math is identical because both use a
/// Haar DWT, only the layer count and geometry differ.
///
/// Returns `(energy_per_layer, bytes_used)`. `energy_per_layer.len() ==
/// manifest.nlayers`. Layers that lost too many shards to decode are
/// returned as `LayerLost`; the page should surface that as "n/a".
pub async fn layer_energies(
    gf: &Gf,
    manifest: &Manifest,
    live: &LiveNodes,
) -> Result<(Vec<f64>, u64), ClientError> {
    let nlayers = manifest.nlayers as usize;
    let mut energy = vec![0f64; nlayers];
    let mut bytes_used: u64 = 0;

    for c in 0..manifest.channels as usize {
        for l in 0..nlayers {
            let raw = gather_layer(manifest, live, c as u8, l as u8).await?;
            let expected: HashSet<Hash> =
                manifest.shard_hashes[c][l].iter().copied().collect();
            let verified: Vec<Shard> = raw
                .into_iter()
                .filter(|s| expected.contains(&shard_hash(s)))
                .collect();
            let sl = manifest.sym_len[l] as usize;
            bytes_used += verified.len() as u64 * (manifest.k as u64 + sl as u64);
            let refs: Vec<&Shard> = verified.iter().collect();
            let bytes = decode_layer(gf, &refs, sl).ok_or(ClientError::LayerLost {
                channel: c as u8,
                layer: l as u8,
            })?;
            let positions = &manifest.layer_positions[l];
            for idx in 0..positions.len() {
                let off = idx * 4;
                let arr = [bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]];
                let v = f32::from_le_bytes(arr) as f64;
                energy[l] += v * v;
            }
        }
    }
    Ok((energy, bytes_used))
}


/// Encode an arbitrary binary as a single RLNC "canvas": the payload is split
/// into K chunks and encode_layer emits n systematic+RLNC shards.
/// GET is all-or-nothing — no partial recovery (which is correct for files
/// whose half-recovered version would be garbage).
pub async fn put_opaque_object(
    gf: &Gf,
    manifest: &mut Manifest,
    live: &LiveNodes,
    data: &[u8],
) -> Result<(), ClientError> {
    use holofs_model::manifest::ObjectKind;
    assert_eq!(manifest.kind, ObjectKind::Opaque);
    assert_eq!(manifest.channels, 1);
    assert_eq!(manifest.nlayers, 1);

    // sym_len = ceil(len / K). Real length (no padding) → chunk_lens[0].
    let real_len = data.len();
    let k = manifest.k as usize;
    let sym_len = (real_len + k - 1) / k;
    manifest.sym_len = vec![sym_len as u32];
    manifest.chunk_lens = vec![real_len as u32];

    // CID = SHA-256 of raw bytes + content_type.
    let cid = {
        let mut h = holofs_core::hash::Sha256::new();
        h.update(b"holofs-opaque-v1");
        h.update(manifest.content_type.as_bytes());
        h.update(data);
        h.finalize()
    };
    manifest.data_cid = cid;
    manifest.object_id = u64::from_be_bytes(cid[0..8].try_into().unwrap());
    let seed = u64::from_be_bytes(cid[8..16].try_into().unwrap());
    let mut rng = Rng::new(seed);

    let n = manifest.n_per_layer[0] as usize;
    let (sl, shards) = encode_layer(gf, data, n, &mut rng);
    assert_eq!(sl, sym_len);

    let mut hashes: Vec<Hash> = Vec::with_capacity(shards.len());
    let mut per_node_reqs: Vec<Vec<Request>> = vec![Vec::new(); manifest.nodes.len()];
    for (idx, shard) in shards.into_iter().enumerate() {
        let h = shard_hash(&shard);
        hashes.push(h);
        let node = manifest.place_shard(0, 0, idx as u32, live)?;
        per_node_reqs[node].push(Request::Put {
            object_id: manifest.object_id,
            channel: 0,
            layer: 0,
            shard,
        });
    }
    fanout_puts(&manifest.nodes, per_node_reqs).await?;
    manifest.shard_hashes = vec![vec![hashes.clone()]];
    manifest.merkle_root = merkle_root(&hashes);
    Ok(())
}

/// Read an opaque object byte-perfect. If fewer than K shards are available
/// returns None (no partial recovery; see the `ObjectKind::Opaque` docstring).
pub async fn get_opaque_object(
    gf: &Gf,
    manifest: &Manifest,
    live: &LiveNodes,
) -> Result<Vec<u8>, ClientError> {
    use holofs_model::manifest::ObjectKind;
    assert_eq!(manifest.kind, ObjectKind::Opaque);
    let raw = gather_layer(manifest, live, 0, 0).await?;
    let expected: HashSet<Hash> = manifest.shard_hashes[0][0].iter().copied().collect();
    let verified: Vec<Shard> = raw
        .into_iter()
        .filter(|s| expected.contains(&shard_hash(s)))
        .collect();
    let refs: Vec<&Shard> = verified.iter().collect();
    let sl = manifest.sym_len[0] as usize;
    let decoded = decode_layer(gf, &refs, sl).ok_or(ClientError::LayerLost {
        channel: 0,
        layer: 0,
    })?;
    // Trim to the real length (decoded has padding up to K*sym_len).
    let real_len = manifest
        .chunk_lens
        .first()
        .copied()
        .unwrap_or(decoded.len() as u32) as usize;
    Ok(decoded[..real_len.min(decoded.len())].to_vec())
}

/// Gather live shards for a (channel, layer) from all live nodes.
///
/// Every node's `Get` fires in parallel: on a `LiveNodes` of 40 the
/// previous sequential loop paid `40 × RTT` per (channel, layer) —
/// which the review's 3a analysis pinned as the single largest
/// contributor to GET latency (~O(C·L·N) RTT of ~84 hops for an
/// RGB × 7-layer × 4-node image). Fan-out collapses that to `max
/// RTT`, and any single-node failure aborts the fan-out and
/// surfaces the underlying `ClientError`.
pub async fn gather_layer(
    manifest: &Manifest,
    live: &LiveNodes,
    channel: u8,
    layer: u8,
) -> Result<Vec<Shard>, ClientError> {
    use futures_util::future::try_join_all;

    let tasks = live.iter().filter_map(|&node| {
        // Manifests written when the cluster had fewer nodes carry a
        // shorter `manifest.nodes` — `add_node` extends existing
        // manifests but a monitor tick can race that mutation. Skip
        // out-of-range indices instead of panicking on a raw index.
        let addr = manifest.nodes.get(node)?.clone();
        let req = Request::Get {
            object_id: manifest.object_id,
            channel,
            layer,
        };
        Some(async move {
            match rpc(&addr, req).await? {
                Response::Shards(v) => Ok::<Vec<Shard>, ClientError>(v),
                Response::Error(msg) => Err(ClientError::RemoteError(msg)),
                other => Err(ClientError::UnexpectedResponse {
                    expected: "Shards",
                    got: format!("{other:?}"),
                }),
            }
        })
    });
    let per_node = try_join_all(tasks).await?;
    Ok(per_node.into_iter().flatten().collect())
}

// === Repair ================================================================

/// Regenerate the `replacement` node after the corresponding `dead_node`
/// (same index in `manifest.nodes`) has died.
///
/// Internals: for each (channel, layer) we count how many shards should have
/// landed there according to placement, compare against the current state of
/// `replacement`, fetch `d` donor shards from live nodes, and mix fresh
/// combinations. Writes go to the `replacement` node.
///
/// Regenerates `replacement` on top of its empty storage. Donors are verified
/// (corrupt ones are dropped BEFORE mixing); fresh shards get new hashes which
/// are appended to `manifest.shard_hashes`; the Merkle root is recomputed.
/// The manifest is mutated.
pub async fn repair_node(
    gf: &Gf,
    rng: &mut Rng,
    manifest: &mut Manifest,
    live: &LiveNodes,
    replacement: usize,
    d: usize,
) -> Result<RepairStats, ClientError> {
    // Directory-manifest guard (mirrors the one in
    // `repair_node_replicated`). Monitor / auditor walk every
    // catalog entry including directories, whose `nodes` vec is
    // empty. Without this the raw index below panics with
    // "index out of bounds: the len is 0 but the index is N".
    //parallel Replicated PUTs
    // triggered monitor scans that eventually landed a
    // directory manifest here.
    if manifest.nodes.is_empty() {
        return Ok(RepairStats::default());
    }
    assert!(
        live.contains(&replacement),
        "replacement node must be live"
    );
    let mut stats = RepairStats::default();

    // Clear storage on the node being replaced.
    let replacement_addr = manifest.nodes.get(replacement).ok_or_else(|| {
        ClientError::RemoteError(format!(
            "repair_node: replacement index {replacement} outside manifest.nodes (len={})",
            manifest.nodes.len()
        ))
    })?;
    match rpc(
        replacement_addr,
        Request::Purge {
            object_id: manifest.object_id,
        },
    )
    .await?
    {
        Response::Ack => {}
        Response::Error(msg) => return Err(ClientError::RemoteError(msg)),
        other => {
            return Err(ClientError::UnexpectedResponse { expected: "Ack from Purge", got: format!("{other:?}") })
        }
    }

    let k = manifest.k as usize;

    for c in 0..manifest.channels {
        for l in 0..manifest.nlayers {
            let n_total = manifest.n_per_layer[l as usize] as usize;
            let mut need = 0usize;
            for idx in 0..n_total as u32 {
                if manifest.place_shard(c, l, idx, live)? == replacement {
                    need += 1;
                }
            }
            if need == 0 {
                continue;
            }

            // Collect donors from OTHER live nodes; verify by hash.
            let expected: HashSet<Hash> = manifest.shard_hashes[c as usize][l as usize]
                .iter()
                .copied()
                .collect();
            let live_others: Vec<usize> =
                live.iter().copied().filter(|&n| n != replacement).collect();
            let mut donors: Vec<Shard> = Vec::new();
            for &node in &live_others {
                let Some(addr) = manifest.nodes.get(node) else {
                    continue;
                };
                let req = Request::Get {
                    object_id: manifest.object_id,
                    channel: c,
                    layer: l,
                };
                match rpc(addr, req).await? {
                    Response::Shards(v) => {
                        for s in v {
                            if expected.contains(&shard_hash(&s)) {
                                donors.push(s);
                            }
                            if donors.len() >= d {
                                break;
                            }
                        }
                    }
                    Response::Error(msg) => return Err(ClientError::RemoteError(msg)),
                    other => {
                        return Err(ClientError::UnexpectedResponse { expected: "Shards", got: format!("{other:?}") })
                    }
                }
                if donors.len() >= d {
                    break;
                }
            }

            if donors.is_empty() {
                stats.layers_unrecoverable += 1;
                continue;
            }
            donors.truncate(d);

            let slen = manifest.sym_len[l as usize] as u64;
            let bytes_per_shard = k as u64 + slen;
            stats.bytes_downloaded += donors.len() as u64 * bytes_per_shard;
            stats.bytes_baseline_full += (k as u64).min(donors.len() as u64) * bytes_per_shard;
            stats.gf_muls_baseline +=
                (k as u64) * (k as u64 + 1) * (slen + k as u64) + (need as u64) * (k as u64) * slen;
            stats.gf_muls_repair += (need as u64) * (donors.len() as u64) * (slen + k as u64);

            let fresh = mix_donors(gf, &donors, need, rng);
            for shard in fresh {
                let h = shard_hash(&shard);
                manifest.shard_hashes[c as usize][l as usize].push(h);
                match rpc(
                    replacement_addr,
                    Request::Put {
                        object_id: manifest.object_id,
                        channel: c,
                        layer: l,
                        shard,
                    },
                )
                .await?
                {
                    Response::Ack => {
                        stats.shards_generated += 1;
                    }
                    Response::Error(msg) => return Err(ClientError::RemoteError(msg)),
                    other => {
                        return Err(ClientError::UnexpectedResponse { expected: "Ack", got: format!("{other:?}") })
                    }
                }
            }
            stats.layers_repaired += 1;
        }
    }

    // Recompute the Merkle root: flat list of all expected hashes.
    let mut leaves = Vec::new();
    for per_c in &manifest.shard_hashes {
        for per_l in per_c {
            leaves.extend_from_slice(per_l);
        }
    }
    manifest.merkle_root = merkle_root(&leaves);

    Ok(stats)
}

/// repair path for `ObjectEncoding::Replicated` objects.
///
/// For every `(channel, layer, block_id)` whose HRW-computed
/// R-placement contains `replacement`, fetch a byte-identical copy
/// from any of the other R-1 replicas via `Request::Audit` and
/// re-put it on the replacement node. No new hashes are generated
/// (block content is byte-identical across replicas → Merkle root
/// stays put), so the manifest's `shard_hashes` and `merkle_root`
/// are read-only here.
///
/// Stats fields (repurposed from the RLNC-shaped `RepairStats`):
///   * `shards_generated` — blocks actually re-planted.
///   * `bytes_downloaded` — total fetched from surviving replicas.
///   * `layers_repaired` — layers that had at least one block
///     touched (loose "layer touched" count).
///   * `layers_unrecoverable` — blocks whose every OTHER replica
///     was also dead.
///
/// Assumes `manifest.encoding` is `Replicated`; returns
/// `Incompatible` otherwise so a caller that forked incorrectly
/// fails loud instead of silently dropping shards.
pub async fn repair_node_replicated(
    manifest: &mut Manifest,
    live: &LiveNodes,
    replacement: usize,
) -> Result<RepairStats, ClientError> {
    // Directory-manifest guard. `Manifest::directory` produces a
    // no-shard marker whose `nodes` vec is empty; the monitor path
    // (`Gateway::repair_object_inplace`) walks every catalog entry
    // including directories, and without this guard would panic
    // on `manifest.nodes[replacement]` below. Matches the same
    // guard the RLNC `repair_node` gets via `place_shard`'s
    // empty-set branch.
    if manifest.nodes.is_empty() {
        return Ok(RepairStats::default());
    }
    assert!(
        live.contains(&replacement),
        "replacement node must be live"
    );
    let (replication, _block_size) = match manifest.encoding {
        ObjectEncoding::Replicated {
            replication,
            block_size,
        } => (replication, block_size),
        ObjectEncoding::Rlnc => {
            return Err(ClientError::Incompatible(
                "repair_node_replicated called on an Rlnc-encoded object; \
                 use repair_node instead"
                    .into(),
            ))
        }
    };
    let replacement_addr = manifest.nodes.get(replacement).ok_or_else(|| {
        ClientError::RemoteError(format!(
            "repair_node_replicated: replacement index {replacement} outside \
             manifest.nodes (len={})",
            manifest.nodes.len()
        ))
    })?;
    let mut stats = RepairStats::default();
    // Purge the replacement's object bucket first — clears any
    // stale shards from a previous incarnation (matches the RLNC
    // repair's Purge step). Idempotent on a fresh node.
    match rpc(
        replacement_addr,
        Request::Purge {
            object_id: manifest.object_id,
        },
    )
    .await?
    {
        Response::Ack => {}
        Response::Error(msg) => return Err(ClientError::RemoteError(msg)),
        other => {
            return Err(ClientError::UnexpectedResponse {
                expected: "Ack from Purge",
                got: format!("{other:?}"),
            })
        }
    }

    let live_others: Vec<usize> = live.iter().copied().filter(|&n| n != replacement).collect();
    for c in 0..manifest.channels {
        for l in 0..manifest.nlayers {
            let n_blocks = manifest.n_per_layer[l as usize] as usize;
            let mut layer_touched = false;
            for b in 0..n_blocks as u32 {
                let key = ShardKey {
                    object_id: manifest.object_id,
                    channel: c,
                    layer: l,
                    shard_idx: b,
                };
                let placement = place_replicas(key, replication, live)?;
                if !placement.contains(&replacement) {
                    continue;
                }
                let expected_hash = manifest.shard_hashes[c as usize][l as usize][b as usize];
                // Fetch from any live OTHER replica first, then
                // any live OTHER node — HRW-favoured order.
                let other_replicas: Vec<usize> = placement
                    .iter()
                    .copied()
                    .filter(|&n| n != replacement && live_others.contains(&n))
                    .collect();
                let mut sources: Vec<usize> = other_replicas.clone();
                for &n in &live_others {
                    if !sources.contains(&n) {
                        sources.push(n);
                    }
                }
                let mut fetched: Option<Shard> = None;
                for &node in &sources {
                    let Some(addr) = manifest.nodes.get(node) else {
                        continue;
                    };
                    let req = Request::Audit {
                        object_id: manifest.object_id,
                        channel: c,
                        layer: l,
                        shard_hash: expected_hash,
                    };
                    match rpc(addr, req).await {
                        Ok(Response::AuditResp { shard: Some(s) })
                            if shard_hash(&s) == expected_hash =>
                        {
                            fetched = Some(s);
                            break;
                        }
                        Ok(_) => continue,
                        Err(_) => continue,
                    }
                }
                let Some(shard) = fetched else {
                    stats.layers_unrecoverable += 1;
                    continue;
                };
                stats.bytes_downloaded += shard.payload.len() as u64;
                let put_req = Request::Put {
                    object_id: manifest.object_id,
                    channel: c,
                    layer: l,
                    shard,
                };
                match rpc(replacement_addr, put_req).await? {
                    Response::Ack => {
                        stats.shards_generated += 1;
                        layer_touched = true;
                    }
                    Response::Error(msg) => return Err(ClientError::RemoteError(msg)),
                    other => {
                        return Err(ClientError::UnexpectedResponse {
                            expected: "Ack from Put",
                            got: format!("{other:?}"),
                        })
                    }
                }
            }
            if layer_touched {
                stats.layers_repaired += 1;
            }
        }
    }
    // Manifest merkle_root + shard_hashes unchanged for Replicated
    // (block content is byte-identical across replicas).
    Ok(stats)
}

/// Erase every shard of the object on each `live` node. Invoked from
/// `DELETE /<name>` in the gateway. Returns Ok only when ALL nodes acknowledge
/// the Purge; the first refusal yields Err.
pub async fn purge_object(manifest: &Manifest, live: &LiveNodes) -> Result<(), ClientError> {
    for &node in live {
        let Some(addr) = manifest.nodes.get(node) else {
            continue;
        };
        let req = Request::Purge {
            object_id: manifest.object_id,
        };
        match rpc(addr, req).await? {
            Response::Ack => {}
            Response::Error(msg) => return Err(ClientError::RemoteError(msg)),
            other => {
                return Err(ClientError::UnexpectedResponse { expected: "Ack from Purge", got: format!("{other:?}") })
            }
        }
    }
    Ok(())
}

/// diagnostic: fetch a node's process-wide breakdown of PUT wall
/// time as `(lock_wait_ns, put_appended_ns, wal_wait_ns, count)`.
/// Zero-cost on hot path — counters are three `fetch_add` per PUT
/// on the node side. Used by the July 2026 fanout amplification
/// study to pin down whether the 500× slowdown lives in the store
/// mutex, the WAL append, or the group-commit fsync wait.
pub async fn node_put_timings(addr: &str) -> Result<(u64, u64, u64, u64), ClientError> {
    match rpc(addr, Request::PutTimings).await? {
        Response::PutTimings {
            lock_wait_ns,
            put_appended_ns,
            wal_wait_ns,
            count,
        } => Ok((lock_wait_ns, put_appended_ns, wal_wait_ns, count)),
        Response::Error(msg) => Err(ClientError::RemoteError(msg)),
        other => Err(ClientError::UnexpectedResponse {
            expected: "PutTimings from PutTimings",
            got: format!("{other:?}"),
        }),
    }
}

/// enumerate every shard hash a node currently stores.
/// Used by the gateway GC to compute orphans (held by node but not
/// referenced by any catalog / version manifest). Address is taken
/// directly — node need not belong to any particular manifest, so the
/// caller picks from cluster topology, not from `manifest.nodes`.
pub async fn list_node_hashes(addr: &str) -> Result<Vec<Hash>, ClientError> {
    match rpc(addr, Request::ListHashes).await? {
        Response::Hashes(hs) => Ok(hs),
        Response::Error(msg) => Err(ClientError::RemoteError(msg)),
        other => Err(ClientError::UnexpectedResponse { expected: "Hashes from ListHashes", got: format!("{other:?}") }),
    }
}

/// ask a node to delete every shard whose hash is in
/// `hashes`. Idempotent — a node that never held a hash just no-ops on
/// it. Returns `Ok(())` once the node acks; on Error response, surfaces
/// the protocol error.
pub async fn purge_node_by_hash(
    addr: &str,
    hashes: Vec<Hash>,
) -> Result<(), ClientError> {
    match rpc(addr, Request::PurgeByHash { hashes }).await? {
        Response::Ack => Ok(()),
        Response::Error(msg) => Err(ClientError::RemoteError(msg)),
        other => Err(ClientError::UnexpectedResponse { expected: "Ack from PurgeByHash", got: format!("{other:?}") }),
    }
}

/// epoch-GC: read a node's current wall-clock (ms since
/// UNIX_EPOCH). Snapshotted by the gateway GC pass to gate the
/// subsequent [`purge_node_by_hash_up_to`] — shards with an epoch
/// greater than the snapshot are protected from purge.
pub async fn node_current_epoch(addr: &str) -> Result<u64, ClientError> {
    match rpc(addr, Request::CurrentEpoch).await? {
        Response::Epoch { epoch } => Ok(epoch),
        Response::Error(msg) => Err(ClientError::RemoteError(msg)),
        other => Err(ClientError::UnexpectedResponse {
            expected: "Epoch from CurrentEpoch",
            got: format!("{other:?}"),
        }),
    }
}

/// epoch-GC: same as [`purge_node_by_hash`] but the node only
/// removes shards whose stored write-epoch is `<= max_epoch`. Purges
/// that would otherwise race a concurrent PUT are safely no-op'd on
/// the fresh shard.
pub async fn purge_node_by_hash_up_to(
    addr: &str,
    hashes: Vec<Hash>,
    max_epoch: u64,
) -> Result<(), ClientError> {
    match rpc(addr, Request::PurgeByHashUpTo { hashes, max_epoch }).await? {
        Response::Ack => Ok(()),
        Response::Error(msg) => Err(ClientError::RemoteError(msg)),
        other => Err(ClientError::UnexpectedResponse {
            expected: "Ack from PurgeByHashUpTo",
            got: format!("{other:?}"),
        }),
    }
}

/// Ping every node in `manifest`. Returns the indices of those that replied Pong.
pub async fn discover_live(manifest: &Manifest) -> LiveNodes {
    let mut live = Vec::new();
    for (i, addr) in manifest.nodes.iter().enumerate() {
        match rpc(addr, Request::Ping).await {
            Ok(Response::Pong) => live.push(i),
            _ => {}
        }
    }
    live
}

/// check a node's identity. Sends a random nonce, receives a
/// signature, verifies it against `expected_pubkey`. `true` — the node
/// controls the private key from the whitelist; `false` — spoofing, network
/// error, wrong key, or broken protocol.
pub async fn auth_check(addr: &str, expected_pubkey: &PubKey) -> bool {
    let nonce = fresh_nonce();
    let resp = match rpc(addr, Request::AuthChallenge { nonce }).await {
        Ok(r) => r,
        Err(_) => return false,
    };
    match resp {
        Response::AuthChallengeOk { signature } => {
            verify_challenge(expected_pubkey, &nonce, &signature)
        }
        _ => false,
    }
}

/// Discover, filtered by an identity handshake against `whitelist`.
/// Nodes whose actual pubkey does not match the whitelist entry are dropped.
/// Nodes absent from the whitelist are dropped.
///
/// Returns indices into `manifest.nodes`. Use this in place of `discover_live`
/// when you want trust-rooted-in-admin semantics.
pub async fn discover_live_with_whitelist(
    manifest: &Manifest,
    whitelist: &holofs_storage::whitelist::Whitelist,
) -> LiveNodes {
    let mut live = Vec::new();
    for (i, addr) in manifest.nodes.iter().enumerate() {
        let Some(entry) = whitelist.lookup_by_addr(addr) else {
            continue;
        };
        // Ping first (fast rejection), then auth (heavier, but the pool is small).
        if let Ok(Response::Pong) = rpc(addr, Request::Ping).await {
            if auth_check(addr, &entry.pubkey).await {
                live.push(i);
            }
        }
    }
    live
}

#[cfg(test)]
mod error_variant_tests {
    use super::*;

    #[test]
    fn remote_error_displays_remote_prefix() {
        let e = ClientError::RemoteError("shard missing".into());
        assert_eq!(format!("{e}"), "remote: shard missing");
        assert!(!e.is_timeout());
    }

    #[test]
    fn unexpected_response_records_expected_and_got() {
        let e = ClientError::UnexpectedResponse {
            expected: "Ack",
            got: "Pong".into(),
        };
        let s = format!("{e}");
        assert!(s.contains("expected Ack"), "{s}");
        assert!(s.contains("got Pong"), "{s}");
    }

    #[test]
    fn is_timeout_keys_on_io_kind_only() {
        let e = ClientError::Io(io::Error::new(io::ErrorKind::TimedOut, "slow peer"));
        assert!(e.is_timeout());
        let e = ClientError::Io(io::Error::new(io::ErrorKind::BrokenPipe, "EPIPE"));
        assert!(!e.is_timeout());
        let e = ClientError::RemoteError("nope".into());
        assert!(!e.is_timeout());
    }
}

#[cfg(test)]
mod rpc_tests {
    //! Exercise the wire-touching helpers (list_node_hashes /
    //! purge_node_by_hash / discover_live / auth_check) against
    //! tiny in-process mock servers. Same pattern as the auditor
    //! tests in holofs-cluster.

    use super::*;
    use holofs_testutils::spawn_mock_node;
    use tokio::net::TcpListener;

    /// RAII guard combining (a) the crate-wide pool test mutex
    /// (serialises with `pool::tests`) and (b) `HOLOFS_POOL_DISABLE=1`
    /// scoped to the test. The combination prevents this test's env
    /// var from bleeding into a parallel pool-reuse test.
    struct DisablePool {
        _lock: std::sync::MutexGuard<'static, ()>,
        old: Option<String>,
    }
    impl DisablePool {
        fn new() -> Self {
            let lock = crate::pool::test_pool_lock();
            let old = std::env::var("HOLOFS_POOL_DISABLE").ok();
            std::env::set_var("HOLOFS_POOL_DISABLE", "1");
            Self { _lock: lock, old }
        }
    }
    impl Drop for DisablePool {
        fn drop(&mut self) {
            match &self.old {
                Some(v) => std::env::set_var("HOLOFS_POOL_DISABLE", v),
                None => std::env::remove_var("HOLOFS_POOL_DISABLE"),
            }
        }
    }

    #[tokio::test]
    async fn list_node_hashes_decodes_response() {
        let _g = DisablePool::new();
        let payload = vec![[0x11u8; 32], [0x22u8; 32]];
        let (addr, _h) = spawn_mock_node(Response::Hashes(payload.clone())).await;
        let got = list_node_hashes(&addr).await.unwrap();
        assert_eq!(got, payload);
    }

    #[tokio::test]
    async fn list_node_hashes_surfaces_remote_error() {
        let _g = DisablePool::new();
        let (addr, _h) = spawn_mock_node(Response::Error("disk full".into())).await;
        let err = list_node_hashes(&addr).await.unwrap_err();
        assert!(matches!(err, ClientError::RemoteError(s) if s == "disk full"));
    }

    #[tokio::test]
    async fn list_node_hashes_unexpected_response_surfaces_typed_error() {
        let _g = DisablePool::new();
        let (addr, _h) = spawn_mock_node(Response::Pong).await;
        let err = list_node_hashes(&addr).await.unwrap_err();
        assert!(matches!(err, ClientError::UnexpectedResponse { .. }));
    }

    ///`tokio::time::timeout` drops the RPC
    /// future mid-frame, but the pooled socket was returned via
    /// its normal Drop path — pool then handed a half-read socket
    /// to the *next* caller who got somebody else's response and
    /// surfaced it as `UnexpectedResponse`. Fix: poison-on-any-Err
    /// in `rpc_attempt`. This test pins that behavior: after a
    /// timeout, the pool must hold ZERO idle entries for the
    /// timed-out addr.
    #[tokio::test]
    async fn rpc_timeout_poisons_socket_instead_of_recycling_to_pool() {
        // Not using DisablePool — we specifically want the pool
        // ENABLED so the test can observe whether the socket
        // came back. Grab the pool test lock manually.
        let _lock = crate::pool::test_pool_lock();
        crate::pool::clear();

        // Mock that accepts connections but never writes back.
        // Every RPC against it stalls forever until the client
        // times out.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let _stall = tokio::spawn(async move {
            loop {
                let (sock, _) = match listener.accept().await {
                    Ok(p) => p,
                    Err(_) => return,
                };
                tokio::spawn(async move {
                    let _keep = sock;
                    std::future::pending::<()>().await;
                });
            }
        });

        // Very short RPC timeout so the test doesn't wait long.
        let prev = std::env::var("HOLOFS_RPC_TIMEOUT_MS").ok();
        std::env::set_var("HOLOFS_RPC_TIMEOUT_MS", "80");

        let res = rpc(&addr, Request::Ping).await;
        assert!(
            matches!(&res, Err(e) if e.kind() == io::ErrorKind::TimedOut),
            "expected TimedOut, got {res:?}"
        );

        // The invariant. Pool must be empty for this addr — both
        // the initial attempt AND the transient-retry attempt
        // timed out AND their sockets must have been poisoned
        // instead of recycled.
        let (_, total_idle) = crate::pool::stats();
        assert_eq!(
            total_idle, 0,
            "timed-out sockets must be poisoned, not recycled to pool"
        );

        match prev {
            Some(v) => std::env::set_var("HOLOFS_RPC_TIMEOUT_MS", v),
            None => std::env::remove_var("HOLOFS_RPC_TIMEOUT_MS"),
        }
        crate::pool::clear();
    }

    #[tokio::test]
    async fn purge_node_by_hash_acks_successfully() {
        let _g = DisablePool::new();
        let (addr, _h) = spawn_mock_node(Response::Ack).await;
        purge_node_by_hash(&addr, vec![[0xAB; 32]]).await.unwrap();
    }

    #[tokio::test]
    async fn discover_live_picks_only_pong_nodes() {
        let _g = DisablePool::new();
        // One node responds with Pong, one with Error. Only the first
        // should land in the live set.
        let (good_addr, _g) = spawn_mock_node(Response::Pong).await;
        let (bad_addr, _b) = spawn_mock_node(Response::Error("oh no".into())).await;
        // Spoof a manifest with just these two nodes.
        let mut m = Manifest {
            object_id: 0,
            k: 4,
            nlayers: 1,
            n_per_layer: vec![4],
            sym_len: vec![32],
            layer_positions: vec![vec![]],
            channels: 1,
            width: 8,
            height: 8,
            levels: 1,
            nodes: vec![good_addr.clone(), bad_addr.clone()],
            placement: holofs_model::placement::Placement::Rendezvous,
            zones: vec![0, 0],
            data_cid: [0; 32],
            merkle_root: [0; 32],
            shard_hashes: vec![vec![Vec::new(); 1]; 1],
            kind: holofs_model::manifest::ObjectKind::Image,
            content_type: "image/png".into(),
            chunk_lens: vec![],
            audio_sample_rate: 0,
            text_minhash: vec![],
            created_at_unix: 0,
            encoding: holofs_model::manifest::ObjectEncoding::Rlnc,
            state: holofs_model::manifest::ManifestState::Ready,
        };
        // Tag for ignored warnings on read-only fields.
        m.object_id = 1;
        let live = discover_live(&m).await;
        assert_eq!(live, vec![0], "expected only the Pong-responding node");
    }

    #[tokio::test]
    async fn discover_live_returns_empty_when_all_unreachable() {
        let _g = DisablePool::new();
        // Bind+drop to get two closed ports.
        let l1 = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a1 = l1.local_addr().unwrap().to_string();
        let l2 = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a2 = l2.local_addr().unwrap().to_string();
        drop(l1);
        drop(l2);
        let m = Manifest {
            object_id: 0,
            k: 4,
            nlayers: 1,
            n_per_layer: vec![4],
            sym_len: vec![32],
            layer_positions: vec![vec![]],
            channels: 1,
            width: 8,
            height: 8,
            levels: 1,
            nodes: vec![a1, a2],
            placement: holofs_model::placement::Placement::Rendezvous,
            zones: vec![0, 0],
            data_cid: [0; 32],
            merkle_root: [0; 32],
            shard_hashes: vec![vec![Vec::new(); 1]; 1],
            kind: holofs_model::manifest::ObjectKind::Image,
            content_type: "image/png".into(),
            chunk_lens: vec![],
            audio_sample_rate: 0,
            text_minhash: vec![],
            created_at_unix: 0,
            encoding: holofs_model::manifest::ObjectEncoding::Rlnc,
            state: holofs_model::manifest::ManifestState::Ready,
        };
        let live = discover_live(&m).await;
        assert!(live.is_empty());
    }
}
