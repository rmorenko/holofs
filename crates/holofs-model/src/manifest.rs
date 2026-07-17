//! Object manifest: metadata that tells the client how an object is encoded
//! and where its shards must live.

use std::io;

use crate::placement::{place, place_layer_zone_aware, NoLiveNodes, Placement, ShardKey};
use holofs_core::merkle::Hash;

/// Object kind determines the PUT/GET path. Introduced in stage 8.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKind {
    /// Raster image. DWT decomposition into priority layers, 3 channels of f32,
    /// holographic (fuzzy) degradation on loss.
    Image,
    /// Text file. Split into chunks along UTF-8 boundaries (single layer, channels=1).
    /// Shard loss yields **holes** at the lost positions — the rest of the
    /// text stays readable. `content_type` stores the original MIME
    /// (text/plain, text/markdown).
    Text,
    /// Audio file. 1D Haar DWT per channel, 4 priority layers.
    /// `manifest.width` = sample_count, `height` = 1, `channels` = 1 or 2.
    /// On node loss high frequencies drop first → audio becomes muffled while
    /// the bass and overall envelope persist the longest.
    Audio,
    /// Arbitrary binary (PDF, DOCX, ZIP, EXE, DB dumps, etc.). Encoded as a
    /// single RLNC layer without DWT — such formats have no "low resolution"
    /// (half a ZIP is broken ZIP, not a "fuzzy archive"). Behaviour is classic
    /// erasure coding: `alive ≥ K` → byte-perfect recovery, otherwise dead.
    /// No graceful degradation, but the cluster still serves as a universal
    /// durable store. `chunk_lens[0]` = real length (without padding).
    Opaque,
    /// Directory — a manifest-as-marker for a path prefix. Carries no shards,
    /// no nodes, no integrity hashes (all Vec fields empty, all u8/u32
    /// numeric fields zero). Its sole purpose is to make a hierarchical
    /// path-prefix navigable: `photos/2026/img.jpg` is only addressable when
    /// `photos` and `photos/2026` both exist as `Directory` entries. Use
    /// [`Manifest::directory`] to construct one; the binary codec emits it
    /// as discriminant `4` under magic `HOLOFSM7`.
    Directory,
}

/// Lifecycle state for asynchronous ingest. The sync path always
/// produces `Ready` (encode + fanout finish before the handler
/// returns). The async path (opt-in via `HOLOFS_ASYNC_ENCODE=1`)
/// inserts a `Encoding` manifest into the catalog synchronously,
/// returns `202 Accepted` to the client, and flips the state to
/// `Ready` (or `Failed`) once the background worker finishes.
///
/// Read-side handlers gate on this: GET/HEAD on `Encoding` returns
/// `503 Retry-After`, on `Failed` returns `404 Not Found` (with a
/// `X-Encode-Failed` header for diagnostics).
///
/// Legacy manifests (magic `HOLOFSM9` and earlier) decode as
/// `Ready` — the enum was introduced in `HOLOFSMA`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ManifestState {
    /// Sync-produced object, or async object that finished encoding
    /// + shard-fanout successfully. Every reader path uses this.
    #[default]
    Ready,
    /// Async in-flight: the catalog entry exists but shard hashes /
    /// merkle root are placeholders and the shards have not yet
    /// been dispatched to the cluster. Only `PUT` (idempotent
    /// conflict) touches these.
    Encoding,
    /// Async encoding attempt failed (RLNC / wire / decode error).
    /// The catalog keeps the entry as a tombstone so a subsequent
    /// `PUT` can replace it; reads treat it as absent.
    Failed,
}

impl ManifestState {
    /// Discriminant byte used on the wire-format.
    #[must_use]
    pub fn tag(&self) -> u8 {
        match self {
            ManifestState::Ready => 0,
            ManifestState::Encoding => 1,
            ManifestState::Failed => 2,
        }
    }

    /// Reverse of [`Self::tag`]. Unknown discriminants become
    /// `Ready` — safer than failing to decode, and future writers
    /// that add new states can update readers to interpret them.
    #[must_use]
    pub fn from_tag(tag: u8) -> Self {
        match tag {
            1 => ManifestState::Encoding,
            2 => ManifestState::Failed,
            _ => ManifestState::Ready,
        }
    }
}

/// /.1: how an object's per-(channel, layer) shards are
/// laid out.
///
/// `Rlnc` (the historical default) packs each layer's coefficients into
/// `K` source chunks and emits `n_per_layer[l]` linear combinations —
/// fault-tolerant but opaque to ROI fetches because every shard mixes
/// every coefficient.
///
/// `Replicated { replication, block_size }` ships in each
/// layer's coefficients are grouped into `block_size`-wide blocks and
/// each block is replicated to `replication` cluster nodes. Payload
/// per shard is `block_size * 4` bytes (raw `f32` coefficients).
/// Because a block is a contiguous slice of `layer_positions`, the
/// gateway can ask for exactly the blocks that overlap a ROI —
/// bandwidth-aware `/spotlight` is the marquee use case.
///
/// **Sizing rule of thumb (worth the same 5-second sanity-check the
/// rollback taught us):** total shards per PUT ≈
/// `channels × (Σ layer_lengths / block_size) × replication`. A
/// 512×512 RGB image at `block_size=64, replication=3` hits ≈ 36 k
/// shards — comfortable for the disk-backed store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectEncoding {
    /// Default. Each layer's K chunks fan into n_per_layer[l] RLNC
    /// shards. `shard_hashes[c][l]` length matches `n_per_layer[l]`.
    Rlnc,
    /// per-block replicated encoding. See the type doc.
    Replicated {
        /// How many cluster nodes hold each block. Also the
        /// per-block fault-tolerance budget: `replication - 1`
        /// nodes may drop a block before it's unrecoverable.
        replication: u8,
        /// Block width in coefficients. Payload of one shard =
        /// `block_size * 4` bytes.
        block_size: u32,
    },
}

impl ObjectEncoding {
    /// Discriminant byte used on the wire-format.
    #[must_use]
    pub fn tag(&self) -> u8 {
        match self {
            ObjectEncoding::Rlnc => 0,
            ObjectEncoding::Replicated { .. } => 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub object_id: u64,
    pub k: u16,
    pub nlayers: u8,
    pub n_per_layer: Vec<u32>,
    pub sym_len: Vec<u32>,
    pub layer_positions: Vec<Vec<u32>>, // pixel positions in the source image (shared across channels)
    pub channels: u8,
    pub width: u32,
    pub height: u32,
    pub levels: u8,
    pub nodes: Vec<String>,
    pub placement: Placement,
    /// Zone id (rack/AZ) for each node in `nodes`. Length = `nodes.len()`.
    /// Used by `Placement::RendezvousZoneAware` for anti-affinity. If all
    /// values are equal (or the field is all zeros) zone-aware degenerates
    /// into plain HRW.
    pub zones: Vec<u8>,

    /// Stable object CID — hash over source data + parameters.
    /// Identical inputs produce identical `data_cid` across clients.
    pub data_cid: Hash,
    /// Merkle root over the hashes of the currently-live shards. Updated on repair.
    pub merkle_root: Hash,
    /// Expected shard hashes: `shard_hashes[channel][layer]` — every hash that
    /// ever validly lived in that pair (including regenerated ones).
    /// On GET, a shard whose hash is not in here is dropped before decode.
    pub shard_hashes: Vec<Vec<Vec<Hash>>>,

    /// Object kind (Image / Text). Determines the decode path and response format.
    pub kind: ObjectKind,
    /// MIME returned to clients on GET (`image/png`, `text/plain; charset=utf-8`).
    pub content_type: String,
    /// For `Text`: lengths of the K source chunks (before padding to `sym_len`).
    /// Without this we cannot tell where a chunk truly ends vs zero padding.
    /// Empty for `Image`.
    pub chunk_lens: Vec<u32>,
    /// For `Audio`: sample rate in Hz. 0 for non-audio.
    pub audio_sample_rate: u32,
    /// For `Text`: bottom-K MinHash fingerprint for fuzzy similar-text search.
    /// Computed at PUT time, ~64 u32 values. Empty for non-text.
    pub text_minhash: Vec<u32>,

    /// Unix epoch seconds at which this manifest was created (PUT for
    /// objects, `mkdir` for directories). `0` means "unknown / legacy"
    /// — manifests written under magic `HOLOFSM6` or `HOLOFSM7` (i.e.
    /// before ) carry no timestamp and decode as zero. The
    /// gateway sets this field automatically on every catalog mutation,
    /// so going forward it stays populated.
    pub created_at_unix: u64,

    /// How shards are laid out inside each `(channel, layer)`. Legacy
    /// manifests (HOLOFSM8 and older) decode as `Rlnc`; `HOLOFSM9`
    /// adds an explicit byte plus per-variant payload (currently just
    /// `replication: u8`).
    pub encoding: ObjectEncoding,

    /// Async-ingest lifecycle. Sync-produced manifests are always
    /// [`ManifestState::Ready`]; the async path uses `Encoding` /
    /// `Failed` as transient markers. Legacy manifests (`HOLOFSM9`
    /// and earlier) decode as `Ready` — this field only appears on
    /// `HOLOFSMA` on the wire.
    pub state: ManifestState,

    /// P2.2 per-object retention policy. `None` = no expiry, which
    /// is the default for every object created before P2.2 and every
    /// PUT that doesn't explicitly set retention. `Some(policy)` =
    /// the retention GC daemon may delete this object when the
    /// policy's condition trips. Legacy manifests (`HOLOFSMA` and
    /// earlier) decode as `None` — this field only appears on
    /// `HOLOFSMB` on the wire.
    pub retention: Option<RetentionPolicy>,
}

impl Manifest {
    /// Build a directory marker. Every field is zeroed / empty; `kind` is
    /// `Directory` and `content_type` is the conventional `inode/directory`
    /// MIME. `object_id` is the only payload — used by analytics for stable
    /// identity. Holofs treats this entry as an opaque tombstone for path
    /// resolution: it must exist for every prefix on the way to a real
    /// object, but it never round-trips through the encode / decode shard
    /// machinery. `created_at_unix` is Unix epoch seconds; pass `0` for
    /// directories whose creation time is unknown (e.g. synthesized by
    /// the legacy-catalog migration in `Directory::synthesize_missing_directories`).
    /// Build a blank RGB image manifest ready for `put_object` to
    /// fill in `data_cid`, `merkle_root`, and `shard_hashes`.
    ///
    /// v2 P4.1: single source of truth for the image manifest
    /// layout. Prior to this the same shape was inlined in
    /// `Gateway::blank_manifest` (ingest.rs) *and* the CLI /
    /// bootstrap seed helpers — a canonical maintenance trap that
    /// the review flagged for dedup. The factory lives on `Manifest`
    /// itself so every call site (gateway ingest, CLI `holofs-fs`)
    /// shares it; the bootstrap seed path was retired in v3-11 in
    /// favour of `Gateway::ingest_bytes`.
    ///
    /// - `w`, `h`: source image dimensions in pixels. Must be
    ///   multiples of `2^LEVELS`; asserted by DWT downstream, not here.
    /// - `nodes`: the current cluster's per-node RPC addresses.
    /// - `zones`: parallel to `nodes`; zone/rack id for anti-affinity
    ///   under `RendezvousZoneAware`.
    /// - `placement`: chosen scheme — bootstrap uses
    ///   `RendezvousZoneAware`; the gateway pulls from
    ///   `ClusterInfo::placement`.
    pub fn blank_image(
        w: usize,
        h: usize,
        nodes: Vec<String>,
        zones: Vec<u8>,
        placement: Placement,
    ) -> Self {
        use holofs_core::transform::coeff_layer;
        use holofs_core::{K, LEVELS, NLAYERS, RED};

        let mut layer_positions: Vec<Vec<u32>> = vec![Vec::new(); NLAYERS];
        for y in 0..h {
            for x in 0..w {
                layer_positions[coeff_layer(x, y, w, h)].push((y * w + x) as u32);
            }
        }
        let n_per_layer: Vec<u32> = (0..NLAYERS)
            .map(|l| (K as f32 * RED[l]).round() as u32)
            .collect();
        let sym_len: Vec<u32> = layer_positions
            .iter()
            .map(|pos| ((pos.len() * 4 + K - 1) / K) as u32)
            .collect();
        Self {
            object_id: 0,
            k: K as u16,
            nlayers: NLAYERS as u8,
            n_per_layer,
            sym_len,
            layer_positions,
            channels: 3,
            width: w as u32,
            height: h as u32,
            levels: LEVELS as u8,
            nodes,
            placement,
            zones,
            data_cid: [0; 32],
            merkle_root: [0; 32],
            shard_hashes: vec![vec![Vec::new(); NLAYERS]; 3],
            kind: ObjectKind::Image,
            content_type: "image/png".into(),
            chunk_lens: vec![],
            audio_sample_rate: 0,
            text_minhash: vec![],
            created_at_unix: 0,
            encoding: ObjectEncoding::Rlnc,
            state: ManifestState::Ready,
            retention: None,
        }
    }

    pub fn directory(object_id: u64, created_at_unix: u64) -> Self {
        Self {
            object_id,
            k: 0,
            nlayers: 0,
            n_per_layer: Vec::new(),
            sym_len: Vec::new(),
            layer_positions: Vec::new(),
            channels: 0,
            width: 0,
            height: 0,
            levels: 0,
            nodes: Vec::new(),
            placement: Placement::Rendezvous,
            zones: Vec::new(),
            data_cid: [0u8; 32],
            merkle_root: [0u8; 32],
            shard_hashes: Vec::new(),
            kind: ObjectKind::Directory,
            content_type: "inode/directory".to_string(),
            chunk_lens: Vec::new(),
            audio_sample_rate: 0,
            text_minhash: Vec::new(),
            created_at_unix,
            encoding: ObjectEncoding::Rlnc,
            state: ManifestState::Ready,
            retention: None,
        }
    }

    /// Where shard `(channel, layer, shard_idx)` would land for the given `live`.
    /// For zone-aware placement this recomputes the whole-layer layout — that
    /// is `O(n_shards * live)`, which for our sizes (≤ 64 shards, ≤ 64 nodes)
    /// runs in microseconds.
    ///
    /// Returns [`NoLiveNodes`] when `live` is empty — callers are expected
    /// to surface this as a 503 / cluster-degraded error rather than letting
    /// the gateway panic on a fully-down cluster.
    pub fn place_shard(
        &self,
        channel: u8,
        layer: u8,
        shard_idx: u32,
        live: &[usize],
    ) -> Result<usize, NoLiveNodes> {
        match self.placement {
            Placement::RoundRobin | Placement::Rendezvous => {
                let key = ShardKey {
                    object_id: self.object_id,
                    channel,
                    layer,
                    shard_idx,
                };
                place(self.placement, key, self.nodes.len(), live)
            }
            Placement::RendezvousZoneAware => {
                let layout = place_layer_zone_aware(
                    self.object_id,
                    channel,
                    layer,
                    self.n_per_layer[layer as usize],
                    self.nodes.len(),
                    live,
                    &self.zones,
                )?;
                // `shard_idx` may exceed `n_per_layer[layer]` when
                // the caller is walking `shard_hashes[c][l]` after
                // an auto-repair (repair_node APPENDS fresh hashes
                // without growing `n_per_layer`). Wrap into the
                // layout instead of panicking — the extra hashes
                // land on the same rendezvous slots as the
                // originals, which is the canonical-node
                // semantic auditor callers already expect. Guard
                // for the empty-layout edge (n_per_layer[l] == 0
                // on a corrupt/empty manifest).
                if layout.is_empty() {
                    return Err(NoLiveNodes);
                }
                Ok(layout[(shard_idx as usize) % layout.len()])
            }
        }
    }
}

/// P2.2 — per-object retention policy. `None` = no expiry (default,
/// backward-compatible with pre-P2.2 manifests). `Some(policy)` = the
/// retention GC daemon (see `holofs_gateway::retention_gc`) may
/// delete this object based on the policy's rules.
///
/// # Wire format
///
/// The manifest encoder emits one discriminant byte + variant-
/// specific payload after the async-ingest `state` byte:
///
/// ```text
/// [1 byte retention_kind]
///   0 = None (no tail)
///   1 = ExpiresAt { expires_at_unix: u64 BE }
/// ```
///
/// New variants append their own discriminant + payload without
/// bumping the magic further — the discriminant is an extension
/// point identical in shape to `ObjectEncoding::tag`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetentionPolicy {
    /// Absolute deadline: the GC daemon deletes this object once
    /// wall-clock ≥ `expires_at_unix`. Unix epoch **seconds**.
    /// Setting `expires_at_unix` in the past is allowed — the object
    /// becomes eligible on the next GC tick (idempotent). Clients
    /// that want "expire N days from now" compute `now() + N * 86400`
    /// on their side and store that.
    ExpiresAt {
        /// Unix epoch seconds at which the object should be deleted.
        expires_at_unix: u64,
    },
}

impl RetentionPolicy {
    /// Discriminant byte for [`Self`]. `0` means "no policy" (encoded
    /// when the manifest's `retention` is `None`), so live variants
    /// start at `1`.
    #[must_use]
    pub fn tag(&self) -> u8 {
        match self {
            RetentionPolicy::ExpiresAt { .. } => 1,
        }
    }

    /// `true` when wall-clock `now_unix` (Unix epoch seconds) has
    /// crossed this policy's deletion trigger. Free-standing so the
    /// GC daemon can decide without materialising a wall-clock inside
    /// the type.
    #[must_use]
    pub const fn is_expired(&self, now_unix: u64) -> bool {
        match self {
            RetentionPolicy::ExpiresAt { expires_at_unix } => now_unix >= *expires_at_unix,
        }
    }
}

/// Current on-disk magic. Bumped `HOLOFSM8` → `HOLOFSM9` to append
/// the `ObjectEncoding` tail (one byte for the variant, plus
/// variant-specific payload); then bumped `HOLOFSM9` → `HOLOFSMA`
/// to append the async-ingest `state` byte; then bumped `HOLOFSMA`
/// → `HOLOFSMB` to append the P2.2 `retention` tail (discriminant
/// byte + variant payload — see [`RetentionPolicy`]). Pure-append
/// schema extensions each time: readers see the new bytes, legacy
/// readers
/// decode through and default missing fields (`encoding = Rlnc`,
/// `state = Ready` for pre-async manifests and `retention = None`
/// for pre-P2.2 manifests).
const MAGIC: &[u8; 8] = b"HOLOFSMB";
/// Previous MAGIC — has every field of `HOLOFSMB` *except* the
/// trailing `retention` tail (discriminant byte + variant payload).
/// Records under this magic decode with `retention = None`.
const MAGIC_LEGACY_VA: &[u8; 8] = b"HOLOFSMA";
/// Pre-async MAGIC — has every field of `HOLOFSMA` *except* the
/// trailing `state` byte introduced by the async-ingest work.
/// Records under this magic decode with `state = Ready`.
const MAGIC_LEGACY_V9: &[u8; 8] = b"HOLOFSM9";
/// magic — accepted on read; lacks the trailing
/// `encoding` byte (defaults to `Rlnc`).
const MAGIC_LEGACY_V8: &[u8; 8] = b"HOLOFSM8";
/// magic — accepted on read; also no `created_at_unix`
/// (defaults to 0). Legal set of `kind` values identical to V8.
const MAGIC_LEGACY_V7: &[u8; 8] = b"HOLOFSM7";
/// Pre-Stage-9 magic — also accepted on read. Cannot carry
/// `ObjectKind::Directory`; otherwise identical wire layout. Both
/// `created_at_unix` and `encoding` default on decode.
const MAGIC_LEGACY: &[u8; 8] = b"HOLOFSM6";

impl Manifest {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(MAGIC);
        b.extend_from_slice(&self.object_id.to_be_bytes());
        b.extend_from_slice(&self.k.to_be_bytes());
        b.push(self.nlayers);
        b.push(self.channels);
        b.extend_from_slice(&self.width.to_be_bytes());
        b.extend_from_slice(&self.height.to_be_bytes());
        b.push(self.levels);
        b.push(self.placement.tag());

        assert_eq!(self.n_per_layer.len(), self.nlayers as usize);
        assert_eq!(self.sym_len.len(), self.nlayers as usize);
        assert_eq!(self.layer_positions.len(), self.nlayers as usize);

        for &n in &self.n_per_layer {
            b.extend_from_slice(&n.to_be_bytes());
        }
        for &s in &self.sym_len {
            b.extend_from_slice(&s.to_be_bytes());
        }
        for positions in &self.layer_positions {
            b.extend_from_slice(&(positions.len() as u32).to_be_bytes());
            for &p in positions {
                b.extend_from_slice(&p.to_be_bytes());
            }
        }
        b.extend_from_slice(&(self.nodes.len() as u32).to_be_bytes());
        for n in &self.nodes {
            let bytes = n.as_bytes();
            b.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
            b.extend_from_slice(bytes);
        }

        assert_eq!(
            self.zones.len(),
            self.nodes.len(),
            "manifest.zones length must equal nodes.len()"
        );
        b.extend_from_slice(&self.zones);

        b.extend_from_slice(&self.data_cid);
        b.extend_from_slice(&self.merkle_root);
        assert_eq!(self.shard_hashes.len(), self.channels as usize);
        for per_channel in &self.shard_hashes {
            assert_eq!(per_channel.len(), self.nlayers as usize);
            for per_layer in per_channel {
                b.extend_from_slice(&(per_layer.len() as u32).to_be_bytes());
                for h in per_layer {
                    b.extend_from_slice(h);
                }
            }
        }

        b.push(match self.kind {
            ObjectKind::Image => 0,
            ObjectKind::Text => 1,
            ObjectKind::Audio => 2,
            ObjectKind::Opaque => 3,
            ObjectKind::Directory => 4,
        });
        let ctb = self.content_type.as_bytes();
        b.extend_from_slice(&(ctb.len() as u16).to_be_bytes());
        b.extend_from_slice(ctb);
        b.extend_from_slice(&(self.chunk_lens.len() as u32).to_be_bytes());
        for &cl in &self.chunk_lens {
            b.extend_from_slice(&cl.to_be_bytes());
        }
        b.extend_from_slice(&self.audio_sample_rate.to_be_bytes());
        b.extend_from_slice(&(self.text_minhash.len() as u32).to_be_bytes());
        for &h in &self.text_minhash {
            b.extend_from_slice(&h.to_be_bytes());
        }

        b.extend_from_slice(&self.created_at_unix.to_be_bytes());

        b.push(self.encoding.tag());
        match self.encoding {
            ObjectEncoding::Rlnc => {}
            ObjectEncoding::Replicated {
                replication,
                block_size,
            } => {
                b.push(replication);
                b.extend_from_slice(&block_size.to_be_bytes());
            }
        }
        // Async-ingest lifecycle byte. Present under HOLOFSMA and
        // HOLOFSMB; pre-async catalogs decode with `state = Ready`.
        b.push(self.state.tag());
        // P2.2 retention tail. Discriminant `0` = None (no payload).
        // Under HOLOFSMB every writer emits at least the discriminant
        // byte, so pre-P2.2 readers won't mis-parse the tail as a
        // trailing async-ingest state — the magic bumped precisely
        // for this. Kept as a pure-append extension: new
        // RetentionPolicy variants add their own discriminant + tail
        // without another magic bump.
        match self.retention {
            None => b.push(0),
            Some(policy) => {
                b.push(policy.tag());
                match policy {
                    RetentionPolicy::ExpiresAt { expires_at_unix } => {
                        b.extend_from_slice(&expires_at_unix.to_be_bytes());
                    }
                }
            }
        }
        b
    }

    pub fn decode(buf: &[u8]) -> io::Result<Self> {
        let mut c = make_cursor(buf);
        let magic_bytes = c.take(8)?;
        let mut magic = [0u8; 8];
        magic.copy_from_slice(magic_bytes);
        let is_current = magic == *MAGIC;
        let is_legacy_va = magic == *MAGIC_LEGACY_VA;
        let is_legacy_v9 = magic == *MAGIC_LEGACY_V9;
        let is_legacy_v8 = magic == *MAGIC_LEGACY_V8;
        let is_legacy_v7 = magic == *MAGIC_LEGACY_V7;
        let is_legacy_v6 = magic == *MAGIC_LEGACY;
        if !is_current
            && !is_legacy_va
            && !is_legacy_v9
            && !is_legacy_v8
            && !is_legacy_v7
            && !is_legacy_v6
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not a holofs manifest",
            ));
        }
        // `HOLOFSM9`, `HOLOFSMA`, and `HOLOFSMB` share the encoding
        // tail — all three predecessors of the async work carry
        // `ObjectEncoding` explicitly.
        let has_encoding_tail = is_current || is_legacy_va || is_legacy_v9;
        // Async-ingest `state` byte and P2.2 `retention` tail live
        // under the two most recent magics.
        let has_state_tail = is_current || is_legacy_va;
        let has_retention_tail = is_current;
        let object_id = c.u64()?;
        let k = c.u16()?;
        let nlayers = c.u8()?;
        let channels = c.u8()?;
        let width = c.u32()?;
        let height = c.u32()?;
        let levels = c.u8()?;
        let placement_tag = c.u8()?;
        let placement = Placement::from_tag(placement_tag).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown placement scheme: {placement_tag}"),
            )
        })?;

        let mut n_per_layer = Vec::with_capacity(nlayers as usize);
        for _ in 0..nlayers {
            n_per_layer.push(c.u32()?);
        }
        let mut sym_len = Vec::with_capacity(nlayers as usize);
        for _ in 0..nlayers {
            sym_len.push(c.u32()?);
        }
        let mut layer_positions = Vec::with_capacity(nlayers as usize);
        for _ in 0..nlayers {
            let plen = c.u32()? as usize;
            let mut pos = Vec::with_capacity(bounded_cap(plen, 4, c.remaining()));
            for _ in 0..plen {
                pos.push(c.u32()?);
            }
            layer_positions.push(pos);
        }
        let nn = c.u32()? as usize;
        // Minimum per-node encoding = u16 length prefix. Real addrs are
        // longer, but the floor is what makes bounded_cap safe.
        let mut nodes = Vec::with_capacity(bounded_cap(nn, 2, c.remaining()));
        for _ in 0..nn {
            let nlen = c.u16()? as usize;
            let bytes = c.take(nlen)?.to_vec();
            nodes.push(String::from_utf8(bytes).map_err(|e| {
                io::Error::new(io::ErrorKind::InvalidData, format!("node address: {e}"))
            })?);
        }

        let zones = c.take(nn)?.to_vec();

        let mut data_cid = [0u8; 32];
        data_cid.copy_from_slice(c.take(32)?);
        let mut merkle_root = [0u8; 32];
        merkle_root.copy_from_slice(c.take(32)?);
        let mut shard_hashes = vec![vec![Vec::<Hash>::new(); nlayers as usize]; channels as usize];
        for cc in 0..channels as usize {
            for ll in 0..nlayers as usize {
                let count = c.u32()? as usize;
                let mut v = Vec::with_capacity(bounded_cap(count, 32, c.remaining()));
                for _ in 0..count {
                    let mut h = [0u8; 32];
                    h.copy_from_slice(c.take(32)?);
                    v.push(h);
                }
                shard_hashes[cc][ll] = v;
            }
        }

        let kind = match c.u8()? {
            0 => ObjectKind::Image,
            1 => ObjectKind::Text,
            2 => ObjectKind::Audio,
            3 => ObjectKind::Opaque,
            4 => ObjectKind::Directory,
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unknown ObjectKind: {other}"),
                ))
            }
        };
        let ctlen = c.u16()? as usize;
        let content_type = String::from_utf8(c.take(ctlen)?.to_vec()).map_err(|e| {
            io::Error::new(io::ErrorKind::InvalidData, format!("content_type: {e}"))
        })?;
        let cl_n = c.u32()? as usize;
        let mut chunk_lens = Vec::with_capacity(bounded_cap(cl_n, 4, c.remaining()));
        for _ in 0..cl_n {
            chunk_lens.push(c.u32()?);
        }
        let audio_sample_rate = c.u32()?;
        let mh_n = c.u32()? as usize;
        let mut text_minhash = Vec::with_capacity(bounded_cap(mh_n, 4, c.remaining()));
        for _ in 0..mh_n {
            text_minhash.push(c.u32()?);
        }

        // trailing u64 timestamp. Present in HOLOFSM8
        // and HOLOFSM9; legacy HOLOFSM6/HOLOFSM7 records end before it.
        let created_at_unix = if is_current || is_legacy_va || is_legacy_v9 || is_legacy_v8 {
            c.u64()?
        } else {
            0
        };
        // /.1: trailing encoding selector. Only present in
        // HOLOFSM9; everything older defaults to `Rlnc`. The
        // Replicated tail grew a `block_size: u32` in         // without a magic bump — the invariant carried over from
        // 15.0 that no `Replicated` manifest was ever persisted
        // means there's no backwards-compat load-path to preserve.
        let encoding = if has_encoding_tail {
            match c.u8()? {
                0 => ObjectEncoding::Rlnc,
                1 => {
                    let replication = c.u8()?;
                    let block_size = c.u32()?;
                    ObjectEncoding::Replicated {
                        replication,
                        block_size,
                    }
                }
                other => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("unknown ObjectEncoding tag: {other}"),
                    ));
                }
            }
        } else {
            ObjectEncoding::Rlnc
        };
        // Async-ingest state byte only under HOLOFSMA and HOLOFSMB.
        // Everything older is by definition `Ready` because those
        // magics predate the async path.
        let state = if has_state_tail {
            ManifestState::from_tag(c.u8()?)
        } else {
            ManifestState::Ready
        };

        // P2.2 retention tail. `has_retention_tail` gates whether we
        // consume any bytes at all — legacy magics have no trailing
        // discriminant to read. Under the current magic the
        // discriminant `0` means "no policy"; unknown discriminants
        // are treated as `None` (forward-compat: a future writer
        // that adds a new variant won't crash older readers, they
        // just miss the policy — the daemon on the older side won't
        // then delete anything it doesn't understand).
        let retention = if has_retention_tail {
            match c.u8()? {
                0 => None,
                1 => {
                    let expires_at_unix = c.u64()?;
                    Some(RetentionPolicy::ExpiresAt { expires_at_unix })
                }
                _other => None,
            }
        } else {
            None
        };

        Ok(Manifest {
            object_id,
            k,
            nlayers,
            n_per_layer,
            sym_len,
            layer_positions,
            channels,
            width,
            height,
            levels,
            nodes,
            placement,
            zones,
            data_cid,
            merkle_root,
            shard_hashes,
            kind,
            content_type,
            chunk_lens,
            audio_sample_rate,
            text_minhash,
            created_at_unix,
            encoding,
            state,
            retention,
        })
    }
}

// v2 P4.3: private `struct Cursor` extracted to
// [`holofs_core::cursor::BeCursor`]. Alias + wrapper keep the local
// callsite spelling unchanged.
type Cursor<'a> = holofs_core::cursor::BeCursor<'a>;

fn make_cursor(buf: &[u8]) -> Cursor<'_> {
    Cursor::new(buf, "manifest truncated")
}

/// Cap a manifest-supplied element count `n` to what could physically
/// fit in the remaining bytes assuming `elem_min_size` per element.
/// Mirrors [`holofs_wire::bounded_cap`] — same rationale (a stray
/// `u32::MAX` in a truncated / crafted manifest must not turn
/// `Vec::with_capacity` into an OOM abort).
fn bounded_cap(n: usize, elem_min_size: usize, remaining: usize) -> usize {
    if elem_min_size == 0 {
        return n;
    }
    n.min(remaining / elem_min_size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_roundtrip() {
        let mut shard_hashes = vec![vec![Vec::<Hash>::new(); 4]; 3];
        for c in 0..3 {
            for l in 0..4 {
                shard_hashes[c][l] = (0..3u8)
                    .map(|i| {
                        let mut h = [0u8; 32];
                        h[0] = c as u8;
                        h[1] = l as u8;
                        h[2] = i;
                        h
                    })
                    .collect();
            }
        }
        let m = Manifest {
            object_id: 0xC0FFEE_BABE,
            k: 16,
            nlayers: 4,
            n_per_layer: vec![64, 40, 26, 18],
            sym_len: vec![4096, 12288, 49152, 196608],
            layer_positions: vec![
                vec![0, 1, 2, 3],
                vec![10, 11, 12],
                vec![100, 200],
                vec![999],
            ],
            channels: 3,
            width: 256,
            height: 256,
            levels: 3,
            nodes: vec![
                "127.0.0.1:5000".into(),
                "127.0.0.1:5001".into(),
                "node-42.local:5002".into(),
            ],
            placement: Placement::RendezvousZoneAware,
            zones: vec![0, 0, 1],
            data_cid: [0xAB; 32],
            merkle_root: [0xCD; 32],
            shard_hashes,
            kind: ObjectKind::Text,
            content_type: "text/plain; charset=utf-8".into(),
            chunk_lens: vec![100, 90, 80, 0],
            audio_sample_rate: 0,
            text_minhash: vec![],
            created_at_unix: 1_700_000_000,
            encoding: ObjectEncoding::Rlnc,
            state: ManifestState::Ready,
            retention: None,
        };
        let bytes = m.encode();
        let back = Manifest::decode(&bytes).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn manifest_rejects_bad_magic() {
        let bad = vec![0u8; 32];
        assert!(Manifest::decode(&bad).is_err());
    }

    #[test]
    fn directory_manifest_roundtrip() {
        let dir = Manifest::directory(0xDEADBEEF, 1_750_000_000);
        let bytes = dir.encode();
        let back = Manifest::decode(&bytes).unwrap();
        assert_eq!(back, dir);
        assert_eq!(back.kind, ObjectKind::Directory);
        assert_eq!(back.content_type, "inode/directory");
        assert!(back.nodes.is_empty());
        assert!(back.shard_hashes.is_empty());
    }

    #[test]
    fn legacy_magic_still_decodes() {
        // Round-trip under HOLOFSM7, then rewrite the header to the
        // pre-Stage-9 magic. The wire layout is identical; decode must
        // accept both. (We can't use ObjectKind::Directory here — the old
        // magic implies the discriminant did not yet exist.)
        let m = Manifest {
            object_id: 7,
            k: 4,
            nlayers: 1,
            n_per_layer: vec![8],
            sym_len: vec![16],
            layer_positions: vec![vec![0]],
            channels: 1,
            width: 1,
            height: 1,
            levels: 0,
            nodes: vec!["127.0.0.1:5000".into()],
            placement: Placement::Rendezvous,
            zones: vec![0],
            data_cid: [1u8; 32],
            merkle_root: [2u8; 32],
            shard_hashes: vec![vec![vec![[3u8; 32]]]],
            kind: ObjectKind::Opaque,
            content_type: "application/octet-stream".into(),
            chunk_lens: vec![8],
            audio_sample_rate: 0,
            text_minhash: vec![],
            // Legacy magic implies no `created_at_unix` was stored, so the
            // expected decode default is 0 — the field gets set here to
            // match what `decode` will yield.
            created_at_unix: 0,
            // Same story for encoding selector.
            encoding: ObjectEncoding::Rlnc,
            state: ManifestState::Ready,
            retention: None,
        };
        let mut bytes = m.encode();
        // Pretend this is a HOLOFSM6 record: rewrite the magic AND chop
        // off the trailing fields appended after V6:
        //   * `created_at_unix` (u64, 8 bytes, )
        //   * encoding selector tag (u8, 1 byte, )
        // That's 9 bytes total for the `Rlnc` default.
        bytes[..8].copy_from_slice(b"HOLOFSM6");
        bytes.truncate(bytes.len() - 9);
        let back = Manifest::decode(&bytes).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn legacy_v7_magic_decodes_with_zero_timestamp() {
        // Records written under HOLOFSM7 lack the
        // trailing `created_at_unix` field. New code must still accept
        // them and zero-fill the timestamp.
        let m = Manifest::directory(0xC0FFEE, 0);
        let mut bytes = m.encode();
        bytes[..8].copy_from_slice(b"HOLOFSM7");
        // Strip timestamp (8) + encoding tag (1).
        bytes.truncate(bytes.len() - 9);
        let back = Manifest::decode(&bytes).unwrap();
        assert_eq!(back.created_at_unix, 0);
        assert_eq!(back.kind, ObjectKind::Directory);
    }

    #[test]
    fn replicated_encoding_roundtrips_block_size() {
        // Replicated tail now carries block_size after
        // replication. Build a small manifest with a Replicated
        // encoding, roundtrip through encode/decode, verify both
        // fields survive.
        let mut m = Manifest::directory(0xC01D, 1_700_000_000);
        // `directory` starts as Rlnc; swap the field directly.
        m.encoding = ObjectEncoding::Replicated {
            replication: 3,
            block_size: 64,
        };
        let bytes = m.encode();
        let back = Manifest::decode(&bytes).unwrap();
        assert_eq!(
            back.encoding,
            ObjectEncoding::Replicated {
                replication: 3,
                block_size: 64,
            }
        );
    }

    #[test]
    fn place_shard_zone_aware_wraps_shard_idx_past_n_per_layer() {
        // Reproducer for theauditor-panic bug: after
        // an auto-repair grows `shard_hashes[c][l]` beyond
        // `n_per_layer[l]`, the auditor picks a random `h_idx`
        // from the grown array and passes it into
        // `place_shard(.., h_idx as u32, ..)`. Zone-aware branch
        // used to `layout[shard_idx]` directly and panicked
        // with `index out of bounds`.
        let n_per_layer: u32 = 4;
        let mut m = Manifest::directory(0xBEEF, 0);
        m.placement = Placement::RendezvousZoneAware;
        m.nodes = (0..8u8)
            .map(|i| format!("127.0.0.1:{}", 9000 + i as u16))
            .collect();
        m.zones = vec![0, 0, 1, 1, 2, 2, 3, 3];
        m.channels = 1;
        m.nlayers = 1;
        m.n_per_layer = vec![n_per_layer];
        let live: Vec<usize> = (0..8).collect();

        // In-range idx works.
        assert!(m.place_shard(0, 0, 0, &live).is_ok());
        assert!(m.place_shard(0, 0, n_per_layer - 1, &live).is_ok());
        // Post-repair "extra" idx — must NOT panic; wraps into
        // the layout.
        assert!(m.place_shard(0, 0, n_per_layer, &live).is_ok());
        assert!(m.place_shard(0, 0, n_per_layer + 17, &live).is_ok());
        assert!(m.place_shard(0, 0, u32::MAX, &live).is_ok());
    }

    #[test]
    fn legacy_v8_magic_decodes_with_default_encoding() {
        // HOLOFSM8 records were written before — they carry
        // `created_at_unix` but no encoding selector. Decode must
        // default to `Rlnc` and keep the timestamp.
        let m = Manifest::directory(0xBADC0FFEE0, 1_700_000_123);
        let mut bytes = m.encode();
        bytes[..8].copy_from_slice(b"HOLOFSM8");
        // Strip only the encoding tag (the legacy V8 path
        // does NOT read it).
        bytes.truncate(bytes.len() - 1);
        let back = Manifest::decode(&bytes).unwrap();
        assert_eq!(back.created_at_unix, 1_700_000_123);
        assert_eq!(back.encoding, ObjectEncoding::Rlnc);
    }

    // --- P2.2 retention roundtrip + backward compat ---

    #[test]
    fn retention_roundtrip_none_and_expires_at() {
        // Both `retention = None` (the pre-P2.2 default) and an
        // explicit `ExpiresAt` policy must survive encode → decode
        // byte-for-byte identical.
        let mut m = Manifest::directory(0xDEADBEEF, 1_700_000_000);
        m.retention = None;
        let back = Manifest::decode(&m.encode()).unwrap();
        assert_eq!(back.retention, None);

        m.retention = Some(RetentionPolicy::ExpiresAt {
            expires_at_unix: 0x1234_5678_9ABC_DEF0,
        });
        let back = Manifest::decode(&m.encode()).unwrap();
        assert_eq!(
            back.retention,
            Some(RetentionPolicy::ExpiresAt {
                expires_at_unix: 0x1234_5678_9ABC_DEF0
            })
        );
    }

    #[test]
    fn legacy_va_manifest_decodes_with_no_retention() {
        // A `HOLOFSMA` manifest predates the retention tail. Decoder
        // must default `retention = None` without consuming any
        // bytes past the state byte — otherwise a legacy catalog
        // fails to load on upgrade, which is the P2.2 backward-
        // compat contract.
        let m = Manifest::directory(0xC0FFEE, 1_700_000_555);
        let mut bytes = m.encode();
        bytes[..8].copy_from_slice(b"HOLOFSMA");
        // Drop the retention tail (single discriminant byte with
        // value 0 = None; that byte is the ONE the HOLOFSMA layout
        // doesn't carry).
        assert_eq!(
            *bytes.last().unwrap(),
            0,
            "test invariant: current encode ends with retention_kind=0"
        );
        bytes.pop();
        let back = Manifest::decode(&bytes).unwrap();
        assert_eq!(back.retention, None);
        // And every field that HOLOFSMA does carry decodes cleanly.
        assert_eq!(back.created_at_unix, 1_700_000_555);
        assert_eq!(back.state, ManifestState::Ready);
    }

    #[test]
    fn retention_unknown_discriminant_forward_compat() {
        // A future writer that adds a new RetentionPolicy variant
        // will emit a discriminant this older decoder doesn't know.
        // The decoder must fall back to `None` rather than crash.
        let m = Manifest::directory(0xF00D, 1_700_000_999);
        let mut bytes = m.encode();
        // Overwrite the trailing retention_kind=0 with a made-up
        // discriminant. NO payload follows in this crafted case, but
        // the current decoder must not try to consume any.
        *bytes.last_mut().unwrap() = 0xEE;
        let back = Manifest::decode(&bytes).unwrap();
        assert_eq!(
            back.retention, None,
            "unknown retention discriminant must decode as None (forward compat)"
        );
    }

    #[test]
    fn is_expired_boundary() {
        let p = RetentionPolicy::ExpiresAt {
            expires_at_unix: 1000,
        };
        assert!(!p.is_expired(999));
        assert!(p.is_expired(1000), "boundary is inclusive");
        assert!(p.is_expired(10_000));
    }
}
