//! Node service: tokio TCP, accepts wire frames (see `holofs-wire`) and holds
//! a shard store keyed by `(object_id, channel, layer)`.
//!
//! Two storage modes:
//! - **in-memory** (`Store::new`) — pure RAM `HashMap`. Fast, lost on process
//!   exit. Used by tests and in-process demos.
//! - **persistent** (`Store::open(dir)`) — RAM index + one file per shard in
//!   `dir`. Atomic writes via `.tmp` + rename. On startup it scans `dir` and
//!   rebuilds the index. Crash-safe w.r.t. individual mutations: either the
//!   shard file is fully present or it is absent.
//!
//! Shard file format (`HOLOFSS1`):
//! ```text
//! magic:        8  bytes = b"HOLOFSS1"
//! object_id:    8  bytes BE
//! channel:      1  byte
//! layer:        1  byte
//! coeffs_len:   4  bytes BE
//! payload_len:  4  bytes BE
//! coeffs:       coeffs_len bytes
//! payload:      payload_len bytes
//! ```
//! Filename = `hex(sha256(shard))` + `.shard`, located at
//! `dir/<first 2 hex>/<remaining 62 hex>.shard` (git-style fanout so that
//! `ls` does not choke on tens of thousands of files in one directory).

use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

use crate::identity::NodeIdentity;
use holofs_core::hash::hex;
use holofs_core::merkle::{shard_hash, Hash};
use holofs_core::rlnc::Shard;
use holofs_wire::{read_frame, write_frame, Request, Response};

type Key = (u64, u8, u8);

const SHARD_MAGIC_V1: &[u8; 8] = b"HOLOFSS1";
/// v0.7: same file layout as v1 but the `coeffs || payload` blob is
/// sealed with AES-256-GCM. See [`crate::crypto`] for the wire format.
const SHARD_MAGIC_V2: &[u8; 8] = b"HOLOFSS2";
/// Legacy alias kept for pre-v0.7 call sites (tests). New writers
/// pick the magic based on the store's `enc_key` field.
#[allow(dead_code)]
const SHARD_MAGIC: &[u8; 8] = SHARD_MAGIC_V1;

/// Store: for each (object_id, channel, layer) — `HashMap<shard_hash, Shard>`.
/// Using the hash as the key gives O(1) dedup: a repeated PUT of the same
/// shard is a no-op.
///
/// If `dir` is set, every mutation (`put`/`purge`/`wipe`) is mirrored to disk.
#[derive(Default)]
pub struct Store {
    shards: HashMap<Key, HashMap<Hash, Shard>>,
    dir: Option<PathBuf>,
    /// v0.7: per-node AES-256-GCM key derived from
    /// `NodeIdentity::to_bytes()` via HKDF-SHA256. `None` = plaintext
    /// shard files on disk. Reads accept both formats regardless of
    /// this field so upgrades roll gracefully. Writes pick the
    /// format based on this being `Some(_)`.
    enc_key: Option<[u8; crate::crypto::KEY_LEN]>,
}

impl Store {
    /// In-memory store. Process restart = all shards lost.
    pub fn new() -> Self {
        Self::default()
    }

    /// Persistent store: index in RAM, shard files in `dir`. On startup it
    /// scans `dir` and rebuilds the index from existing files. `dir` is
    /// created if it does not exist.
    pub fn open(dir: impl AsRef<Path>) -> io::Result<Self> {
        Self::open_inner(dir, None)
    }

    /// Persistent store with an AES-256-GCM key for at-rest encryption.
    /// The key is what [`crate::crypto::derive_shard_key`] returns when
    /// fed the node's [`crate::identity::NodeIdentity::to_bytes`] seed.
    /// New writes land as `HOLOFSS2` sealed files; reads accept both
    /// `HOLOFSS1` (legacy plaintext) and `HOLOFSS2` (sealed) transparently
    /// so upgrades roll without a rewrite pass.
    pub fn open_with_key(
        dir: impl AsRef<Path>,
        key: [u8; crate::crypto::KEY_LEN],
    ) -> io::Result<Self> {
        Self::open_inner(dir, Some(key))
    }

    fn open_inner(
        dir: impl AsRef<Path>,
        enc_key: Option<[u8; crate::crypto::KEY_LEN]>,
    ) -> io::Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir)?;
        let mut shards: HashMap<Key, HashMap<Hash, Shard>> = HashMap::new();
        for entry in walk_shard_files(&dir)? {
            match read_shard_file(&entry, enc_key.as_ref()) {
                Ok((k, h, shard)) => {
                    shards.entry(k).or_default().insert(h, shard);
                }
                Err(e) => {
                    eprintln!("Store::open: skipping broken file {entry:?}: {e}");
                }
            }
        }
        Ok(Store {
            shards,
            dir: Some(dir),
            enc_key,
        })
    }

    /// Returns `true` if the shard was actually added (false → duplicate).
    pub fn put(&mut self, k: Key, shard: Shard) -> bool {
        let h = shard_hash(&shard);
        let bucket = self.shards.entry(k).or_default();
        if bucket.contains_key(&h) {
            return false;
        }
        if let Some(dir) = &self.dir {
            if let Err(e) = write_shard_file(dir, k, &h, &shard, self.enc_key.as_ref()) {
                eprintln!("Store::put: could not write shard file: {e}");
                return false;
            }
        }
        bucket.insert(h, shard);
        true
    }

    pub fn get(&self, k: Key) -> Vec<Shard> {
        self.shards
            .get(&k)
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default()
    }

    /// Point lookup by shard hash. Used by the Stage 7 audit: a client checks
    /// that the node actually stores **exactly that** shard.
    pub fn get_by_hash(&self, k: Key, hash: &Hash) -> Option<Shard> {
        self.shards.get(&k).and_then(|m| m.get(hash)).cloned()
    }

    pub fn purge(&mut self, object_id: u64) -> usize {
        let mut removed_hashes: Vec<Hash> = Vec::new();
        self.shards.retain(|(o, _, _), bucket| {
            if *o != object_id {
                return true;
            }
            removed_hashes.extend(bucket.keys().copied());
            false
        });
        if let Some(dir) = &self.dir {
            for h in &removed_hashes {
                let path = shard_path(dir, h);
                let _ = fs::remove_file(&path); // missing file is fine
            }
        }
        removed_hashes.len()
    }

    pub fn total(&self) -> usize {
        self.shards.values().map(|v| v.len()).sum()
    }

    /// Stage 14.0: list every shard hash this node currently stores,
    /// across every `(object_id, channel, layer)` bucket. Used by the
    /// gateway's GC pass to compute the "held but not referenced
    /// anywhere in the catalog or version archives" delta.
    pub fn list_all_hashes(&self) -> Vec<Hash> {
        let mut out = Vec::with_capacity(self.total());
        for bucket in self.shards.values() {
            out.extend(bucket.keys().copied());
        }
        out
    }

    /// Stage 14.0: delete every shard whose hash is in `targets`,
    /// regardless of which `(object_id, channel, layer)` bucket it
    /// lived in. Returns the count actually removed. Buckets that
    /// become empty are pruned to reclaim the outer HashMap slot.
    pub fn purge_by_hashes(&mut self, targets: &std::collections::HashSet<Hash>) -> usize {
        let mut removed = 0usize;
        let mut removed_files: Vec<Hash> = Vec::new();
        self.shards.retain(|_, bucket| {
            bucket.retain(|h, _| {
                if targets.contains(h) {
                    removed += 1;
                    removed_files.push(*h);
                    false
                } else {
                    true
                }
            });
            !bucket.is_empty()
        });
        if let Some(dir) = &self.dir {
            for h in &removed_files {
                let path = shard_path(dir, h);
                let _ = fs::remove_file(&path);
            }
        }
        removed
    }

    /// Drop everything (used on "node death + replacement").
    pub fn wipe(&mut self) {
        self.shards.clear();
        if let Some(dir) = &self.dir {
            // Remove the directory contents but keep the directory itself.
            if let Ok(entries) = fs::read_dir(dir) {
                for e in entries.flatten() {
                    let p = e.path();
                    if p.is_dir() {
                        let _ = fs::remove_dir_all(&p);
                    } else {
                        let _ = fs::remove_file(&p);
                    }
                }
            }
        }
    }

    /// Override a shard by key + hash (test-only path for corruption testing).
    pub fn inject_corrupt(&mut self, k: Key, shard: Shard) {
        let h = shard_hash(&shard);
        self.shards.entry(k).or_default().insert(h, shard);
    }

    /// Storage directory if persistent; otherwise `None`.
    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }
}

// === Shard file I/O ========================================================

fn shard_path(dir: &Path, h: &Hash) -> PathBuf {
    let s = hex(h);
    let (head, tail) = s.split_at(2);
    dir.join(head).join(format!("{tail}.shard"))
}

fn write_shard_file(
    dir: &Path,
    k: Key,
    h: &Hash,
    shard: &Shard,
    enc_key: Option<&[u8; crate::crypto::KEY_LEN]>,
) -> io::Result<()> {
    let path = shard_path(dir, h);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("shard.tmp");
    {
        let mut f = fs::File::create(&tmp)?;
        // Build the 26-byte header once. It's plaintext in either
        // format and — when enc_key is set — feeds the AES-GCM AAD
        // so any tamper with these bytes trips the tag on decrypt.
        let mut header = [0u8; 26];
        let magic = if enc_key.is_some() {
            SHARD_MAGIC_V2
        } else {
            SHARD_MAGIC_V1
        };
        header[..8].copy_from_slice(magic);
        header[8..16].copy_from_slice(&k.0.to_be_bytes());
        header[16] = k.1;
        header[17] = k.2;
        header[18..22].copy_from_slice(&(shard.coeffs.len() as u32).to_be_bytes());
        header[22..26].copy_from_slice(&(shard.payload.len() as u32).to_be_bytes());
        f.write_all(&header)?;

        // v1 = plaintext body; v2 = [nonce | ct+tag] under GCM with
        // the header as AAD.
        match enc_key {
            None => {
                f.write_all(&shard.coeffs)?;
                f.write_all(&shard.payload)?;
            }
            Some(key) => {
                let mut plaintext = Vec::with_capacity(shard.coeffs.len() + shard.payload.len());
                plaintext.extend_from_slice(&shard.coeffs);
                plaintext.extend_from_slice(&shard.payload);
                let sealed = crate::crypto::encrypt(key, &header, &plaintext);
                f.write_all(&sealed)?;
            }
        }
        f.sync_all()?;
    }
    fs::rename(&tmp, &path)?;
    Ok(())
}

fn read_shard_file(
    path: &Path,
    enc_key: Option<&[u8; crate::crypto::KEY_LEN]>,
) -> io::Result<(Key, Hash, Shard)> {
    let mut f = fs::File::open(path)?;
    let mut header = [0u8; 26];
    f.read_exact(&mut header)?;
    let magic = &header[..8];
    let object_id = u64::from_be_bytes(header[8..16].try_into().unwrap());
    let channel = header[16];
    let layer = header[17];
    let coeffs_len = u32::from_be_bytes(header[18..22].try_into().unwrap()) as usize;
    let payload_len = u32::from_be_bytes(header[22..26].try_into().unwrap()) as usize;

    let (coeffs, payload) = if magic == SHARD_MAGIC_V1 {
        // Legacy plaintext file — enc_key is irrelevant.
        let mut coeffs = vec![0u8; coeffs_len];
        f.read_exact(&mut coeffs)?;
        let mut payload = vec![0u8; payload_len];
        f.read_exact(&mut payload)?;
        (coeffs, payload)
    } else if magic == SHARD_MAGIC_V2 {
        // Sealed file — need the key or we can't recover the body.
        let key = enc_key.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "shard file is sealed (HOLOFSS2) but no encryption key configured",
            )
        })?;
        let mut sealed = Vec::new();
        f.read_to_end(&mut sealed)?;
        let plaintext = crate::crypto::decrypt(key, &header, &sealed).map_err(|e| {
            io::Error::new(io::ErrorKind::InvalidData, format!("shard decrypt: {e}"))
        })?;
        if plaintext.len() != coeffs_len + payload_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "decrypted shard length mismatch: got {}, header wants {}+{}",
                    plaintext.len(),
                    coeffs_len,
                    payload_len,
                ),
            ));
        }
        let (c_slice, p_slice) = plaintext.split_at(coeffs_len);
        (c_slice.to_vec(), p_slice.to_vec())
    } else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a holofs shard file (unknown magic)",
        ));
    };

    let shard = Shard { coeffs, payload };
    let h = shard_hash(&shard);
    Ok(((object_id, channel, layer), h, shard))
}

fn walk_shard_files(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for top in fs::read_dir(dir)? {
        let top = top?;
        if !top.file_type()?.is_dir() {
            continue;
        }
        for inner in fs::read_dir(top.path())? {
            let inner = inner?;
            let p = inner.path();
            if p.extension().and_then(|s| s.to_str()) == Some("shard") {
                out.push(p);
            }
        }
    }
    Ok(out)
}

pub type SharedStore = Arc<Mutex<Store>>;

/// What `spawn_node` returns to callers: address, store, node identity, handle.
/// The identity is either randomly generated (in-memory mode) or loaded from
/// `storage_dir/identity.key` (persistent mode).
pub struct NodeHandle {
    pub addr: SocketAddr,
    pub store: SharedStore,
    pub identity: NodeIdentity,
    pub task: tokio::task::JoinHandle<()>,
}

/// Start an in-memory node: bind `addr` and serve frames. The identity is
/// randomly generated (it changes across restarts). For the legacy contract
/// `(addr, store, handle)` see the legacy wrapper below.
pub async fn spawn_node(
    addr: SocketAddr,
) -> io::Result<(SocketAddr, SharedStore, tokio::task::JoinHandle<()>)> {
    let h = spawn_node_with_identity(addr, Store::new(), NodeIdentity::generate(), None).await?;
    Ok((h.addr, h.store, h.task))
}

/// Start a persistent node: data lives in `storage_dir`, identity too
/// (`identity.key`), index is rebuilt from a filesystem scan.
pub async fn spawn_node_persistent(
    addr: SocketAddr,
    storage_dir: impl AsRef<Path>,
) -> io::Result<(SocketAddr, SharedStore, tokio::task::JoinHandle<()>)> {
    spawn_node_persistent_with_tls(addr, storage_dir, None).await
}

/// Same as [`spawn_node_persistent`] but wraps every accepted connection in
/// TLS using `tls`. `tls = None` falls back to plain TCP — backward
/// compatibility for callers that have not migrated to Stage 6 yet.
pub async fn spawn_node_persistent_with_tls(
    addr: SocketAddr,
    storage_dir: impl AsRef<Path>,
    tls: Option<Arc<rustls::ServerConfig>>,
) -> io::Result<(SocketAddr, SharedStore, tokio::task::JoinHandle<()>)> {
    let dir = storage_dir.as_ref().to_path_buf();
    // v0.7: opt in to at-rest shard encryption via
    // `HOLOFS_AT_REST_ENC=1`. Key material comes from the node's
    // own identity seed — no new secret to manage. Reads accept
    // both plaintext (v1) and sealed (v2) files, so nothing needs
    // to move on a rolling upgrade.
    let identity = NodeIdentity::load_or_create(dir.join("identity.key"))?;
    let at_rest_on = std::env::var("HOLOFS_AT_REST_ENC")
        .ok()
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);
    let store = if at_rest_on {
        let key = crate::crypto::derive_shard_key(&identity.to_bytes());
        Store::open_with_key(&dir, key)?
    } else {
        Store::open(&dir)?
    };
    let h = spawn_node_with_identity(addr, store, identity, tls).await?;
    Ok((h.addr, h.store, h.task))
}

/// Start an in-memory node with a specific identity. Returns a struct with
/// the node's pubkey — needed for building a whitelist.
pub async fn spawn_node_full(addr: SocketAddr) -> io::Result<NodeHandle> {
    spawn_node_with_identity(addr, Store::new(), NodeIdentity::generate(), None).await
}

async fn spawn_node_with_identity(
    addr: SocketAddr,
    store: Store,
    identity: NodeIdentity,
    tls: Option<Arc<rustls::ServerConfig>>,
) -> io::Result<NodeHandle> {
    let store: SharedStore = Arc::new(Mutex::new(store));
    let listener = TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    let store_for_task = store.clone();
    let identity_for_task = identity.clone();
    let acceptor = tls.clone().map(tokio_rustls::TlsAcceptor::from);
    let task = tokio::spawn(async move {
        loop {
            let (stream, _peer) = match listener.accept().await {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("accept: {e}");
                    return;
                }
            };
            let store = store_for_task.clone();
            let identity = identity_for_task.clone();
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let result = match acceptor {
                    None => handle_connection(stream, store, identity).await,
                    Some(acc) => match acc.accept(stream).await {
                        Ok(tls_stream) => {
                            handle_connection(tls_stream, store, identity).await
                        }
                        Err(e) => Err(io::Error::new(
                            io::ErrorKind::ConnectionAborted,
                            format!("TLS handshake: {e}"),
                        )),
                    },
                };
                if let Err(e) = result {
                    // Client connection closed — that's fine; we only log loud failures.
                    if e.kind() != io::ErrorKind::UnexpectedEof
                        && e.kind() != io::ErrorKind::ConnectionReset
                    {
                        eprintln!("node connection error: {e}");
                    }
                }
            });
        }
    });
    Ok(NodeHandle {
        addr: bound,
        store,
        identity,
        task,
    })
}

async fn handle_connection<S>(
    mut stream: S,
    store: SharedStore,
    identity: NodeIdentity,
) -> io::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    loop {
        let buf = read_frame(&mut stream).await?;
        let req = Request::decode(&buf)?;
        let resp = handle_request(req, &store, &identity).await;
        write_frame(&mut stream, &resp.encode()).await?;
    }
}

async fn handle_request(req: Request, store: &SharedStore, identity: &NodeIdentity) -> Response {
    match req {
        Request::Ping => Response::Pong,
        Request::Put {
            object_id,
            channel,
            layer,
            shard,
        } => {
            let mut s = store.lock().await;
            s.put((object_id, channel, layer), shard);
            Response::Ack
        }
        Request::Get {
            object_id,
            channel,
            layer,
        } => {
            let s = store.lock().await;
            Response::Shards(s.get((object_id, channel, layer)))
        }
        Request::Purge { object_id } => {
            let mut s = store.lock().await;
            s.purge(object_id);
            Response::Ack
        }
        Request::Stat => {
            let s = store.lock().await;
            Response::StatResp {
                total_shards: s.total() as u32,
            }
        }
        Request::Audit {
            object_id,
            channel,
            layer,
            shard_hash,
        } => {
            let s = store.lock().await;
            let shard = s.get_by_hash((object_id, channel, layer), &shard_hash);
            Response::AuditResp { shard }
        }
        Request::AuthChallenge { nonce } => {
            let signature = identity.sign_challenge(&nonce);
            Response::AuthChallengeOk { signature }
        }
        Request::ListHashes => {
            let s = store.lock().await;
            Response::Hashes(s.list_all_hashes())
        }
        Request::PurgeByHash { hashes } => {
            let set: std::collections::HashSet<Hash> = hashes.into_iter().collect();
            let mut s = store.lock().await;
            s.purge_by_hashes(&set);
            Response::Ack
        }
        Request::PutBatch {
            object_id,
            channel,
            layer,
            shards,
        } => {
            let mut s = store.lock().await;
            for shard in shards {
                s.put((object_id, channel, layer), shard);
            }
            Response::Ack
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    fn make_shard(seed: u8) -> Shard {
        Shard {
            coeffs: vec![seed, seed.wrapping_add(1), seed.wrapping_add(2)],
            payload: vec![0xAA, 0xBB, seed, seed.wrapping_add(7)],
        }
    }

    async fn rpc(stream: &mut TcpStream, req: Request) -> Response {
        let bytes = req.encode();
        let len = bytes.len() as u32;
        stream.write_all(&len.to_be_bytes()).await.unwrap();
        stream.write_all(&bytes).await.unwrap();
        let mut lb = [0u8; 4];
        stream.read_exact(&mut lb).await.unwrap();
        let n = u32::from_be_bytes(lb) as usize;
        let mut payload = vec![0u8; n];
        stream.read_exact(&mut payload).await.unwrap();
        Response::decode(&payload).unwrap()
    }

    #[tokio::test]
    async fn ping_pong() {
        let (addr, _store, _h) = spawn_node((Ipv4Addr::LOCALHOST, 0).into()).await.unwrap();
        let mut s = TcpStream::connect(addr).await.unwrap();
        assert_eq!(rpc(&mut s, Request::Ping).await, Response::Pong);
    }

    #[tokio::test]
    async fn put_then_get_returns_shards() {
        let (addr, _store, _h) = spawn_node((Ipv4Addr::LOCALHOST, 0).into()).await.unwrap();
        let mut s = TcpStream::connect(addr).await.unwrap();
        let sh1 = make_shard(1);
        let sh2 = make_shard(2);
        rpc(
            &mut s,
            Request::Put {
                object_id: 7,
                channel: 0,
                layer: 1,
                shard: sh1.clone(),
            },
        )
        .await;
        rpc(
            &mut s,
            Request::Put {
                object_id: 7,
                channel: 0,
                layer: 1,
                shard: sh2.clone(),
            },
        )
        .await;
        match rpc(
            &mut s,
            Request::Get {
                object_id: 7,
                channel: 0,
                layer: 1,
            },
        )
        .await
        {
            Response::Shards(v) => {
                assert_eq!(v.len(), 2);
                // HashMap order is undefined — compare as a set.
                let set: std::collections::HashSet<_> = v.into_iter().collect();
                assert!(set.contains(&sh1) && set.contains(&sh2));
            }
            other => panic!("expected Shards, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn put_dedupes_identical_shards() {
        let (addr, _store, _h) = spawn_node((Ipv4Addr::LOCALHOST, 0).into()).await.unwrap();
        let mut s = TcpStream::connect(addr).await.unwrap();
        let sh = make_shard(42);
        for _ in 0..5 {
            rpc(
                &mut s,
                Request::Put {
                    object_id: 1,
                    channel: 0,
                    layer: 0,
                    shard: sh.clone(),
                },
            )
            .await;
        }
        // All 5 are identical → only 1 must remain.
        assert_eq!(
            rpc(&mut s, Request::Stat).await,
            Response::StatResp { total_shards: 1 }
        );
    }

    #[tokio::test]
    async fn purge_removes_only_target_object() {
        let (addr, _store, _h) = spawn_node((Ipv4Addr::LOCALHOST, 0).into()).await.unwrap();
        let mut s = TcpStream::connect(addr).await.unwrap();
        for obj in [10u64, 20] {
            rpc(
                &mut s,
                Request::Put {
                    object_id: obj,
                    channel: 0,
                    layer: 0,
                    shard: make_shard(obj as u8),
                },
            )
            .await;
        }
        rpc(&mut s, Request::Purge { object_id: 10 }).await;
        let stat = rpc(&mut s, Request::Stat).await;
        assert_eq!(stat, Response::StatResp { total_shards: 1 });
        let v = rpc(
            &mut s,
            Request::Get {
                object_id: 20,
                channel: 0,
                layer: 0,
            },
        )
        .await;
        match v {
            Response::Shards(s) => assert_eq!(s.len(), 1),
            o => panic!("{o:?}"),
        }
    }

    fn tmpdir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "holofs-test-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn persistent_store_roundtrip_through_restart() {
        let dir = tmpdir("roundtrip");

        // Round 1: open, write a couple of shards, close.
        let mut s1 = Store::open(&dir).unwrap();
        let sh1 = make_shard(1);
        let sh2 = make_shard(2);
        assert!(s1.put((42, 0, 1), sh1.clone()));
        assert!(s1.put((42, 0, 1), sh2.clone()));
        assert_eq!(s1.total(), 2);
        drop(s1);

        // Round 2: open again — index is rebuilt from files.
        let s2 = Store::open(&dir).unwrap();
        assert_eq!(s2.total(), 2);
        let got = s2.get((42, 0, 1));
        let set: std::collections::HashSet<_> = got.into_iter().collect();
        assert!(set.contains(&sh1) && set.contains(&sh2));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn encrypted_store_roundtrip_and_disk_ciphertext() {
        // v0.7: writing under `open_with_key` yields HOLOFSS2 files.
        // Reading them back reconstructs the shards; and inspecting
        // the raw bytes confirms the payload is not plaintext.
        let dir = tmpdir("enc-roundtrip");
        let key = crate::crypto::derive_shard_key(&[0xEE; 32]);

        // Round 1: sealed writes.
        let mut s1 = Store::open_with_key(&dir, key).unwrap();
        let sh = make_shard(0xAA);
        assert!(s1.put((100, 1, 2), sh.clone()));
        drop(s1);

        // File exists and starts with the v0.7 magic.
        let files = walk_shard_files(&dir).unwrap();
        assert_eq!(files.len(), 1);
        let raw = std::fs::read(&files[0]).unwrap();
        assert_eq!(&raw[..8], b"HOLOFSS2");
        // The plaintext payload bytes must NOT appear verbatim on disk.
        assert!(
            !raw.windows(sh.payload.len()).any(|w| w == sh.payload.as_slice()),
            "encrypted file contains plaintext payload — GCM broken?"
        );

        // Round 2: opening WITH the right key must succeed.
        let s2 = Store::open_with_key(&dir, key).unwrap();
        assert_eq!(s2.total(), 1);
        assert_eq!(s2.get((100, 1, 2)), vec![sh.clone()]);

        // Round 3: opening WITHOUT a key sees the file but can't
        // decrypt — it skips with an eprintln, so the index is empty.
        let s3 = Store::open(&dir).unwrap();
        assert_eq!(s3.total(), 0);

        // Round 4: WRONG key also fails to decrypt.
        let wrong_key = crate::crypto::derive_shard_key(&[0xDD; 32]);
        let s4 = Store::open_with_key(&dir, wrong_key).unwrap();
        assert_eq!(s4.total(), 0);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mixed_format_directory_reads_both_v1_and_v2() {
        // A rolling upgrade leaves some legacy plaintext files
        // alongside new sealed ones. `open_with_key` must accept
        // both on read; only new writes go v2.
        let dir = tmpdir("mixed");
        let key = crate::crypto::derive_shard_key(&[0xEE; 32]);

        // Seed a legacy (v1) file.
        {
            let mut s0 = Store::open(&dir).unwrap();
            s0.put((1, 0, 0), make_shard(1));
        }
        // Open with key and add a sealed one.
        {
            let mut s1 = Store::open_with_key(&dir, key).unwrap();
            s1.put((2, 0, 0), make_shard(2));
        }
        // Re-open with key: both must be visible.
        let s2 = Store::open_with_key(&dir, key).unwrap();
        assert_eq!(s2.total(), 2);
        let all: std::collections::HashSet<_> = s2
            .get((1, 0, 0))
            .into_iter()
            .chain(s2.get((2, 0, 0)))
            .collect();
        assert!(all.contains(&make_shard(1)));
        assert!(all.contains(&make_shard(2)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn persistent_store_dedupes_on_disk() {
        let dir = tmpdir("dedupe");
        let mut s = Store::open(&dir).unwrap();
        let sh = make_shard(7);
        for _ in 0..5 {
            s.put((1, 0, 0), sh.clone());
        }
        assert_eq!(s.total(), 1);
        // Exactly one shard file must exist on disk too.
        let files: Vec<_> = walk_shard_files(&dir).unwrap();
        assert_eq!(files.len(), 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn persistent_purge_removes_files() {
        let dir = tmpdir("purge");
        let mut s = Store::open(&dir).unwrap();
        s.put((10, 0, 0), make_shard(1));
        s.put((20, 0, 0), make_shard(2));
        assert_eq!(walk_shard_files(&dir).unwrap().len(), 2);
        s.purge(10);
        assert_eq!(s.total(), 1);
        assert_eq!(walk_shard_files(&dir).unwrap().len(), 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn persistent_wipe_clears_files() {
        let dir = tmpdir("wipe");
        let mut s = Store::open(&dir).unwrap();
        for i in 0..4 {
            s.put((1, 0, 0), make_shard(i));
        }
        assert!(walk_shard_files(&dir).unwrap().len() >= 1);
        s.wipe();
        assert_eq!(s.total(), 0);
        assert_eq!(walk_shard_files(&dir).unwrap().len(), 0);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn persistent_spawn_node_survives_restart() {
        let dir = tmpdir("spawn-restart");

        // Round 1: start a persistent node, write shards via RPC, shut down.
        let (addr1, _store1, handle1) =
            spawn_node_persistent((Ipv4Addr::LOCALHOST, 0).into(), &dir)
                .await
                .unwrap();
        let mut s = TcpStream::connect(addr1).await.unwrap();
        let sh1 = make_shard(5);
        let sh2 = make_shard(9);
        rpc(
            &mut s,
            Request::Put {
                object_id: 100,
                channel: 1,
                layer: 2,
                shard: sh1.clone(),
            },
        )
        .await;
        rpc(
            &mut s,
            Request::Put {
                object_id: 100,
                channel: 1,
                layer: 2,
                shard: sh2.clone(),
            },
        )
        .await;
        drop(s);
        handle1.abort();
        // Not strictly necessary, but give drop a tick to close the listener.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Round 2: same storage_dir — the data must be there.
        let (addr2, _store2, handle2) =
            spawn_node_persistent((Ipv4Addr::LOCALHOST, 0).into(), &dir)
                .await
                .unwrap();
        let mut s = TcpStream::connect(addr2).await.unwrap();
        let resp = rpc(&mut s, Request::Stat).await;
        assert_eq!(resp, Response::StatResp { total_shards: 2 });
        match rpc(
            &mut s,
            Request::Get {
                object_id: 100,
                channel: 1,
                layer: 2,
            },
        )
        .await
        {
            Response::Shards(v) => {
                let set: std::collections::HashSet<_> = v.into_iter().collect();
                assert!(set.contains(&sh1) && set.contains(&sh2));
            }
            other => panic!("expected Shards, got {other:?}"),
        }
        handle2.abort();

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn stat_counts_total() {
        let (addr, _store, _h) = spawn_node((Ipv4Addr::LOCALHOST, 0).into()).await.unwrap();
        let mut s = TcpStream::connect(addr).await.unwrap();
        for i in 0..7 {
            rpc(
                &mut s,
                Request::Put {
                    object_id: 1,
                    channel: 0,
                    layer: 0,
                    shard: make_shard(i),
                },
            )
            .await;
        }
        assert_eq!(
            rpc(&mut s, Request::Stat).await,
            Response::StatResp { total_shards: 7 }
        );
    }
}
