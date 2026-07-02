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
use holofs_model::manifest::Manifest;
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
        Err(e) if is_likely_transient(&e) => rpc_attempt(addr, &encoded).await,
        Err(e) => Err(e),
    }
}

async fn rpc_attempt(addr: &str, encoded_req: &[u8]) -> io::Result<Response> {
    let mut s = pool::acquire(addr).await?;
    let inner = async {
        if let Err(e) = write_frame(&mut s, encoded_req).await {
            s.poison();
            return Err(e);
        }
        let buf = match read_frame(&mut s).await {
            Ok(b) => b,
            Err(e) => {
                s.poison();
                return Err(e);
            }
        };
        Response::decode(&buf)
    };
    match rpc_timeout() {
        Some(timeout) => match tokio::time::timeout(timeout, inner).await {
            Ok(r) => r,
            Err(_) => {
                // tokio::time::timeout drops the future on expiry —
                // the in-flight write/read got cancelled mid-frame,
                // so the pooled stream is at an undefined byte
                // boundary. Mark it poisoned so it's not returned
                // to the pool, then surface the timeout to the
                // caller (rpc() retries on TimedOut via
                // is_likely_transient).
                io::Result::Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("rpc to {addr} exceeded {:?}", timeout),
                ))
            }
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

    // 2. Encode, hash, and collect a flat list of Merkle leaves in parallel.
    let mut shard_hashes: Vec<Vec<Vec<Hash>>> =
        vec![vec![Vec::new(); nlayers]; manifest.channels as usize];
    let mut leaves_flat: Vec<Hash> = Vec::new();

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
                let req = Request::Put {
                    object_id: manifest.object_id,
                    channel: c as u8,
                    layer: l as u8,
                    shard,
                };
                match rpc(&manifest.nodes[node], req).await? {
                    Response::Ack => {}
                    Response::Error(msg) => return Err(ClientError::RemoteError(msg)),
                    other => {
                        return Err(ClientError::UnexpectedResponse { expected: "Ack", got: format!("{other:?}") })
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
    let w = manifest.width as usize;
    let h = manifest.height as usize;
    let levels = manifest.levels as usize;
    let nlayers = manifest.nlayers as usize;
    let mut out = vec![vec![0f32; w * h]; manifest.channels as usize];
    let mut bytes_used: u64 = 0;

    for c in 0..manifest.channels as usize {
        let mut plane = vec![0f32; w * h];
        for l in 0..nlayers {
            if (l as u8) > max_layer {
                // Skip this layer — its coefficients stay zero.
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
        haar_inverse(&mut plane, w, h, levels);
        out[c] = plane;
    }
    Ok((out, bytes_used))
}

/// Stage 14.1: variant of [`get_object_up_to_layer`] that decodes
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
/// "two-pass spatial composite" of Stage 13.2.
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

/// Stage 12.5: wavelet mix. For each `(channel, layer)`, pull shards from
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

// === Stage 8: text path ====================================================

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

    // MinHash for fuzzy similar-text search (Stage 10b).
    manifest.text_minhash = holofs_analytics::shingle::compute_minhash(text);

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
    for (idx, shard) in shards.into_iter().enumerate() {
        let h = shard_hash(&shard);
        hashes.push(h);
        let node = manifest.place_shard(0, 0, idx as u32, live)?;
        let req = Request::Put {
            object_id: manifest.object_id,
            channel: 0,
            layer: 0,
            shard,
        };
        match rpc(&manifest.nodes[node], req).await? {
            Response::Ack => {}
            Response::Error(msg) => return Err(ClientError::RemoteError(msg)),
            other => {
                return Err(ClientError::UnexpectedResponse { expected: "Ack", got: format!("{other:?}") })
            }
        }
    }
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

// === Stage 9: audio path ===================================================

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
                let req = Request::Put {
                    object_id: manifest.object_id,
                    channel: c as u8,
                    layer: l as u8,
                    shard,
                };
                match rpc(&manifest.nodes[node], req).await? {
                    Response::Ack => {}
                    Response::Error(msg) => return Err(ClientError::RemoteError(msg)),
                    other => {
                        return Err(ClientError::UnexpectedResponse { expected: "Ack", got: format!("{other:?}") })
                    }
                }
            }
        }
    }

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

/// Stage 12.5: audio layer filtering. Decode an audio object but only
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

/// Stage 12.7: per-layer DWT coefficient energy, summed across channels.
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

// === Stage 10: opaque blob (arbitrary files, no graceful degradation) ======

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
    for (idx, shard) in shards.into_iter().enumerate() {
        let h = shard_hash(&shard);
        hashes.push(h);
        let node = manifest.place_shard(0, 0, idx as u32, live)?;
        let req = Request::Put {
            object_id: manifest.object_id,
            channel: 0,
            layer: 0,
            shard,
        };
        match rpc(&manifest.nodes[node], req).await? {
            Response::Ack => {}
            Response::Error(msg) => return Err(ClientError::RemoteError(msg)),
            other => {
                return Err(ClientError::UnexpectedResponse { expected: "Ack", got: format!("{other:?}") })
            }
        }
    }
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
pub async fn gather_layer(
    manifest: &Manifest,
    live: &LiveNodes,
    channel: u8,
    layer: u8,
) -> Result<Vec<Shard>, ClientError> {
    let mut acc = Vec::new();
    for &node in live {
        let req = Request::Get {
            object_id: manifest.object_id,
            channel,
            layer,
        };
        match rpc(&manifest.nodes[node], req).await? {
            Response::Shards(mut v) => acc.append(&mut v),
            Response::Error(msg) => return Err(ClientError::RemoteError(msg)),
            other => {
                return Err(ClientError::UnexpectedResponse { expected: "Shards", got: format!("{other:?}") })
            }
        }
    }
    Ok(acc)
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
    assert!(
        live.contains(&replacement),
        "replacement node must be live"
    );
    let mut stats = RepairStats::default();

    // Clear storage on the node being replaced.
    match rpc(
        &manifest.nodes[replacement],
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
                let req = Request::Get {
                    object_id: manifest.object_id,
                    channel: c,
                    layer: l,
                };
                match rpc(&manifest.nodes[node], req).await? {
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
                    &manifest.nodes[replacement],
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

/// Erase every shard of the object on each `live` node. Invoked from
/// `DELETE /<name>` in the gateway. Returns Ok only when ALL nodes acknowledge
/// the Purge; the first refusal yields Err.
pub async fn purge_object(manifest: &Manifest, live: &LiveNodes) -> Result<(), ClientError> {
    for &node in live {
        let req = Request::Purge {
            object_id: manifest.object_id,
        };
        match rpc(&manifest.nodes[node], req).await? {
            Response::Ack => {}
            Response::Error(msg) => return Err(ClientError::RemoteError(msg)),
            other => {
                return Err(ClientError::UnexpectedResponse { expected: "Ack from Purge", got: format!("{other:?}") })
            }
        }
    }
    Ok(())
}

/// Stage 14.0: enumerate every shard hash a node currently stores.
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

/// Stage 14.0: ask a node to delete every shard whose hash is in
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

/// v0.7 epoch-GC: read a node's current wall-clock (ms since
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

/// v0.7 epoch-GC: same as [`purge_node_by_hash`] but the node only
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

/// Stage 7.3: check a node's identity. Sends a random nonce, receives a
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
        };
        let live = discover_live(&m).await;
        assert!(live.is_empty());
    }
}
