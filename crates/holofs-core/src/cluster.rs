//! Shard placement metadata: which node holds it and which layer it belongs to.

use crate::rlnc::Shard;

#[derive(Clone)]
pub struct ShardRec {
    pub node: usize,
    pub channel: usize,
    pub layer: usize,
    pub shard: Shard,
}
