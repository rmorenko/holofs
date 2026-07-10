//! Binary holofs protocol over TCP.
//!
//! Frame: `[u32 len BE][len bytes payload]`. Payload is a Request or Response
//! in a custom serialisation. Maximum frame size is 64 MB.

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use holofs_core::merkle::Hash;
use holofs_core::rlnc::Shard;

pub const MAX_FRAME: usize = 64 * 1024 * 1024;

// === Frame =================================================================

pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, bytes: &[u8]) -> io::Result<()> {
    if bytes.len() > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "frame too large",
        ));
    }
    w.write_u32(bytes.len() as u32).await?;
    w.write_all(bytes).await?;
    Ok(())
}

pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<Vec<u8>> {
    let len = r.read_u32().await? as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    Ok(buf)
}

// === Requests / responses ==================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Ping,
    Put {
        object_id: u64,
        channel: u8,
        layer: u8,
        shard: Shard,
    },
    Get {
        object_id: u64,
        channel: u8,
        layer: u8,
    },
    Purge {
        object_id: u64,
    },
    Stat,
    /// Proof of Retrievability. Asks for a shard by hash;
    /// if the node actually stores it, returns it; otherwise `AuditResp { shard: None }`.
    /// The client verifies `sha256(shard) == shard_hash`.
    Audit {
        object_id: u64,
        channel: u8,
        layer: u8,
        shard_hash: Hash,
    },
    /// handshake challenge. The node replies with `AuthChallengeOk { signature }`,
    /// where `signature = sign(node_secret, b"holofs-auth-v1" || nonce)`.
    /// The client verifies it against the node's pubkey (from the whitelist).
    AuthChallenge {
        nonce: [u8; 32],
    },
    /// enumerate every shard hash this node holds. Used by
    /// the gateway's GC pass; node replies with [`Response::Hashes`].
    ListHashes,
    /// delete every shard whose hash is in `hashes`. The
    /// node replies with [`Response::Ack`] regardless of whether the
    /// hashes actually existed (idempotent).
    PurgeByHash {
        hashes: Vec<Hash>,
    },
    /// batched PUT — store every shard in `shards` under
    /// the same `(object_id, channel, layer)` bucket. Single Ack on
    /// success. Used by the replicated-encoding path where a single
    /// `(object_id, channel, layer)` can carry thousands of one-
    /// coefficient shards; sending each as its own RPC exhausts
    /// ephemeral ports.
    PutBatch {
        object_id: u64,
        channel: u8,
        layer: u8,
        shards: Vec<Shard>,
    },
    /// epoch-GC: ask the node for its current write-epoch (wall
    /// clock, milliseconds since UNIX_EPOCH). The gateway snapshots
    /// this once at the start of a GC pass and uses it to gate
    /// [`Request::PurgeByHashUpTo`] — shards written after the
    /// snapshot are protected from concurrent purge.
    CurrentEpoch,
    /// epoch-GC: same semantics as [`Request::PurgeByHash`] but
    /// the node only removes hashes whose stored write-epoch is
    /// ≤ `max_epoch`. Fresh writes that landed after the snapshot
    /// (epoch > max_epoch) survive.
    PurgeByHashUpTo {
        hashes: Vec<Hash>,
        max_epoch: u64,
    },
    /// diagnostic: dump the node's process-wide breakdown of PUT
    /// wall time — `(lock_wait, put_appended, wal_wait, count)` in
    /// nanoseconds. Added to trace fanout amplification under load.
    PutTimings,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    Pong,
    Ack,
    Shards(Vec<Shard>),
    StatResp {
        total_shards: u32,
    },
    /// reply to `Request::Audit`. `None` means the node does not hold this shard.
    AuditResp {
        shard: Option<Shard>,
    },
    /// reply to `AuthChallenge`. Signature over the nonce under the auth domain.
    AuthChallengeOk {
        signature: [u8; 64],
    },
    /// enumeration response — every shard hash this node
    /// currently holds, no ordering guarantees.
    Hashes(Vec<Hash>),
    /// epoch-GC: reply to [`Request::CurrentEpoch`].
    Epoch {
        epoch: u64,
    },
    /// diagnostic: reply to [`Request::PutTimings`].
    PutTimings {
        lock_wait_ns: u64,
        put_appended_ns: u64,
        wal_wait_ns: u64,
        count: u64,
    },
    Error(String),
}

const OP_PING: u8 = 0x00;
const OP_PUT: u8 = 0x01;
const OP_GET: u8 = 0x02;
const OP_PURGE: u8 = 0x03;
const OP_STAT: u8 = 0x04;
const OP_AUDIT: u8 = 0x05;
const OP_AUTH: u8 = 0x06;
const OP_LIST_HASHES: u8 = 0x07;
const OP_PURGE_BY_HASH: u8 = 0x08;
const OP_PUT_BATCH: u8 = 0x09;
// epoch-GC ops.
const OP_CURRENT_EPOCH: u8 = 0x0a;
const OP_PURGE_BY_HASH_UP_TO: u8 = 0x0b;
const OP_PUT_TIMINGS: u8 = 0x0c;

const RSP_PONG: u8 = 0x00;
const RSP_ACK: u8 = 0x01;
const RSP_SHARDS: u8 = 0x02;
const RSP_STAT: u8 = 0x03;
const RSP_AUDIT: u8 = 0x04;
const RSP_AUTH: u8 = 0x05;
const RSP_HASHES: u8 = 0x06;
// epoch-GC: raw u64 write-epoch (ms since UNIX_EPOCH).
const RSP_EPOCH: u8 = 0x07;
const RSP_PUT_TIMINGS: u8 = 0x08;
const RSP_ERR: u8 = 0xff;

impl Request {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::new();
        match self {
            Request::Ping => b.push(OP_PING),
            Request::Put {
                object_id,
                channel,
                layer,
                shard,
            } => {
                b.push(OP_PUT);
                b.extend_from_slice(&object_id.to_be_bytes());
                b.push(*channel);
                b.push(*layer);
                encode_shard(&mut b, shard);
            }
            Request::Get {
                object_id,
                channel,
                layer,
            } => {
                b.push(OP_GET);
                b.extend_from_slice(&object_id.to_be_bytes());
                b.push(*channel);
                b.push(*layer);
            }
            Request::Purge { object_id } => {
                b.push(OP_PURGE);
                b.extend_from_slice(&object_id.to_be_bytes());
            }
            Request::Stat => b.push(OP_STAT),
            Request::Audit {
                object_id,
                channel,
                layer,
                shard_hash,
            } => {
                b.push(OP_AUDIT);
                b.extend_from_slice(&object_id.to_be_bytes());
                b.push(*channel);
                b.push(*layer);
                b.extend_from_slice(shard_hash);
            }
            Request::AuthChallenge { nonce } => {
                b.push(OP_AUTH);
                b.extend_from_slice(nonce);
            }
            Request::ListHashes => b.push(OP_LIST_HASHES),
            Request::PurgeByHash { hashes } => {
                b.push(OP_PURGE_BY_HASH);
                b.extend_from_slice(&(hashes.len() as u32).to_be_bytes());
                for h in hashes {
                    b.extend_from_slice(h);
                }
            }
            Request::PutBatch {
                object_id,
                channel,
                layer,
                shards,
            } => {
                b.push(OP_PUT_BATCH);
                b.extend_from_slice(&object_id.to_be_bytes());
                b.push(*channel);
                b.push(*layer);
                b.extend_from_slice(&(shards.len() as u32).to_be_bytes());
                for s in shards {
                    encode_shard(&mut b, s);
                }
            }
            Request::CurrentEpoch => b.push(OP_CURRENT_EPOCH),
            Request::PurgeByHashUpTo { hashes, max_epoch } => {
                b.push(OP_PURGE_BY_HASH_UP_TO);
                b.extend_from_slice(&max_epoch.to_be_bytes());
                b.extend_from_slice(&(hashes.len() as u32).to_be_bytes());
                for h in hashes {
                    b.extend_from_slice(h);
                }
            }
            Request::PutTimings => b.push(OP_PUT_TIMINGS),
        }
        b
    }

    pub fn decode(buf: &[u8]) -> io::Result<Self> {
        let mut c = make_cursor(buf);
        let op = c.u8()?;
        match op {
            OP_PING => Ok(Request::Ping),
            OP_PUT => Ok(Request::Put {
                object_id: c.u64()?,
                channel: c.u8()?,
                layer: c.u8()?,
                shard: decode_shard(&mut c)?,
            }),
            OP_GET => Ok(Request::Get {
                object_id: c.u64()?,
                channel: c.u8()?,
                layer: c.u8()?,
            }),
            OP_PURGE => Ok(Request::Purge {
                object_id: c.u64()?,
            }),
            OP_STAT => Ok(Request::Stat),
            OP_AUDIT => {
                let object_id = c.u64()?;
                let channel = c.u8()?;
                let layer = c.u8()?;
                let mut h = [0u8; 32];
                let raw = c.take(32)?;
                h.copy_from_slice(raw);
                Ok(Request::Audit {
                    object_id,
                    channel,
                    layer,
                    shard_hash: h,
                })
            }
            OP_AUTH => {
                let mut nonce = [0u8; 32];
                let raw = c.take(32)?;
                nonce.copy_from_slice(raw);
                Ok(Request::AuthChallenge { nonce })
            }
            OP_LIST_HASHES => Ok(Request::ListHashes),
            OP_PURGE_BY_HASH => {
                let n = c.u32()? as usize;
                let mut hashes = Vec::with_capacity(bounded_cap(n, HASH_MIN_SIZE, c.remaining()));
                for _ in 0..n {
                    let raw = c.take(32)?;
                    let mut h = [0u8; 32];
                    h.copy_from_slice(raw);
                    hashes.push(h);
                }
                Ok(Request::PurgeByHash { hashes })
            }
            OP_PUT_BATCH => {
                let object_id = c.u64()?;
                let channel = c.u8()?;
                let layer = c.u8()?;
                let n = c.u32()? as usize;
                let mut shards = Vec::with_capacity(bounded_cap(n, SHARD_MIN_SIZE, c.remaining()));
                for _ in 0..n {
                    shards.push(decode_shard(&mut c)?);
                }
                Ok(Request::PutBatch {
                    object_id,
                    channel,
                    layer,
                    shards,
                })
            }
            OP_CURRENT_EPOCH => Ok(Request::CurrentEpoch),
            OP_PURGE_BY_HASH_UP_TO => {
                let max_epoch = c.u64()?;
                let n = c.u32()? as usize;
                let mut hashes = Vec::with_capacity(bounded_cap(n, HASH_MIN_SIZE, c.remaining()));
                for _ in 0..n {
                    let raw = c.take(32)?;
                    let mut h = [0u8; 32];
                    h.copy_from_slice(raw);
                    hashes.push(h);
                }
                Ok(Request::PurgeByHashUpTo { hashes, max_epoch })
            }
            OP_PUT_TIMINGS => Ok(Request::PutTimings),
            other => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown op: {other:#x}"),
            )),
        }
    }
}

impl Response {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::new();
        match self {
            Response::Pong => b.push(RSP_PONG),
            Response::Ack => b.push(RSP_ACK),
            Response::Shards(shards) => {
                b.push(RSP_SHARDS);
                b.extend_from_slice(&(shards.len() as u32).to_be_bytes());
                for s in shards {
                    encode_shard(&mut b, s);
                }
            }
            Response::StatResp { total_shards } => {
                b.push(RSP_STAT);
                b.extend_from_slice(&total_shards.to_be_bytes());
            }
            Response::AuditResp { shard } => {
                b.push(RSP_AUDIT);
                match shard {
                    None => b.push(0),
                    Some(s) => {
                        b.push(1);
                        encode_shard(&mut b, s);
                    }
                }
            }
            Response::AuthChallengeOk { signature } => {
                b.push(RSP_AUTH);
                b.extend_from_slice(signature);
            }
            Response::Hashes(hashes) => {
                b.push(RSP_HASHES);
                b.extend_from_slice(&(hashes.len() as u32).to_be_bytes());
                for h in hashes {
                    b.extend_from_slice(h);
                }
            }
            Response::Epoch { epoch } => {
                b.push(RSP_EPOCH);
                b.extend_from_slice(&epoch.to_be_bytes());
            }
            Response::PutTimings {
                lock_wait_ns,
                put_appended_ns,
                wal_wait_ns,
                count,
            } => {
                b.push(RSP_PUT_TIMINGS);
                b.extend_from_slice(&lock_wait_ns.to_be_bytes());
                b.extend_from_slice(&put_appended_ns.to_be_bytes());
                b.extend_from_slice(&wal_wait_ns.to_be_bytes());
                b.extend_from_slice(&count.to_be_bytes());
            }
            Response::Error(msg) => {
                b.push(RSP_ERR);
                let bytes = msg.as_bytes();
                b.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
                b.extend_from_slice(bytes);
            }
        }
        b
    }

    pub fn decode(buf: &[u8]) -> io::Result<Self> {
        let mut c = make_cursor(buf);
        let tag = c.u8()?;
        match tag {
            RSP_PONG => Ok(Response::Pong),
            RSP_ACK => Ok(Response::Ack),
            RSP_SHARDS => {
                let n = c.u32()? as usize;
                let mut v = Vec::with_capacity(bounded_cap(n, SHARD_MIN_SIZE, c.remaining()));
                for _ in 0..n {
                    v.push(decode_shard(&mut c)?);
                }
                Ok(Response::Shards(v))
            }
            RSP_STAT => Ok(Response::StatResp {
                total_shards: c.u32()?,
            }),
            RSP_AUDIT => {
                let present = c.u8()? != 0;
                let shard = if present {
                    Some(decode_shard(&mut c)?)
                } else {
                    None
                };
                Ok(Response::AuditResp { shard })
            }
            RSP_AUTH => {
                let mut sig = [0u8; 64];
                let raw = c.take(64)?;
                sig.copy_from_slice(raw);
                Ok(Response::AuthChallengeOk { signature: sig })
            }
            RSP_HASHES => {
                let n = c.u32()? as usize;
                let mut hashes = Vec::with_capacity(bounded_cap(n, HASH_MIN_SIZE, c.remaining()));
                for _ in 0..n {
                    let raw = c.take(32)?;
                    let mut h = [0u8; 32];
                    h.copy_from_slice(raw);
                    hashes.push(h);
                }
                Ok(Response::Hashes(hashes))
            }
            RSP_EPOCH => Ok(Response::Epoch { epoch: c.u64()? }),
            RSP_PUT_TIMINGS => Ok(Response::PutTimings {
                lock_wait_ns: c.u64()?,
                put_appended_ns: c.u64()?,
                wal_wait_ns: c.u64()?,
                count: c.u64()?,
            }),
            RSP_ERR => {
                let n = c.u32()? as usize;
                let bytes = c.take(n)?;
                Ok(Response::Error(String::from_utf8_lossy(bytes).into_owned()))
            }
            other => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown response tag: {other:#x}"),
            )),
        }
    }
}

// === Shard serialisation ===================================================
// format: [u16 K BE][u32 plen BE][K bytes coeffs][plen bytes payload]

fn encode_shard(b: &mut Vec<u8>, s: &Shard) {
    b.extend_from_slice(&(s.coeffs.len() as u16).to_be_bytes());
    b.extend_from_slice(&(s.payload.len() as u32).to_be_bytes());
    b.extend_from_slice(&s.coeffs);
    b.extend_from_slice(&s.payload);
}

fn decode_shard(c: &mut Cursor<'_>) -> io::Result<Shard> {
    let k = c.u16()? as usize;
    let plen = c.u32()? as usize;
    let coeffs = c.take(k)?.to_vec();
    let payload = c.take(plen)?.to_vec();
    Ok(Shard { coeffs, payload })
}

// === Helper buffer cursor ==================================================
//
// v2 P4.3: the local `struct Cursor` extracted to
// [`holofs_core::cursor::BeCursor`] so both hand-rolled binary
// decoders in the workspace (wire + manifest) share one
// implementation. The alias keeps the callsite spellings inside this
// file unchanged.
type Cursor<'a> = holofs_core::cursor::BeCursor<'a>;

fn make_cursor(buf: &[u8]) -> Cursor<'_> {
    Cursor::new(buf, "buffer truncated")
}

/// Cap a wire-supplied element count `n` to what could physically fit in
/// the remaining bytes assuming `elem_min_size` per element. Guards
/// `Vec::with_capacity(n)` from turning a stray `u32::MAX` into a
/// terabyte reservation (aborting the process before we ever get to
/// [`Cursor::take`]'s length check). The subsequent per-element decode
/// still returns `UnexpectedEof` if `n` is dishonest.
fn bounded_cap(n: usize, elem_min_size: usize, remaining: usize) -> usize {
    if elem_min_size == 0 {
        return n;
    }
    n.min(remaining / elem_min_size)
}

/// Min encoded size of one wire `Hash` (= 32-byte SHA-256).
const HASH_MIN_SIZE: usize = 32;
/// Min encoded size of one wire `Shard` — `K u16` + `plen u32` header
/// with empty coeffs/payload. Real shards are much bigger, but this
/// keeps `bounded_cap` safe for degenerate frames.
const SHARD_MIN_SIZE: usize = 2 + 4;

#[cfg(test)]
mod tests {
    use super::*;

    fn s(coeffs: &[u8], payload: &[u8]) -> Shard {
        Shard {
            coeffs: coeffs.to_vec(),
            payload: payload.to_vec(),
        }
    }

    #[test]
    fn request_roundtrip_ping() {
        let r = Request::Ping;
        assert_eq!(Request::decode(&r.encode()).unwrap(), r);
    }

    #[test]
    fn request_roundtrip_put() {
        let r = Request::Put {
            object_id: 0xDEADBEEFCAFE,
            channel: 2,
            layer: 1,
            shard: s(&[1, 2, 3, 4], &[0xAA, 0xBB, 0xCC]),
        };
        assert_eq!(Request::decode(&r.encode()).unwrap(), r);
    }

    #[test]
    fn request_roundtrip_get_purge_stat() {
        let r1 = Request::Get {
            object_id: 42,
            channel: 0,
            layer: 3,
        };
        let r2 = Request::Purge { object_id: 7 };
        let r3 = Request::Stat;
        assert_eq!(Request::decode(&r1.encode()).unwrap(), r1);
        assert_eq!(Request::decode(&r2.encode()).unwrap(), r2);
        assert_eq!(Request::decode(&r3.encode()).unwrap(), r3);
    }

    #[test]
    fn response_roundtrip_all_variants() {
        for r in [
            Response::Pong,
            Response::Ack,
            Response::Shards(vec![s(&[1, 2], &[5, 6, 7]), s(&[9, 8], &[0, 0])]),
            Response::StatResp {
                total_shards: 12345,
            },
            Response::AuditResp { shard: None },
            Response::AuditResp {
                shard: Some(s(&[3, 4], &[7, 8, 9])),
            },
            Response::Error("oops".into()),
        ] {
            assert_eq!(Response::decode(&r.encode()).unwrap(), r);
        }
    }

    #[test]
    fn request_audit_roundtrip() {
        let r = Request::Audit {
            object_id: 0x1234,
            channel: 2,
            layer: 1,
            shard_hash: [0xAB; 32],
        };
        assert_eq!(Request::decode(&r.encode()).unwrap(), r);
    }

    #[test]
    fn request_auth_challenge_roundtrip() {
        let r = Request::AuthChallenge { nonce: [0x5A; 32] };
        assert_eq!(Request::decode(&r.encode()).unwrap(), r);
    }

    #[test]
    fn response_auth_challenge_ok_roundtrip() {
        let r = Response::AuthChallengeOk {
            signature: [0xCD; 64],
        };
        assert_eq!(Response::decode(&r.encode()).unwrap(), r);
    }

    #[test]
    fn decode_truncated_returns_error() {
        let mut bytes = Request::Get {
            object_id: 1,
            channel: 0,
            layer: 0,
        }
        .encode();
        bytes.truncate(bytes.len() - 1);
        assert!(Request::decode(&bytes).is_err());
    }

    #[test]
    fn decode_unknown_op_returns_error() {
        assert!(Request::decode(&[0xEE]).is_err());
        assert!(Response::decode(&[0xEE]).is_err());
    }

    #[tokio::test]
    async fn frame_roundtrip_over_pipe() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        let payload = vec![0xABu8; 257];
        let p2 = payload.clone();
        tokio::spawn(async move {
            write_frame(&mut a, &p2).await.unwrap();
        });
        let got = read_frame(&mut b).await.unwrap();
        assert_eq!(got, payload);
    }

    // --- Stage-14/15 ops that previously had no round-trip coverage. ---

    #[test]
    fn request_list_hashes_roundtrip() {
        let r = Request::ListHashes;
        assert_eq!(Request::decode(&r.encode()).unwrap(), r);
    }

    #[test]
    fn request_purge_by_hash_roundtrip() {
        let r = Request::PurgeByHash {
            hashes: vec![[0x11; 32], [0x22; 32], [0x33; 32]],
        };
        assert_eq!(Request::decode(&r.encode()).unwrap(), r);
    }

    #[test]
    fn request_put_batch_roundtrip() {
        let r = Request::PutBatch {
            object_id: 0xC0FFEE,
            channel: 1,
            layer: 2,
            shards: vec![s(&[1, 2, 3], &[10, 20]), s(&[4, 5], &[30])],
        };
        assert_eq!(Request::decode(&r.encode()).unwrap(), r);
    }

    #[test]
    fn response_hashes_roundtrip_including_empty() {
        let r1 = Response::Hashes(vec![]);
        let r2 = Response::Hashes(vec![[0xA5; 32], [0x5A; 32]]);
        assert_eq!(Response::decode(&r1.encode()).unwrap(), r1);
        assert_eq!(Response::decode(&r2.encode()).unwrap(), r2);
    }

    #[test]
    fn request_current_epoch_roundtrip() {
        let r = Request::CurrentEpoch;
        assert_eq!(Request::decode(&r.encode()).unwrap(), r);
    }

    #[test]
    fn request_purge_by_hash_up_to_roundtrip() {
        let r = Request::PurgeByHashUpTo {
            hashes: vec![[0x11; 32], [0x22; 32]],
            max_epoch: 0x0123_4567_89AB_CDEF,
        };
        assert_eq!(Request::decode(&r.encode()).unwrap(), r);
        // Empty hash list must roundtrip too — a purge with no
        // targets is legal (protocol-level no-op).
        let r_empty = Request::PurgeByHashUpTo {
            hashes: vec![],
            max_epoch: 42,
        };
        assert_eq!(Request::decode(&r_empty.encode()).unwrap(), r_empty);
    }

    #[test]
    fn response_epoch_roundtrip() {
        let r = Response::Epoch {
            epoch: 1_700_000_000_000,
        };
        assert_eq!(Response::decode(&r.encode()).unwrap(), r);
    }

    // --- Decode error paths. These weigh heavily on coverage because
    //     each `take` / cursor read has its own truncation branch.    ---

    #[test]
    fn request_put_truncated_at_each_field_returns_error() {
        let full = Request::Put {
            object_id: 0x1234,
            channel: 1,
            layer: 2,
            shard: s(&[1, 2], &[3, 4]),
        }
        .encode();
        // Step through each prefix length below the full encoding; every
        // truncation must surface as Err — no panic, no silent default.
        for cut in 1..full.len() {
            assert!(
                Request::decode(&full[..cut]).is_err(),
                "truncated PUT of len {cut} decoded successfully"
            );
        }
    }

    #[test]
    fn response_shards_truncated_returns_error() {
        let full = Response::Shards(vec![s(&[1, 2, 3], &[10, 20, 30])]).encode();
        assert!(Response::decode(&full[..full.len() - 1]).is_err());
    }

    #[test]
    fn empty_buffer_decodes_to_error() {
        assert!(Request::decode(&[]).is_err());
        assert!(Response::decode(&[]).is_err());
    }

    // --- Frame-layer error paths (write_frame / read_frame too-large). ---

    #[tokio::test]
    async fn write_frame_rejects_oversize_payload() {
        let (mut a, _b) = tokio::io::duplex(64);
        let oversize = vec![0u8; MAX_FRAME + 1];
        let e = write_frame(&mut a, &oversize).await.unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
    }

    #[tokio::test]
    async fn read_frame_rejects_oversize_header() {
        // Hand-craft a frame whose length prefix declares MAX+1 bytes.
        // read_frame must bail before allocating that buffer.
        let (mut a, mut b) = tokio::io::duplex(64);
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let len = (MAX_FRAME as u32) + 1;
            let _ = a.write_all(&len.to_be_bytes()).await;
        });
        let e = read_frame(&mut b).await.unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn read_frame_returns_eof_on_closed_stream() {
        // Drop the writer side without sending anything; read_u32 should
        // surface UnexpectedEof, not hang or panic.
        let (a, mut b) = tokio::io::duplex(64);
        drop(a);
        let e = read_frame(&mut b).await.unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof);
    }

    // --- B1 regression: dishonest u32 length must not OOM-abort ---
    //
    // Before the bounded_cap guard, a 5-byte OP_PURGE_BY_HASH frame
    // carrying n = 0xFFFFFFFF would call Vec::with_capacity(4 GiB / 32-
    // byte hashes = 4 * 10^9 entries), aborting the process before the
    // per-element take() could refuse the read. The counts below cover
    // every variant that trusts a wire-supplied length.
    #[test]
    fn decode_oversized_purge_by_hash_returns_error() {
        // OP + u32 count only — no hashes follow.
        let mut b = vec![OP_PURGE_BY_HASH];
        b.extend_from_slice(&u32::MAX.to_be_bytes());
        let e = Request::decode(&b).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn decode_oversized_purge_by_hash_up_to_returns_error() {
        // OP + u64 max_epoch + u32 count only.
        let mut b = vec![OP_PURGE_BY_HASH_UP_TO];
        b.extend_from_slice(&0u64.to_be_bytes());
        b.extend_from_slice(&u32::MAX.to_be_bytes());
        let e = Request::decode(&b).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn decode_oversized_put_batch_returns_error() {
        // OP + u64 object_id + u8 channel + u8 layer + u32 count only.
        let mut b = vec![OP_PUT_BATCH];
        b.extend_from_slice(&0u64.to_be_bytes());
        b.push(0);
        b.push(0);
        b.extend_from_slice(&u32::MAX.to_be_bytes());
        let e = Request::decode(&b).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn decode_oversized_response_shards_returns_error() {
        let mut b = vec![RSP_SHARDS];
        b.extend_from_slice(&u32::MAX.to_be_bytes());
        let e = Response::decode(&b).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn decode_oversized_response_hashes_returns_error() {
        let mut b = vec![RSP_HASHES];
        b.extend_from_slice(&u32::MAX.to_be_bytes());
        let e = Response::decode(&b).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof);
    }
}
