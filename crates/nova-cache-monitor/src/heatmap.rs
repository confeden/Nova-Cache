//! # Access Heatmap
//!
//! Tracks per-file and per-region access frequency to identify
//! hot data that should be prioritized in the cache. Uses `RwLock<HashMap>`
//! for thread-safe updates from the ETW callback thread.
//!
//! The heatmap data feeds into cache admission and eviction decisions,
//! allowing frequently accessed blocks to be retained longer.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;
use std::time::Instant;

/// Maximum number of files to track before evicting cold entries.
const MAX_TRACKED_FILES: usize = 4096;

/// Minimum number of accesses before a file is considered "hot".
const HOT_THRESHOLD: u64 = 10;

/// Per-file access statistics.
#[derive(Debug)]
struct FileStats {
    read_count: AtomicU64,
    write_count: AtomicU64,
    bytes_read: AtomicU64,
    bytes_written: AtomicU64,
    first_access: Instant,
    last_access: RwLock<Instant>,
}

impl FileStats {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            read_count: AtomicU64::new(0),
            write_count: AtomicU64::new(0),
            bytes_read: AtomicU64::new(0),
            bytes_written: AtomicU64::new(0),
            first_access: now,
            last_access: RwLock::new(now),
        }
    }

    fn total_accesses(&self) -> u64 {
        self.read_count.load(Ordering::Relaxed) + self.write_count.load(Ordering::Relaxed)
    }

    fn record_read(&self, bytes: u64) {
        self.read_count.fetch_add(1, Ordering::Relaxed);
        self.bytes_read.fetch_add(bytes, Ordering::Relaxed);
        *self.last_access.write().unwrap() = Instant::now();
    }

    fn record_write(&self, bytes: u64) {
        self.write_count.fetch_add(1, Ordering::Relaxed);
        self.bytes_written.fetch_add(bytes, Ordering::Relaxed);
        *self.last_access.write().unwrap() = Instant::now();
    }

    fn is_hot(&self) -> bool {
        self.total_accesses() >= HOT_THRESHOLD
    }

    fn access_frequency(&self) -> f64 {
        let elapsed = self.first_access.elapsed().as_secs_f64();
        if elapsed <= 0.0 {
            return 0.0;
        }
        self.total_accesses() as f64 / elapsed
    }
}

/// Per-block-region access statistics for fine-grained heatmap.
#[derive(Debug)]
struct BlockRegionStats {
    access_count: AtomicU64,
    last_access: RwLock<Instant>,
}

impl BlockRegionStats {
    fn new() -> Self {
        Self {
            access_count: AtomicU64::new(0),
            last_access: RwLock::new(Instant::now()),
        }
    }

    fn record_access(&self) {
        self.access_count.fetch_add(1, Ordering::Relaxed);
        *self.last_access.write().unwrap() = Instant::now();
    }

    fn is_hot(&self) -> bool {
        self.access_count.load(Ordering::Relaxed) >= HOT_THRESHOLD
    }
}

/// A key for block-region tracking: file path + block offset.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
struct BlockKey {
    file_name: String,
    block_offset: u64,
}

/// The heatmap engine that tracks access patterns.
pub struct Heatmap {
    file_stats: RwLock<HashMap<String, FileStats>>,
    block_stats: RwLock<HashMap<BlockKey, BlockRegionStats>>,
    block_size: u64,
    events_processed: AtomicU64,
}

impl Heatmap {
    pub fn new(block_size: u64) -> Self {
        Self {
            file_stats: RwLock::new(HashMap::with_capacity(256)),
            block_stats: RwLock::new(HashMap::with_capacity(4096)),
            block_size,
            events_processed: AtomicU64::new(0),
        }
    }

    pub fn record_read(&self, file_name: &str, offset: u64, size: u64) {
        self.events_processed.fetch_add(1, Ordering::Relaxed);

        // Update file stats
        {
            let mut files = self.file_stats.write().unwrap();
            files
                .entry(file_name.to_string())
                .or_insert_with(FileStats::new)
                .record_read(size);
        }

        // Update block stats
        let block_offset = offset & !(self.block_size - 1);
        let key = BlockKey {
            file_name: file_name.to_string(),
            block_offset,
        };
        {
            let mut blocks = self.block_stats.write().unwrap();
            blocks
                .entry(key)
                .or_insert_with(BlockRegionStats::new)
                .record_access();
        }

        // Evict cold files if too many tracked
        if self.file_stats.read().unwrap().len() > MAX_TRACKED_FILES {
            self.evict_cold_files();
        }
    }

    pub fn record_write(&self, file_name: &str, offset: u64, size: u64) {
        self.events_processed.fetch_add(1, Ordering::Relaxed);

        {
            let mut files = self.file_stats.write().unwrap();
            files
                .entry(file_name.to_string())
                .or_insert_with(FileStats::new)
                .record_write(size);
        }

        let block_offset = offset & !(self.block_size - 1);
        let key = BlockKey {
            file_name: file_name.to_string(),
            block_offset,
        };
        {
            let mut blocks = self.block_stats.write().unwrap();
            blocks
                .entry(key)
                .or_insert_with(BlockRegionStats::new)
                .record_access();
        }
    }

    pub fn is_file_hot(&self, file_name: &str) -> bool {
        self.file_stats
            .read()
            .unwrap()
            .get(file_name)
            .map(|s| s.is_hot())
            .unwrap_or(false)
    }

    pub fn is_block_hot(&self, file_name: &str, offset: u64) -> bool {
        let block_offset = offset & !(self.block_size - 1);
        let key = BlockKey {
            file_name: file_name.to_string(),
            block_offset,
        };
        self.block_stats
            .read()
            .unwrap()
            .get(&key)
            .map(|s| s.is_hot())
            .unwrap_or(false)
    }

    pub fn file_frequency(&self, file_name: &str) -> f64 {
        self.file_stats
            .read()
            .unwrap()
            .get(file_name)
            .map(|s| s.access_frequency())
            .unwrap_or(0.0)
    }

    pub fn top_hot_files(&self, n: usize) -> Vec<(String, u64)> {
        let files = self.file_stats.read().unwrap();
        let mut entries: Vec<(String, u64)> = files
            .iter()
            .map(|(k, v)| (k.clone(), v.total_accesses()))
            .collect();
        drop(files);

        entries.sort_by(|a, b| b.1.cmp(&a.1));
        entries.truncate(n);
        entries
    }

    fn evict_cold_files(&self) {
        let mut files = self.file_stats.write().unwrap();
        let to_remove: Vec<String> = files
            .iter()
            .filter(|(_, v)| !v.is_hot() && v.last_access.read().unwrap().elapsed().as_secs() > 300)
            .map(|(k, _)| k.clone())
            .collect();
        for key in to_remove {
            files.remove(&key);
        }
    }

    pub fn tracked_files(&self) -> usize {
        self.file_stats.read().unwrap().len()
    }

    pub fn tracked_blocks(&self) -> usize {
        self.block_stats.read().unwrap().len()
    }

    pub fn events_processed(&self) -> u64 {
        self.events_processed.load(Ordering::Relaxed)
    }

    pub fn reset(&self) {
        self.file_stats.write().unwrap().clear();
        self.block_stats.write().unwrap().clear();
        self.events_processed.store(0, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_heatmap_basic() {
        let heatmap = Heatmap::new(64 * 1024);

        for i in 0..20 {
            heatmap.record_read("game.exe", i * 64 * 1024, 64 * 1024);
        }

        assert!(heatmap.is_file_hot("game.exe"));
        assert!(!heatmap.is_file_hot("other.exe"));
        assert_eq!(heatmap.tracked_files(), 1);
    }

    #[test]
    fn test_block_heat() {
        let heatmap = Heatmap::new(64 * 1024);

        for _ in 0..15 {
            heatmap.record_read("file.bin", 0, 64 * 1024);
        }

        assert!(heatmap.is_block_hot("file.bin", 0));
        assert!(!heatmap.is_block_hot("file.bin", 64 * 1024));
    }

    #[test]
    fn test_top_hot_files() {
        let heatmap = Heatmap::new(64 * 1024);

        for _ in 0..50 {
            heatmap.record_read("hot1.exe", 0, 100);
        }
        for _ in 0..30 {
            heatmap.record_read("hot2.exe", 0, 100);
        }
        for _ in 0..10 {
            heatmap.record_read("cold.exe", 0, 100);
        }

        let top = heatmap.top_hot_files(2);
        assert_eq!(top.len(), 2);
        assert_eq!(top[0].0, "hot1.exe");
        assert_eq!(top[1].0, "hot2.exe");
    }

    #[test]
    fn test_frequency() {
        let heatmap = Heatmap::new(64 * 1024);
        for _ in 0..100 {
            heatmap.record_read("fast.exe", 0, 100);
        }
        let freq = heatmap.file_frequency("fast.exe");
        assert!(freq > 0.0);
    }

    #[test]
    fn test_reset() {
        let heatmap = Heatmap::new(64 * 1024);
        heatmap.record_read("file.exe", 0, 100);
        assert_eq!(heatmap.tracked_files(), 1);

        heatmap.reset();
        assert_eq!(heatmap.tracked_files(), 0);
        assert_eq!(heatmap.events_processed(), 0);
    }
}
