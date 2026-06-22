//! # Markov Chain Correlation Predictor
//!
//! Tracks sequential access patterns (Block A -> Block B) to predict future reads.
//! Designed for high performance, no allocations during prediction, thread-safe.

use parking_lot::RwLock;

const NUM_SHARDS: usize = 64;
const SHARD_MASK: usize = NUM_SHARDS - 1;
const PREDICTOR_ENTRIES_PER_SHARD: usize = 8192;
const PREDICTOR_MASK: usize = PREDICTOR_ENTRIES_PER_SHARD - 1;

#[derive(Clone, Copy)]
struct PredictorEntry {
    block_a: u64,
    block_b: u64,
    confidence: u8,
}

pub struct MarkovPredictor {
    shards: Vec<RwLock<Vec<PredictorEntry>>>,
}

impl MarkovPredictor {
    pub fn new() -> Self {
        let mut shards = Vec::with_capacity(NUM_SHARDS);
        for _ in 0..NUM_SHARDS {
            shards.push(RwLock::new(vec![
                PredictorEntry {
                    block_a: u64::MAX,
                    block_b: u64::MAX,
                    confidence: 0
                };
                PREDICTOR_ENTRIES_PER_SHARD
            ]));
        }
        Self { shards }
    }

    #[inline]
    fn hash(block: u64) -> usize {
        let mut x = block;
        x ^= x >> 30;
        x = x.wrapping_mul(0xbf58476d1ce4e5b9);
        x ^= x >> 27;
        x = x.wrapping_mul(0x94d049bb133111eb);
        x ^= x >> 31;
        x as usize
    }

    /// Record a transition from block_a to block_b
    pub fn record_transition(&self, block_a: u64, block_b: u64) {
        if block_a == block_b || block_a == u64::MAX {
            return;
        }

        let h = Self::hash(block_a);
        let shard_idx = h & SHARD_MASK;
        let entry_idx = (h >> 6) & PREDICTOR_MASK;

        let mut shard = self.shards[shard_idx].write();
        let entry = &mut shard[entry_idx];

        if entry.block_a == block_a {
            if entry.block_b == block_b {
                entry.confidence = entry.confidence.saturating_add(1).min(10);
            } else {
                if entry.confidence > 0 {
                    entry.confidence -= 1;
                } else {
                    entry.block_b = block_b;
                    entry.confidence = 1;
                }
            }
        } else {
            // Replace entry
            entry.block_a = block_a;
            entry.block_b = block_b;
            entry.confidence = 1;
        }
    }

    /// Predict the next block given block_a
    pub fn predict(&self, block_a: u64) -> Option<u64> {
        let h = Self::hash(block_a);
        let shard_idx = h & SHARD_MASK;
        let entry_idx = (h >> 6) & PREDICTOR_MASK;

        let shard = self.shards[shard_idx].read();
        let entry = &shard[entry_idx];

        // Return prediction if we have seen it at least twice (confidence >= 2)
        if entry.block_a == block_a && entry.confidence >= 2 {
            Some(entry.block_b)
        } else {
            None
        }
    }
}
