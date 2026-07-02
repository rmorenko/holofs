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

/// Stage 15.0/.1: how an object's per-(channel, layer) shards are
/// laid out.
///
/// `Rlnc` (the historical default) packs each layer's coefficients into
/// `K` source chunks and emits `n_per_layer[l]` linear combinations —
/// fault-tolerant but opaque to ROI fetches because every shard mixes
/// every coefficient.
///
/// `Replicated { replication, block_size }` ships in Stage 15.1: each
/// layer's coefficients are grouped into `block_size`-wide blocks and
/// each block is replicated to `replication` cluster nodes. Payload
/// per shard is `block_size * 4` bytes (raw `f32` coefficients).
/// Because a block is a contiguous slice of `layer_positions`, the
/// gateway can ask for exactly the blocks that overlap a ROI —
/// bandwidth-aware `/spotlight` is the marquee use case.
///
/// **Sizing rule of thumb (worth the same 5-second sanity-check the
/// Stage 15.0 rollback taught us):** total shards per PUT ≈
/// `channels × (Σ layer_lengths / block_size) × replication`. A
/// 512×512 RGB image at `block_size=64, replication=3` hits ≈ 36 k
/// shards — comfortable for the disk-backed store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectEncoding {
    /// Default. Each layer's K chunks fan into n_per_layer[l] RLNC
    /// shards. `shard_hashes[c][l]` length matches `n_per_layer[l]`.
    Rlnc,
    /// Stage 15.1 per-block replicated encoding. See the type doc.
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

    // === Stage 3: integrity and addressing ===============================
    /// Stable object CID — hash over source data + parameters.
    /// Identical inputs produce identical `data_cid` across clients.
    pub data_cid: Hash,
    /// Merkle root over the hashes of the currently-live shards. Updated on repair.
    pub merkle_root: Hash,
    /// Expected shard hashes: `shard_hashes[channel][layer]` — every hash that
    /// ever validly lived in that pair (including regenerated ones).
    /// On GET, a shard whose hash is not in here is dropped before decode.
    pub shard_hashes: Vec<Vec<Vec<Hash>>>,

    // === Stage 8: object kind ============================================
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

    // === Stage 11.12: creation timestamp =================================
    /// Unix epoch seconds at which this manifest was created (PUT for
    /// objects, `mkdir` for directories). `0` means "unknown / legacy"
    /// — manifests written under magic `HOLOFSM6` or `HOLOFSM7` (i.e.
    /// before Stage 11.12) carry no timestamp and decode as zero. The
    /// gateway sets this field automatically on every catalog mutation,
    /// so going forward it stays populated.
    pub created_at_unix: u64,

    // === Stage 15.0: shard layout ========================================
    /// How shards are laid out inside each `(channel, layer)`. Legacy
    /// manifests (HOLOFSM8 and older) decode as `Rlnc`; `HOLOFSM9`
    /// adds an explicit byte plus per-variant payload (currently just
    /// `replication: u8`).
    pub encoding: ObjectEncoding,
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
                Ok(layout[shard_idx as usize])
            }
        }
    }
}

/// Current on-disk magic. Stage 15.0 bumped from `HOLOFSM8` to
/// `HOLOFSM9` to append the new `ObjectEncoding` tail (one byte for
/// the variant, plus variant-specific payload). Pure-append schema
/// extension: readers see the new bytes, legacy readers decode
/// through and default `encoding` to `Rlnc`.
const MAGIC: &[u8; 8] = b"HOLOFSM9";
/// Stage 11.12 magic — accepted on read; lacks the trailing
/// `encoding` byte (defaults to `Rlnc`).
const MAGIC_LEGACY_V8: &[u8; 8] = b"HOLOFSM8";
/// Stage 9 magic — accepted on read; also no `created_at_unix`
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
        b.push(match self.placement {
            Placement::RoundRobin => 0,
            Placement::Rendezvous => 1,
            Placement::RendezvousZoneAware => 2,
        });

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

        // === Stage 5: zones =================================================
        assert_eq!(
            self.zones.len(),
            self.nodes.len(),
            "manifest.zones length must equal nodes.len()"
        );
        b.extend_from_slice(&self.zones);

        // === Stage 3 ========================================================
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

        // === Stage 8: object kind ===========================================
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

        // === Stage 11.12: creation timestamp ==================================
        b.extend_from_slice(&self.created_at_unix.to_be_bytes());

        // === Stage 15.0/.1: encoding selector =================================
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
        b
    }

    pub fn decode(buf: &[u8]) -> io::Result<Self> {
        let mut c = Cursor::new(buf);
        let magic_bytes = c.take(8)?;
        let mut magic = [0u8; 8];
        magic.copy_from_slice(magic_bytes);
        let is_current = magic == *MAGIC;
        let is_legacy_v8 = magic == *MAGIC_LEGACY_V8;
        let is_legacy_v7 = magic == *MAGIC_LEGACY_V7;
        let is_legacy_v6 = magic == *MAGIC_LEGACY;
        if !is_current && !is_legacy_v8 && !is_legacy_v7 && !is_legacy_v6 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not a holofs manifest",
            ));
        }
        let object_id = c.u64()?;
        let k = c.u16()?;
        let nlayers = c.u8()?;
        let channels = c.u8()?;
        let width = c.u32()?;
        let height = c.u32()?;
        let levels = c.u8()?;
        let placement = match c.u8()? {
            0 => Placement::RoundRobin,
            1 => Placement::Rendezvous,
            2 => Placement::RendezvousZoneAware,
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unknown placement scheme: {other}"),
                ))
            }
        };

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
            let mut pos = Vec::with_capacity(plen);
            for _ in 0..plen {
                pos.push(c.u32()?);
            }
            layer_positions.push(pos);
        }
        let nn = c.u32()? as usize;
        let mut nodes = Vec::with_capacity(nn);
        for _ in 0..nn {
            let nlen = c.u16()? as usize;
            let bytes = c.take(nlen)?.to_vec();
            nodes.push(String::from_utf8(bytes).map_err(|e| {
                io::Error::new(io::ErrorKind::InvalidData, format!("node address: {e}"))
            })?);
        }

        // === Stage 5: zones =================================================
        let zones = c.take(nn)?.to_vec();

        // === Stage 3 ========================================================
        let mut data_cid = [0u8; 32];
        data_cid.copy_from_slice(c.take(32)?);
        let mut merkle_root = [0u8; 32];
        merkle_root.copy_from_slice(c.take(32)?);
        let mut shard_hashes = vec![vec![Vec::<Hash>::new(); nlayers as usize]; channels as usize];
        for cc in 0..channels as usize {
            for ll in 0..nlayers as usize {
                let count = c.u32()? as usize;
                let mut v = Vec::with_capacity(count);
                for _ in 0..count {
                    let mut h = [0u8; 32];
                    h.copy_from_slice(c.take(32)?);
                    v.push(h);
                }
                shard_hashes[cc][ll] = v;
            }
        }

        // === Stage 8: object kind ===========================================
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
        let mut chunk_lens = Vec::with_capacity(cl_n);
        for _ in 0..cl_n {
            chunk_lens.push(c.u32()?);
        }
        let audio_sample_rate = c.u32()?;
        let mh_n = c.u32()? as usize;
        let mut text_minhash = Vec::with_capacity(mh_n);
        for _ in 0..mh_n {
            text_minhash.push(c.u32()?);
        }

        // Stage 11.12: trailing u64 timestamp. Present in HOLOFSM8
        // and HOLOFSM9; legacy HOLOFSM6/HOLOFSM7 records end before it.
        let created_at_unix = if is_current || is_legacy_v8 {
            c.u64()?
        } else {
            0
        };
        // Stage 15.0/.1: trailing encoding selector. Only present in
        // HOLOFSM9; everything older defaults to `Rlnc`. The
        // Replicated tail grew a `block_size: u32` in Stage 15.1
        // without a magic bump — the invariant carried over from
        // 15.0 that no `Replicated` manifest was ever persisted
        // means there's no backwards-compat load-path to preserve.
        let encoding = if is_current {
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
        })
    }
}

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}
impl<'a> Cursor<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Cursor { buf, pos: 0 }
    }
    fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> io::Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> io::Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn u64(&mut self) -> io::Result<u64> {
        let b = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_be_bytes(a))
    }
    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        if self.pos + n > self.buf.len() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "manifest truncated",
            ));
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
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
            // Same story for Stage 15.0 encoding selector.
            encoding: ObjectEncoding::Rlnc,
        };
        let mut bytes = m.encode();
        // Pretend this is a HOLOFSM6 record: rewrite the magic AND chop
        // off the trailing fields appended after V6:
        //   * `created_at_unix` (u64, 8 bytes, Stage 11.12)
        //   * encoding selector tag (u8, 1 byte, Stage 15.0)
        // That's 9 bytes total for the `Rlnc` default.
        bytes[..8].copy_from_slice(b"HOLOFSM6");
        bytes.truncate(bytes.len() - 9);
        let back = Manifest::decode(&bytes).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn legacy_v7_magic_decodes_with_zero_timestamp() {
        // Records written under HOLOFSM7 (Stage 9, pre-11.12) lack the
        // trailing `created_at_unix` field. New code must still accept
        // them and zero-fill the timestamp.
        let m = Manifest::directory(0xC0FFEE, 0);
        let mut bytes = m.encode();
        bytes[..8].copy_from_slice(b"HOLOFSM7");
        // Strip Stage 11.12 timestamp (8) + Stage 15.0 encoding tag (1).
        bytes.truncate(bytes.len() - 9);
        let back = Manifest::decode(&bytes).unwrap();
        assert_eq!(back.created_at_unix, 0);
        assert_eq!(back.kind, ObjectKind::Directory);
    }

    #[test]
    fn replicated_encoding_roundtrips_block_size() {
        // Stage 15.1: Replicated tail now carries block_size after
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
    fn legacy_v8_magic_decodes_with_default_encoding() {
        // HOLOFSM8 records were written before Stage 15.0 — they carry
        // `created_at_unix` but no encoding selector. Decode must
        // default to `Rlnc` and keep the timestamp.
        let m = Manifest::directory(0xBADC0FFEE0, 1_700_000_123);
        let mut bytes = m.encode();
        bytes[..8].copy_from_slice(b"HOLOFSM8");
        // Strip only the Stage 15.0 encoding tag (the legacy V8 path
        // does NOT read it).
        bytes.truncate(bytes.len() - 1);
        let back = Manifest::decode(&bytes).unwrap();
        assert_eq!(back.created_at_unix, 1_700_000_123);
        assert_eq!(back.encoding, ObjectEncoding::Rlnc);
    }
}
