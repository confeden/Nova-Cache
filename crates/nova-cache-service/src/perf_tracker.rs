use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// CRC32 (Castagnoli) using a precomputed table — same polynomial as Windows kernel's RtlComputeCrc32
const CRC32C_TABLE: [u32; 256] = generate_crc32c_table();

const fn generate_crc32c_table() -> [u32; 256] {
    let poly = 0x82F63B78u32; // CRC-32C polynomial
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut j = 0;
        while j < 8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ poly;
            } else {
                crc >>= 1;
            }
            j += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

pub fn crc32c(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFFFFFF;
    for &byte in data {
        crc = CRC32C_TABLE[((crc ^ byte as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFFFFFF
}

/// Tracks performance statistics: latency, throughput, and boost %
pub struct PerformanceTracker {
    // Counters
    pub total_hdd_read_time_ns: AtomicU64,
    pub total_cache_read_time_ns: AtomicU64,
    pub total_hdd_write_time_ns: AtomicU64,
    pub total_cache_write_time_ns: AtomicU64,

    pub total_hdd_read_ops: AtomicU64,
    pub total_cache_read_ops: AtomicU64,
    pub total_hdd_write_ops: AtomicU64,
    pub total_cache_write_ops: AtomicU64,

    // Driver-reported counters (from shared memory header)
    pub driver_cache_hits: AtomicU64,
    pub driver_total_reads: AtomicU64,
    pub driver_total_writes: AtomicU64,

    // Performance counter frequency (from driver)
    pub perf_counter_freq: AtomicU64,

    pub start_time: Instant,
}

impl PerformanceTracker {
    pub fn new() -> Self {
        Self {
            total_hdd_read_time_ns: AtomicU64::new(0),
            total_cache_read_time_ns: AtomicU64::new(0),
            total_hdd_write_time_ns: AtomicU64::new(0),
            total_cache_write_time_ns: AtomicU64::new(0),

            total_hdd_read_ops: AtomicU64::new(0),
            total_cache_read_ops: AtomicU64::new(0),
            total_hdd_write_ops: AtomicU64::new(0),
            total_cache_write_ops: AtomicU64::new(0),

            driver_cache_hits: AtomicU64::new(0),
            driver_total_reads: AtomicU64::new(0),
            driver_total_writes: AtomicU64::new(0),

            perf_counter_freq: AtomicU64::new(0),

            start_time: Instant::now(),
        }
    }

    pub fn set_perf_counter_freq(&self, freq: u64) {
        self.perf_counter_freq.store(freq, Ordering::Relaxed);
    }

    pub fn ticks_to_ns(&self, ticks: u64) -> u64 {
        let freq = self.perf_counter_freq.load(Ordering::Relaxed);
        if freq == 0 {
            return ticks;
        }
        ticks * 1_000_000_000 / freq
    }

    pub fn record_hdd_read(&mut self, pre_op_tick: u64, post_op_tick: u64) {
        if post_op_tick > 0 {
            let latency_ns = if pre_op_tick > 0 && post_op_tick > pre_op_tick {
                self.ticks_to_ns(post_op_tick - pre_op_tick)
            } else {
                0
            };
            self.total_hdd_read_time_ns
                .fetch_add(latency_ns, Ordering::Relaxed);
            self.total_hdd_read_ops.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn record_hdd_write(&mut self, pre_op_tick: u64, post_op_tick: u64) {
        if post_op_tick > 0 {
            let latency_ns = if pre_op_tick > 0 && post_op_tick > pre_op_tick {
                self.ticks_to_ns(post_op_tick - pre_op_tick)
            } else {
                0
            };
            self.total_hdd_write_time_ns
                .fetch_add(latency_ns, Ordering::Relaxed);
            self.total_hdd_write_ops.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn record_cache_read(&mut self, latency_ns: u64) {
        self.total_cache_read_time_ns
            .fetch_add(latency_ns, Ordering::Relaxed);
        self.total_cache_read_ops.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_cache_write(&mut self, latency_ns: u64) {
        self.total_cache_write_time_ns
            .fetch_add(latency_ns, Ordering::Relaxed);
        self.total_cache_write_ops.fetch_add(1, Ordering::Relaxed);
    }

    /// Simple running average: total_time / count, in microseconds, rounded to integer
    fn avg_latency_us(total_ns: u64, count: u64) -> f64 {
        if count == 0 {
            return 0.0;
        }
        ((total_ns / count) / 1000) as f64
    }

    /// Update driver-side counters from shared memory header
    pub fn update_driver_counters(&self, cached_hits: u64, total_reads: u64, total_writes: u64) {
        self.driver_cache_hits.store(cached_hits, Ordering::Relaxed);
        self.driver_total_reads
            .store(total_reads, Ordering::Relaxed);
        self.driver_total_writes
            .store(total_writes, Ordering::Relaxed);
    }

    /// Calculate performance multiplier vs HDD-only.
    /// Both hits and misses are in chunk units (64KB blocks), consistent with service-side counters.
    /// e.g., 1.35 = 35% faster, 5.61 = 461% faster, 0.56 = 44% slower
    pub fn get_perf_multiplier(&self) -> f64 {
        let hits = self.total_cache_read_ops.load(Ordering::Relaxed) as f64;
        let misses = self.total_hdd_read_ops.load(Ordering::Relaxed) as f64;
        let total = hits + misses;

        if total < 10.0 {
            return 1.0;
        }

        let avg_hdd_ns = if misses > 0.0 {
            self.total_hdd_read_time_ns.load(Ordering::Relaxed) as f64 / misses
        } else {
            return 1.0;
        };

        if avg_hdd_ns <= 0.0 {
            return 1.0;
        }

        const CACHE_NS: f64 = 1000.0;
        let without_cache_ns = total * avg_hdd_ns;
        let with_cache_ns = hits * CACHE_NS + misses * avg_hdd_ns;

        (without_cache_ns / with_cache_ns).max(0.01).min(100.0)
    }

    /// Get a summary snapshot
    pub fn get_snapshot(&self) -> PerfSnapshot {
        PerfSnapshot {
            hdd_read_latency_us: Self::avg_latency_us(
                self.total_hdd_read_time_ns.load(Ordering::Relaxed),
                self.total_hdd_read_ops.load(Ordering::Relaxed),
            ),
            hdd_write_latency_us: Self::avg_latency_us(
                self.total_hdd_write_time_ns.load(Ordering::Relaxed),
                self.total_hdd_write_ops.load(Ordering::Relaxed),
            ),
            l1_read_latency_us: Self::avg_latency_us(
                self.total_cache_read_time_ns.load(Ordering::Relaxed),
                self.total_cache_read_ops.load(Ordering::Relaxed),
            ),
            perf_multiplier: self.get_perf_multiplier(),
            driver_cache_hits: self.driver_cache_hits.load(Ordering::Relaxed),
            driver_total_reads: self.driver_total_reads.load(Ordering::Relaxed),
            driver_total_writes: self.driver_total_writes.load(Ordering::Relaxed),
            total_hdd_read_ops: self.total_hdd_read_ops.load(Ordering::Relaxed),
            total_cache_read_ops: self.total_cache_read_ops.load(Ordering::Relaxed),
            total_hdd_write_ops: self.total_hdd_write_ops.load(Ordering::Relaxed),
            total_cache_write_ops: self.total_cache_write_ops.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PerfSnapshot {
    pub hdd_read_latency_us: f64,
    pub hdd_write_latency_us: f64,
    pub l1_read_latency_us: f64,
    pub perf_multiplier: f64,
    pub driver_cache_hits: u64,
    pub driver_total_reads: u64,
    pub driver_total_writes: u64,
    pub total_hdd_read_ops: u64,
    pub total_cache_read_ops: u64,
    pub total_hdd_write_ops: u64,
    pub total_cache_write_ops: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_crc32c() {
        let data = b"Hello, NovaCache!";
        let c = crc32c(data);
        assert_ne!(c, 0);
        assert_eq!(crc32c(data), c);
    }

    #[test]
    fn test_crc32c_empty() {
        let c = crc32c(b"");
        assert_eq!(c, 0);
    }

    #[test]
    fn test_avg_latency() {
        assert_eq!(PerformanceTracker::avg_latency_us(0, 0), 0.0);
        assert_eq!(PerformanceTracker::avg_latency_us(10_000, 1), 10.0);
        assert_eq!(PerformanceTracker::avg_latency_us(25_000, 2), 12.0);
    }

    #[test]
    fn test_boost_no_data() {
        let tracker = PerformanceTracker::new();
        assert_eq!(tracker.get_perf_multiplier(), 1.0);
    }
}
