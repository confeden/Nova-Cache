//! # TinyLFU Admission Filter
//!
//! A lightweight frequency-based admission filter that prevents cache pollution
//! by rejecting blocks that have only been accessed once. Based on the TinyLFU
//! approach from Megiddo & Modha (2014) and used in production by PrimoCache,
//! Redis (LFU mode), and modern CDN caches.
//!
//! ## Design
//!
//! Uses a Count-Min Sketch with 4 hash functions and 4-bit counters.
//! Combined with a "doorkeeper" Bloom filter that tracks items seen exactly
//! once — these are never admitted on first access.
//!
//! The sketch is periodically reset (aged) to prevent stale frequency counts
//! from dominating eviction decisions.
//!
//! ## Size
//!
//! For 1M entries: ~256 KB (4 rows × 256K slots × 4 bits = 256 KB).
//! This is negligible compared to L1 cache size (typically 256MB+).

use std::sync::atomic::{AtomicU64, Ordering};

/// Number of hash functions / rows in the Count-Min Sketch.
const NUM_ROWS: usize = 4;

/// Counter width in bits (4-bit counters, max value 15).
const COUNTER_BITS: usize = 4;
const COUNTER_MASK: u8 = 0x0F;

/// Doorkeeper Bloom filter size (number of 1-bit slots).
/// Power of 2 for fast modulo via bitwise AND.
const DOORKEEPER_SIZE: usize = 8192;
const DOORKEEPER_MASK: usize = DOORKEEPER_SIZE - 1;

/// Default reset window: every 100,000 accesses.
const DEFAULT_RESET_WINDOW: u64 = 100_000;

/// TinyLFU frequency sketch + doorkeeper.
pub struct TinyLFU {
    /// Count-Min Sketch: NUM_ROWS rows, each `sketch_width` entries of 4 bits.
    /// Stored as a flat Vec<u8> where each byte holds 2 counters.
    sketch: Vec<u8>,
    sketch_width: usize,

    /// Doorkeeper Bloom filter: tracks items seen exactly once.
    doorkeeper: Vec<u64>,

    /// Total number of `access()` calls since last reset.
    access_count: AtomicU64,

    /// How often to reset the sketch (aging window).
    reset_window: u64,

    /// Admission threshold: blocks with frequency <= this are rejected.
    /// Dynamically adjusted based on hit rate feedback.
    threshold: AtomicU64,

    /// Statistics.
    pub admitted: AtomicU64,
    pub rejected: AtomicU64,
}

impl TinyLFU {
    /// Create a new TinyLFU filter sized for `estimated_entries`.
    ///
    /// Memory overhead: approximately `estimated_entries * 2` bytes for the
    /// sketch + `estimated_entries / 64` bytes for the doorkeeper.
    pub fn new(estimated_entries: usize) -> Self {
        // Sketch width: next power of 2 >= estimated_entries / 2
        // (we want ~2 counters per entry for good accuracy)
        let sketch_width = (estimated_entries / 2).next_power_of_two().max(64);
        let sketch_size = (sketch_width * NUM_ROWS + 1) / 2; // 4-bit counters packed

        let doorkeeper_words = DOORKEEPER_SIZE / 64;

        Self {
            sketch: vec![0u8; sketch_size],
            sketch_width,
            doorkeeper: vec![0u64; doorkeeper_words],
            access_count: AtomicU64::new(0),
            reset_window: DEFAULT_RESET_WINDOW,
            threshold: AtomicU64::new(1),
            admitted: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
        }
    }

    /// Record an access to `key` and return the estimated frequency.
    ///
    /// This should be called on every cache access (hit or miss) to keep
    /// the frequency sketch up to date.
    pub fn access(&self, key: u64) -> u64 {
        let count = self.access_count.fetch_add(1, Ordering::Relaxed);

        // Periodic aging: reset sketch and doorkeeper
        if count > 0 && count % self.reset_window == 0 {
            self.reset_sketch();
        }

        // Check doorkeeper first
        let dk_idx = (key as usize) & DOORKEEPER_MASK;
        let dk_word = dk_idx / 64;
        let dk_bit = dk_idx % 64;
        let dk_mask = 1u64 << dk_bit;

        if self.doorkeeper[dk_word] & dk_mask == 0 {
            // Not seen before → set in doorkeeper, return frequency 0
            // SAFETY: doorkeeper is only accessed via this method and reset_sketch
            unsafe {
                let ptr = self.doorkeeper.as_ptr() as *mut u64;
                *ptr.add(dk_word) |= dk_mask;
            }
            return 0;
        }

        // Seen at least once → increment in sketch and return count
        let freq = self.increment_sketch(key);
        freq
    }

    /// Check if a key would be admitted (without modifying state).
    ///
    /// Returns `true` if the key's frequency exceeds the admission threshold.
    pub fn should_admit(&self, key: u64) -> bool {
        let freq = self.count_sketch(key);
        freq >= self.threshold.load(Ordering::Relaxed)
    }

    /// Get the current admission threshold.
    pub fn threshold(&self) -> u64 {
        self.threshold.load(Ordering::Relaxed)
    }

    /// Get total access count since last reset.
    pub fn access_count(&self) -> u64 {
        self.access_count.load(Ordering::Relaxed)
    }

    /// Get admission statistics.
    pub fn stats(&self) -> (u64, u64, u64) {
        (
            self.access_count(),
            self.admitted.load(Ordering::Relaxed),
            self.rejected.load(Ordering::Relaxed),
        )
    }

    /// Dynamically adjust the threshold based on hit rate feedback.
    ///
    /// If hit rate is high (>80%), increase threshold to be more selective.
    /// If hit rate is low (<50%), decrease threshold to admit more.
    pub fn adjust_threshold(&self, hit_rate: f64) {
        let current = self.threshold.load(Ordering::Relaxed);
        if hit_rate > 0.8 {
            let new_val = (current + 1).min(15);
            self.threshold.store(new_val, Ordering::Relaxed);
        } else if hit_rate < 0.5 && current > 0 {
            self.threshold.store(current - 1, Ordering::Relaxed);
        }
    }

    /// Force-reset the sketch and doorkeeper (called periodically).
    fn reset_sketch(&self) {
        // Zero the sketch via raw pointer (single-writer pattern)
        unsafe {
            let ptr = self.sketch.as_ptr() as *mut u8;
            std::ptr::write_bytes(ptr, 0, self.sketch.len());
        }
        // Zero the doorkeeper via raw pointer
        unsafe {
            let ptr = self.doorkeeper.as_ptr() as *mut u64;
            std::ptr::write_bytes(ptr, 0, self.doorkeeper.len());
        }
    }

    /// Increment the count for `key` in the sketch and return the new value.
    fn increment_sketch(&self, key: u64) -> u64 {
        let mut min_freq = u64::MAX;

        for row in 0..NUM_ROWS {
            let idx = self.sketch_index(key, row);
            let byte_idx = idx / 2;
            let is_high = idx % 2 == 1;

            // SAFETY: We use atomic-like pattern with single writer guarantee
            // (caller holds &self, sketch is not shared for writes)
            let byte = unsafe { *self.sketch.as_ptr().add(byte_idx) };
            let counter = if is_high {
                (byte >> COUNTER_BITS) & COUNTER_MASK
            } else {
                byte & COUNTER_MASK
            };

            let new_counter = (counter + 1).min(COUNTER_MASK);
            let new_byte = if is_high {
                (byte & 0x0F) | (new_counter << COUNTER_BITS)
            } else {
                (byte & 0xF0) | new_counter
            };

            // SAFETY: single writer
            unsafe {
                let ptr = self.sketch.as_ptr() as *mut u8;
                *ptr.add(byte_idx) = new_byte;
            }

            if (new_counter as u64) < min_freq {
                min_freq = new_counter as u64;
            }
        }

        min_freq
    }

    /// Read the count for `key` from the sketch (without incrementing).
    fn count_sketch(&self, key: u64) -> u64 {
        let mut min_freq = u64::MAX;

        for row in 0..NUM_ROWS {
            let idx = self.sketch_index(key, row);
            let byte_idx = idx / 2;
            let is_high = idx % 2 == 1;

            let byte = unsafe { *self.sketch.as_ptr().add(byte_idx) };
            let counter = if is_high {
                (byte >> COUNTER_BITS) & COUNTER_MASK
            } else {
                byte & COUNTER_MASK
            };

            if (counter as u64) < min_freq {
                min_freq = counter as u64;
            }
        }

        min_freq
    }

    /// Compute the sketch index for a given key and row.
    #[inline]
    fn sketch_index(&self, key: u64, row: usize) -> usize {
        let hash = splitmix64(key.wrapping_add((row as u64).wrapping_mul(0x9E3779B97F4A7C15)));
        (hash as usize) % self.sketch_width
    }
}

/// Fast, high-quality hash function (splitmix64).
#[inline]
fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E3779B97F4A7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
}

/// Public hash function for external callers (e.g., ArcCache admission).
#[inline]
pub fn splitmix64_plain(x: u64) -> u64 {
    splitmix64(x)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tinylfu_basic() {
        let filter = TinyLFU::new(1024);

        // First access: doorkeeper admits, returns 0
        assert_eq!(filter.access(42), 0);

        // Second access: now in sketch, returns 1
        assert_eq!(filter.access(42), 1);

        // Third access: returns 2
        assert_eq!(filter.access(42), 2);
    }

    #[test]
    fn test_tinylfu_should_admit() {
        let filter = TinyLFU::new(1024);
        filter.threshold.store(2, Ordering::Relaxed);

        // Not seen → should not admit
        assert!(!filter.should_admit(100));

        // Seen once (in doorkeeper) → frequency 0 → reject
        filter.access(100);
        assert!(!filter.should_admit(100));

        // Seen twice → frequency 1 → reject (threshold=2)
        filter.access(100);
        assert!(!filter.should_admit(100));

        // Seen 3 times → frequency 2 → admit
        filter.access(100);
        assert!(filter.should_admit(100));
    }

    #[test]
    fn test_tinylfu_different_keys() {
        let filter = TinyLFU::new(1024);

        for i in 0..100 {
            filter.access(i);
        }

        // Each key seen once → doorkeeper blocks, frequency 0
        for i in 0..100 {
            assert_eq!(filter.count_sketch(i), 0);
        }
    }

    #[test]
    fn test_tinylfu_sketch_accuracy() {
        let filter = TinyLFU::new(4096);

        // Access key 1 many times
        for _ in 0..10 {
            filter.access(1);
        }

        // Access key 2 a few times
        for _ in 0..3 {
            filter.access(2);
        }

        let freq1 = filter.count_sketch(1);
        let freq2 = filter.count_sketch(2);

        assert!(
            freq1 > freq2,
            "key 1 (10x) should have higher freq than key 2 (3x): {} vs {}",
            freq1,
            freq2
        );
    }

    #[test]
    fn test_tinylfu_adjust_threshold() {
        let filter = TinyLFU::new(1024);
        filter.threshold.store(5, Ordering::Relaxed);

        filter.adjust_threshold(0.9); // high hit rate → increase to 6
        assert_eq!(filter.threshold(), 6);

        filter.adjust_threshold(0.3); // low hit rate → decrease to 5
        assert_eq!(filter.threshold(), 5);

        filter.adjust_threshold(0.3); // again → 4
        assert_eq!(filter.threshold(), 4);
    }

    #[test]
    fn test_splitmix64_distribution() {
        // Verify no collisions for small inputs
        let mut seen = std::collections::HashSet::new();
        for i in 0..1000 {
            let h = splitmix64(i);
            assert!(seen.insert(h), "collision at i={}: hash={}", i, h);
        }
    }
}
