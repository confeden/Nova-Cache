//! # Cache Statistics Module
//!
//! Provides real-time performance metrics for the Nova Cache system.
//!
//! All counters use atomic operations for lock-free thread-safe updates,
//! making statistics collection zero-overhead in the hot path.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

/// Relaxed ordering for statistics — we don't need strict ordering,
/// just eventual consistency for display purposes.
const ORD: Ordering = Ordering::Relaxed;

/// Snapshot of cache statistics at a point in time.
///
/// This is a serializable copy of the live atomic counters,
/// suitable for sending to the GUI or logging.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatsSnapshot {
    /// Total number of read requests received.
    pub total_reads: u64,
    /// Total number of write requests received.
    pub total_writes: u64,
    /// Number of read hits in L1 (RAM) cache.
    pub l1_hits: u64,
    /// Number of read hits in L2 (SSD) cache.
    pub l2_hits: u64,
    /// Number of cache misses (had to read from HDD).
    pub misses: u64,
    /// Total bytes served from L1 cache.
    pub bytes_from_l1: u64,
    /// Total bytes served from L2 cache.
    pub bytes_from_l2: u64,
    /// Total bytes read from the backing HDD.
    pub bytes_from_disk: u64,
    /// Total bytes written through cache.
    pub bytes_written: u64,
    /// Number of blocks evicted from L1.
    pub l1_evictions: u64,
    /// Number of blocks evicted from L2.
    pub l2_evictions: u64,
    /// Number of blocks promoted from L2 to L1.
    pub l2_to_l1_promotions: u64,
    /// Number of blocks demoted from L1 to L2.
    pub l1_to_l2_demotions: u64,
    /// Number of dirty blocks flushed to disk.
    pub dirty_flushes: u64,
    /// Current number of blocks in L1 cache.
    pub l1_block_count: u64,
    /// Current number of blocks in L2 cache.
    pub l2_block_count: u64,
    /// Maximum L1 capacity in blocks.
    pub l1_capacity: u64,
    /// Maximum L2 capacity in blocks.
    pub l2_capacity: u64,
    /// Number of game-priority reads.
    pub game_reads: u64,
    /// Number of sequential stream detections.
    pub sequential_detections: u64,
    /// Number of prefetch operations initiated.
    pub prefetch_ops: u64,
    /// Uptime in seconds since cache was started.
    pub uptime_secs: f64,
}

impl StatsSnapshot {
    /// Overall cache hit rate as a percentage (0.0–100.0).
    pub fn hit_rate(&self) -> f64 {
        if self.total_reads == 0 {
            return 0.0;
        }
        let hits = self.l1_hits + self.l2_hits;
        (hits as f64 / self.total_reads as f64) * 100.0
    }

    /// L1-only hit rate as a percentage.
    pub fn l1_hit_rate(&self) -> f64 {
        if self.total_reads == 0 {
            return 0.0;
        }
        (self.l1_hits as f64 / self.total_reads as f64) * 100.0
    }

    /// L2-only hit rate as a percentage.
    pub fn l2_hit_rate(&self) -> f64 {
        if self.total_reads == 0 {
            return 0.0;
        }
        (self.l2_hits as f64 / self.total_reads as f64) * 100.0
    }

    /// L1 cache fill percentage.
    pub fn l1_fill_percent(&self) -> f64 {
        if self.l1_capacity == 0 {
            return 0.0;
        }
        (self.l1_block_count as f64 / self.l1_capacity as f64) * 100.0
    }

    /// L2 cache fill percentage.
    pub fn l2_fill_percent(&self) -> f64 {
        if self.l2_capacity == 0 {
            return 0.0;
        }
        (self.l2_block_count as f64 / self.l2_capacity as f64) * 100.0
    }

    /// Estimated time saved by cache hits.
    ///
    /// Assumes HDD random read latency of ~10ms and SSD of ~0.1ms.
    pub fn estimated_time_saved_secs(&self) -> f64 {
        let hdd_latency_ms = 10.0;
        let ssd_latency_ms = 0.1;
        let ram_latency_ms = 0.001;

        let l1_saved = self.l1_hits as f64 * (hdd_latency_ms - ram_latency_ms) / 1000.0;
        let l2_saved = self.l2_hits as f64 * (hdd_latency_ms - ssd_latency_ms) / 1000.0;

        l1_saved + l2_saved
    }

    /// Throughput in bytes/second from cache (L1 + L2).
    pub fn cached_throughput_bps(&self) -> f64 {
        if self.uptime_secs <= 0.0 {
            return 0.0;
        }
        (self.bytes_from_l1 + self.bytes_from_l2) as f64 / self.uptime_secs
    }
}

/// Live cache statistics with atomic counters.
///
/// This structure is designed for zero-overhead statistics collection
/// in the hot path (I/O callbacks). All updates use relaxed atomic
/// ordering since we only need eventual consistency for display.
pub struct CacheStats {
    pub total_reads: AtomicU64,
    pub total_writes: AtomicU64,
    pub l1_hits: AtomicU64,
    pub l2_hits: AtomicU64,
    pub misses: AtomicU64,
    pub bytes_from_l1: AtomicU64,
    pub bytes_from_l2: AtomicU64,
    pub bytes_from_disk: AtomicU64,
    pub bytes_written: AtomicU64,
    pub l1_evictions: AtomicU64,
    pub l2_evictions: AtomicU64,
    pub l2_to_l1_promotions: AtomicU64,
    pub l1_to_l2_demotions: AtomicU64,
    pub dirty_flushes: AtomicU64,
    pub l1_block_count: AtomicU64,
    pub l2_block_count: AtomicU64,
    pub l1_capacity: AtomicU64,
    pub l2_capacity: AtomicU64,
    pub game_reads: AtomicU64,
    pub sequential_detections: AtomicU64,
    pub prefetch_ops: AtomicU64,
    pub prefetch_useful: AtomicU64,
    pub prefetch_wasted: AtomicU64,
    start_time: RwLock<Instant>,
}

impl CacheStats {
    /// Create a new stats tracker.
    pub fn new() -> Self {
        Self {
            total_reads: AtomicU64::new(0),
            total_writes: AtomicU64::new(0),
            l1_hits: AtomicU64::new(0),
            l2_hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            bytes_from_l1: AtomicU64::new(0),
            bytes_from_l2: AtomicU64::new(0),
            bytes_from_disk: AtomicU64::new(0),
            bytes_written: AtomicU64::new(0),
            l1_evictions: AtomicU64::new(0),
            l2_evictions: AtomicU64::new(0),
            l2_to_l1_promotions: AtomicU64::new(0),
            l1_to_l2_demotions: AtomicU64::new(0),
            dirty_flushes: AtomicU64::new(0),
            l1_block_count: AtomicU64::new(0),
            l2_block_count: AtomicU64::new(0),
            l1_capacity: AtomicU64::new(0),
            l2_capacity: AtomicU64::new(0),
            game_reads: AtomicU64::new(0),
            sequential_detections: AtomicU64::new(0),
            prefetch_ops: AtomicU64::new(0),
            prefetch_useful: AtomicU64::new(0),
            prefetch_wasted: AtomicU64::new(0),
            start_time: RwLock::new(Instant::now()),
        }
    }

    /// Record a read request.
    #[inline]
    pub fn record_read(&self) {
        self.total_reads.fetch_add(1, ORD);
    }

    /// Record a write request.
    #[inline]
    pub fn record_write(&self, bytes: u64) {
        self.total_writes.fetch_add(1, ORD);
        self.bytes_written.fetch_add(bytes, ORD);
    }

    /// Record an L1 cache hit.
    #[inline]
    pub fn record_l1_hit(&self, bytes: u64) {
        self.l1_hits.fetch_add(1, ORD);
        self.bytes_from_l1.fetch_add(bytes, ORD);
    }

    /// Record an L2 cache hit.
    #[inline]
    pub fn record_l2_hit(&self, bytes: u64) {
        self.l2_hits.fetch_add(1, ORD);
        self.bytes_from_l2.fetch_add(bytes, ORD);
    }

    /// Record a cache miss.
    #[inline]
    pub fn record_miss(&self, bytes_from_disk: u64) {
        self.misses.fetch_add(1, ORD);
        self.bytes_from_disk.fetch_add(bytes_from_disk, ORD);
    }

    /// Record a game-priority read.
    #[inline]
    pub fn record_game_read(&self) {
        self.game_reads.fetch_add(1, ORD);
    }

    /// Record a sequential stream detection.
    #[inline]
    pub fn record_sequential_detection(&self) {
        self.sequential_detections.fetch_add(1, ORD);
    }

    /// Record a prefetch operation.
    #[inline]
    pub fn record_prefetch(&self) {
        self.prefetch_ops.fetch_add(1, ORD);
    }

    /// Set L1 capacity.
    pub fn set_l1_capacity(&self, blocks: u64) {
        self.l1_capacity.store(blocks, ORD);
    }

    /// Set L2 capacity.
    pub fn set_l2_capacity(&self, blocks: u64) {
        self.l2_capacity.store(blocks, ORD);
    }

    /// Take a snapshot of current statistics.
    pub fn snapshot(&self) -> StatsSnapshot {
        let uptime = self.start_time.read().elapsed().as_secs_f64();
        StatsSnapshot {
            total_reads: self.total_reads.load(ORD),
            total_writes: self.total_writes.load(ORD),
            l1_hits: self.l1_hits.load(ORD),
            l2_hits: self.l2_hits.load(ORD),
            misses: self.misses.load(ORD),
            bytes_from_l1: self.bytes_from_l1.load(ORD),
            bytes_from_l2: self.bytes_from_l2.load(ORD),
            bytes_from_disk: self.bytes_from_disk.load(ORD),
            bytes_written: self.bytes_written.load(ORD),
            l1_evictions: self.l1_evictions.load(ORD),
            l2_evictions: self.l2_evictions.load(ORD),
            l2_to_l1_promotions: self.l2_to_l1_promotions.load(ORD),
            l1_to_l2_demotions: self.l1_to_l2_demotions.load(ORD),
            dirty_flushes: self.dirty_flushes.load(ORD),
            l1_block_count: self.l1_block_count.load(ORD),
            l2_block_count: self.l2_block_count.load(ORD),
            l1_capacity: self.l1_capacity.load(ORD),
            l2_capacity: self.l2_capacity.load(ORD),
            game_reads: self.game_reads.load(ORD),
            sequential_detections: self.sequential_detections.load(ORD),
            prefetch_ops: self.prefetch_ops.load(ORD),
            uptime_secs: uptime,
        }
    }

    /// Reset all counters.
    pub fn reset(&self) {
        self.total_reads.store(0, ORD);
        self.total_writes.store(0, ORD);
        self.l1_hits.store(0, ORD);
        self.l2_hits.store(0, ORD);
        self.misses.store(0, ORD);
        self.bytes_from_l1.store(0, ORD);
        self.bytes_from_l2.store(0, ORD);
        self.bytes_from_disk.store(0, ORD);
        self.bytes_written.store(0, ORD);
        self.l1_evictions.store(0, ORD);
        self.l2_evictions.store(0, ORD);
        self.l2_to_l1_promotions.store(0, ORD);
        self.l1_to_l2_demotions.store(0, ORD);
        self.dirty_flushes.store(0, ORD);
        self.game_reads.store(0, ORD);
        self.sequential_detections.store(0, ORD);
        self.prefetch_ops.store(0, ORD);
        *self.start_time.write() = Instant::now();
    }
}

impl Default for CacheStats {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stats_creation() {
        let stats = CacheStats::new();
        let snap = stats.snapshot();
        assert_eq!(snap.total_reads, 0);
        assert_eq!(snap.hit_rate(), 0.0);
    }

    #[test]
    fn test_hit_rate_calculation() {
        let stats = CacheStats::new();

        // 3 reads: 1 L1 hit, 1 L2 hit, 1 miss
        stats.record_read();
        stats.record_l1_hit(4096);
        stats.record_read();
        stats.record_l2_hit(4096);
        stats.record_read();
        stats.record_miss(4096);

        let snap = stats.snapshot();
        assert_eq!(snap.total_reads, 3);
        assert_eq!(snap.l1_hits, 1);
        assert_eq!(snap.l2_hits, 1);
        assert_eq!(snap.misses, 1);
        assert!((snap.hit_rate() - 66.666).abs() < 0.01);
        assert!((snap.l1_hit_rate() - 33.333).abs() < 0.01);
    }

    #[test]
    fn test_time_saved_estimation() {
        let stats = CacheStats::new();
        // 1000 L1 hits
        for _ in 0..1000 {
            stats.record_l1_hit(65536);
        }
        let snap = stats.snapshot();
        // 1000 * (10ms - 0.001ms) / 1000 = ~9.999 seconds saved
        assert!(snap.estimated_time_saved_secs() > 9.0);
    }

    #[test]
    fn test_fill_percent() {
        let stats = CacheStats::new();
        stats.set_l1_capacity(1000);
        stats.l1_block_count.store(500, Ordering::Relaxed);

        let snap = stats.snapshot();
        assert!((snap.l1_fill_percent() - 50.0).abs() < 0.01);
    }

    #[test]
    fn test_reset() {
        let stats = CacheStats::new();
        stats.record_read();
        stats.record_l1_hit(100);
        stats.reset();

        let snap = stats.snapshot();
        assert_eq!(snap.total_reads, 0);
        assert_eq!(snap.l1_hits, 0);
    }

    #[test]
    fn test_concurrent_updates() {
        use std::sync::Arc;
        use std::thread;

        let stats = Arc::new(CacheStats::new());
        let mut handles = vec![];

        for _ in 0..10 {
            let s = stats.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..1000 {
                    s.record_read();
                    s.record_l1_hit(64);
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        let snap = stats.snapshot();
        assert_eq!(snap.total_reads, 10_000);
        assert_eq!(snap.l1_hits, 10_000);
    }
}
