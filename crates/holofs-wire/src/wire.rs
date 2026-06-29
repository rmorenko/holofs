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
    /// Stage 7.1: Proof of Retrievability. Asks for a shard by hash;
    /// if the node actually stores it, returns it; otherwise `AuditResp { shard: None }`.
    /// The client verifies `sha256(shard) == shard_hash`.
    Audit {
        object_id: u64,
        channel: u8,
        layer: u8,
        shard_hash: Hash,
    },
    /// Stage 7.3: handshake challenge. The node replies with `AuthChallengeOk { signature }`,
    /// where `signature = sign(node_secret, b"holofs-auth-v1" || nonce)`.
    /// The client verifies it against the node's pubkey (from the whitelist).
    AuthChallenge {
        nonce: [u8; 32],
    },
    /// Stage 14.0: enumerate every shard hash this node holds. Used by
    /// the gateway's GC pass; node replies with [`Response::Hashes`].
    ListHashes,
    /// Stage 14.0: delete every shard whose hash is in `hashes`. The
    /// node replies with [`Response::Ack`] regardless of whether the
    /// hashes actually existed (idempotent).
    PurgeByHash {
        hashes: Vec<Hash>,
    },
    /// Stage 15.0: batched PUT — store every shard in `shards` under
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    Pong,
    Ack,
    Shards(Vec<Shard>),
    StatResp {
        total_shards: u32,
    },
    /// Stage 7.1: reply to `Request::Audit`. `None` means the node does not hold this shard.
    AuditResp {
        shard: Option<Shard>,
    },
    /// Stage 7.3: reply to `AuthChallenge`. Signature over the nonce under the auth domain.
    AuthChallengeOk {
        signature: [u8; 64],
    },
    /// Stage 14.0: enumeration response — every shard hash this node
    /// currently holds, no ordering guarantees.
    Hashes(Vec<Hash>),
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

const RSP_PONG: u8 = 0x00;
const RSP_ACK: u8 = 0x01;
const RSP_SHARDS: u8 = 0x02;
const RSP_STAT: u8 = 0x03;
const RSP_AUDIT: u8 = 0x04;
const RSP_AUTH: u8 = 0x05;
const RSP_HASHES: u8 = 0x06;
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
        }
        b
    }

    pub fn decode(buf: &[u8]) -> io::Result<Self> {
        let mut c = Cursor::new(buf);
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
                let mut hashes = Vec::with_capacity(n);
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
                let mut shards = Vec::with_capacity(n);
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
        let mut c = Cursor::new(buf);
        let tag = c.u8()?;
        match tag {
            RSP_PONG => Ok(Response::Pong),
            RSP_ACK => Ok(Response::Ack),
            RSP_SHARDS => {
                let n = c.u32()? as usize;
                let mut v = Vec::with_capacity(n);
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
                let mut hashes = Vec::with_capacity(n);
                for _ in 0..n {
                    let raw = c.take(32)?;
                    let mut h = [0u8; 32];
                    h.copy_from_slice(raw);
                    hashes.push(h);
                }
                Ok(Response::Hashes(hashes))
            }
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

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Cursor { buf, pos: 0 }
    }
    fn u8(&mut self) -> io::Result<u8> {
        let b = self.take(1)?;
        Ok(b[0])
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
                "buffer truncated",
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
}
