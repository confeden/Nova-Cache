use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tracing::debug;

const DEFAULT_SEQUENTIAL_THRESHOLD_KB: u32 = 512;
const DEFAULT_PREFETCH_AHEAD_MB: u32 = 16;
const MAX_WINDOW_SIZE: usize = 64;
const PREFETCH_DEBOUNCE: Duration = Duration::from_millis(200);

/// Minimum window size in blocks.
const MIN_PREFETCH_BLOCKS: u64 = 4;

/// Track effectiveness over this many prefetch events.
const EFFECTIVENESS_WINDOW: usize = 100;

#[derive(Debug)]
struct FileAccessTracker {
    offsets: VecDeque<u64>,
    last_prefetch: Option<Instant>,
    is_sequential: bool,
    sequential_streak: u32,
    recent_prefetches: VecDeque<u64>,
}

impl FileAccessTracker {
    fn new() -> Self {
        Self {
            offsets: VecDeque::with_capacity(MAX_WINDOW_SIZE),
            last_prefetch: None,
            is_sequential: false,
            sequential_streak: 0,
            recent_prefetches: VecDeque::with_capacity(64),
        }
    }

    fn record_read(&mut self, offset: u64, block_size: u64, threshold_bytes: u64) -> Option<u64> {
        self.offsets.push_back(offset);
        if self.offsets.len() > MAX_WINDOW_SIZE {
            self.offsets.pop_front();
        }

        let sequential = self.detect_sequential(block_size, threshold_bytes);

        if sequential {
            self.sequential_streak += 1;
            self.is_sequential = true;

            if self.sequential_streak >= 2 {
                if let Some(last) = self.last_prefetch {
                    if last.elapsed() < PREFETCH_DEBOUNCE {
                        return None;
                    }
                }
                self.last_prefetch = Some(Instant::now());

                let latest = *self.offsets.back().unwrap();
                let prefetch_start = latest + block_size;

                if self.recent_prefetches.contains(&prefetch_start) {
                    return None;
                }

                self.recent_prefetches.push_back(prefetch_start);
                if self.recent_prefetches.len() > 64 {
                    self.recent_prefetches.pop_front();
                }

                return Some(prefetch_start);
            }
        } else {
            self.sequential_streak = 0;
            self.is_sequential = false;
        }

        None
    }

    fn detect_sequential(&self, block_size: u64, threshold_bytes: u64) -> bool {
        let window: Vec<u64> = self.offsets.iter().cloned().collect();
        if window.len() < 2 {
            return false;
        }

        let mut contiguous_bytes: u64 = 0;
        let mut max_contiguous: u64 = 0;

        for i in 1..window.len() {
            let prev = window[i - 1];
            let curr = window[i];

            if curr == prev + block_size {
                contiguous_bytes += block_size;
            } else if curr > prev && curr <= prev + block_size * 4 {
                contiguous_bytes += block_size;
            } else {
                contiguous_bytes = 0;
            }

            if contiguous_bytes > max_contiguous {
                max_contiguous = contiguous_bytes;
            }
        }

        max_contiguous >= threshold_bytes
    }
}

pub struct PrefetchEngine {
    trackers: HashMap<String, FileAccessTracker>,
    block_size: u64,
    sequential_threshold_bytes: u64,
    /// Current adaptive window size in bytes.
    prefetch_ahead_bytes: u64,
    /// Minimum window size in bytes.
    min_window_bytes: u64,
    /// Maximum window size in bytes.
    max_window_bytes: u64,
    /// Rolling effectiveness tracking: how many of the last N prefetches were useful.
    effectiveness_window: VecDeque<bool>,
    /// Total prefetches initiated.
    prefetch_count: AtomicU64,
    /// Total sequential detections.
    sequential_count: AtomicU64,
}

impl PrefetchEngine {
    pub fn new(
        block_size_bytes: u64,
        sequential_threshold_kb: u32,
        prefetch_ahead_mb: u32,
        min_window_mb: u32,
        max_window_mb: u32,
    ) -> Self {
        Self {
            trackers: HashMap::new(),
            block_size: block_size_bytes,
            sequential_threshold_bytes: sequential_threshold_kb as u64 * 1024,
            prefetch_ahead_bytes: prefetch_ahead_mb as u64 * 1024 * 1024,
            min_window_bytes: min_window_mb as u64 * 1024 * 1024,
            max_window_bytes: max_window_mb as u64 * 1024 * 1024,
            effectiveness_window: VecDeque::with_capacity(EFFECTIVENESS_WINDOW),
            prefetch_count: AtomicU64::new(0),
            sequential_count: AtomicU64::new(0),
        }
    }

    pub fn with_defaults() -> Self {
        Self::new(
            64 * 1024,
            DEFAULT_SEQUENTIAL_THRESHOLD_KB,
            DEFAULT_PREFETCH_AHEAD_MB,
            1,   // min 1 MB
            64,  // max 64 MB
        )
    }

    /// Update configuration at runtime (e.g., from IPC).
    pub fn reconfigure(&mut self, threshold_kb: u32, ahead_mb: u32, min_window_mb: u32, max_window_mb: u32) {
        self.sequential_threshold_bytes = threshold_kb as u64 * 1024;
        self.prefetch_ahead_bytes = ahead_mb as u64 * 1024 * 1024;
        self.min_window_bytes = min_window_mb as u64 * 1024 * 1024;
        self.max_window_bytes = max_window_mb as u64 * 1024 * 1024;
    }

    /// Report whether a prefetched block was actually consumed by the application.
    /// Call this from the consumer loop when a block matched a prefetch.
    pub fn report_useful(&mut self) {
        self.effectiveness_window.push_back(true);
        if self.effectiveness_window.len() > EFFECTIVENESS_WINDOW {
            self.effectiveness_window.pop_front();
        }
        self.adapt_window();
    }

    /// Report that a prefetched block was evicted or never consumed.
    pub fn report_wasted(&mut self) {
        self.effectiveness_window.push_back(false);
        if self.effectiveness_window.len() > EFFECTIVENESS_WINDOW {
            self.effectiveness_window.pop_front();
        }
        self.adapt_window();
    }

    /// Scale the prefetch window based on recent effectiveness.
    fn adapt_window(&mut self) {
        if self.effectiveness_window.len() < 10 {
            return; // Not enough data yet
        }
        let useful = self.effectiveness_window.iter().filter(|&&b| b).count();
        let ratio = useful as f64 / self.effectiveness_window.len() as f64;

        let current_mb = self.prefetch_ahead_bytes / (1024 * 1024);
        let new_mb = if ratio > 0.5 {
            // High effectiveness: increase window (up to max)
            (current_mb * 2).min(self.max_window_bytes / (1024 * 1024))
        } else if ratio < 0.1 {
            // Low effectiveness: shrink window (down to min)
            (current_mb / 2).max(self.min_window_bytes / (1024 * 1024))
        } else {
            current_mb // Stable: keep current size
        };

        if new_mb != current_mb {
            self.prefetch_ahead_bytes = new_mb * 1024 * 1024;
            debug!(
                "Adaptive prefetch: effectiveness={:.1}%, window={}MB",
                ratio * 100.0,
                new_mb
            );
        }

        // Also adapt sequential threshold
        if ratio < 0.05 && self.sequential_threshold_bytes < 1024 * 1024 {
            // Very low effectiveness: raise threshold to reduce false positives
            let new_threshold = (self.sequential_threshold_bytes * 2).min(4 * 1024 * 1024);
            self.sequential_threshold_bytes = new_threshold;
            debug!(
                "Adaptive threshold: increased to {}KB",
                new_threshold / 1024
            );
        } else if ratio > 0.7 && self.sequential_threshold_bytes > 64 * 1024 {
            // High effectiveness: lower threshold to prefetch more aggressively
            let new_threshold = (self.sequential_threshold_bytes / 2).max(64 * 1024);
            self.sequential_threshold_bytes = new_threshold;
            debug!(
                "Adaptive threshold: decreased to {}KB",
                new_threshold / 1024
            );
        }
    }

    pub fn record_read(&mut self, file_name: &str, offset: u64) -> Option<PrefetchRequest> {
        let tracker = self
            .trackers
            .entry(file_name.to_string())
            .or_insert_with(FileAccessTracker::new);

        let prefetch_start =
            tracker.record_read(offset, self.block_size, self.sequential_threshold_bytes)?;

        self.sequential_count.fetch_add(1, Ordering::Relaxed);

        let prefetch_blocks = (self.prefetch_ahead_bytes / self.block_size).max(MIN_PREFETCH_BLOCKS);
        let end_offset = prefetch_start + prefetch_blocks * self.block_size;

        debug!(
            "Prefetch triggered for {} at offset 0x{:X} ({} blocks ahead, {}MB window)",
            file_name,
            prefetch_start,
            prefetch_blocks,
            self.prefetch_ahead_bytes / (1024 * 1024)
        );

        self.prefetch_count.fetch_add(1, Ordering::Relaxed);

        Some(PrefetchRequest {
            file_name: file_name.to_string(),
            start_offset: prefetch_start,
            end_offset,
            block_size: self.block_size,
        })
    }

    pub fn prefetch_count(&self) -> u64 {
        self.prefetch_count.load(Ordering::Relaxed)
    }

    pub fn sequential_count(&self) -> u64 {
        self.sequential_count.load(Ordering::Relaxed)
    }

    pub fn tracked_files(&self) -> usize {
        self.trackers.len()
    }

    pub fn current_window_mb(&self) -> u64 {
        self.prefetch_ahead_bytes / (1024 * 1024)
    }

    pub fn current_threshold_kb(&self) -> u64 {
        self.sequential_threshold_bytes / 1024
    }

    pub fn reset(&mut self) {
        self.trackers.clear();
        self.prefetch_count.store(0, Ordering::Relaxed);
        self.sequential_count.store(0, Ordering::Relaxed);
        self.effectiveness_window.clear();
    }
}

#[derive(Debug, Clone)]
pub struct PrefetchRequest {
    pub file_name: String,
    pub start_offset: u64,
    pub end_offset: u64,
    pub block_size: u64,
}

impl PrefetchRequest {
    pub fn block_offsets(&self) -> impl Iterator<Item = u64> {
        let step = self.block_size;
        let start = self.start_offset;
        let end = self.end_offset;
        (0..)
            .map(move |i| start + i * step)
            .take_while(move |&off| off < end)
    }

    pub fn block_count(&self) -> usize {
        ((self.end_offset - self.start_offset) / self.block_size) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prefetch_request_block_offsets() {
        let req = PrefetchRequest {
            file_name: "test.bin".into(),
            start_offset: 0,
            end_offset: 64 * 1024 * 4,
            block_size: 64 * 1024,
        };

        let offsets: Vec<u64> = req.block_offsets().collect();
        assert_eq!(offsets.len(), 4);
        assert_eq!(offsets[0], 0);
        assert_eq!(offsets[1], 64 * 1024);
        assert_eq!(offsets[2], 128 * 1024);
        assert_eq!(offsets[3], 192 * 1024);
    }

    #[test]
    fn test_sequential_detection() {
        let mut engine = PrefetchEngine::new(64 * 1024, 128, 16, 1, 64);
        let mut prefetches = Vec::new();
        for i in 0..20 {
            let offset = i * 64 * 1024;
            if let Some(req) = engine.record_read("test.bin", offset) {
                prefetches.push(req);
            }
        }
        assert!(!prefetches.is_empty());
        assert!(engine.sequential_count() > 0);
    }

    #[test]
    fn test_random_access_no_prefetch() {
        let mut engine = PrefetchEngine::new(64 * 1024, 512, 16, 1, 64);
        let offsets = [0, 1_000_000, 500_000, 2_000_000, 100_000, 3_000_000];
        let mut prefetch_count = 0;
        for &offset in &offsets {
            if engine.record_read("random.bin", offset).is_some() {
                prefetch_count += 1;
            }
        }
        assert!(
            prefetch_count < 3,
            "Random access should not trigger prefetches: got {}",
            prefetch_count
        );
    }

    #[test]
    fn test_empty_tracker_no_sequential() {
        let mut engine = PrefetchEngine::new(64 * 1024, 512, 16, 1, 64);
        let result = engine.record_read("file.bin", 0);
        assert!(result.is_none());
    }

    #[test]
    fn test_prefetch_debounce() {
        let mut engine = PrefetchEngine::new(64 * 1024, 128, 16, 1, 64);
        for i in 0..5 {
            engine.record_read("debounce.bin", i * 64 * 1024);
        }
        let r1 = engine.record_read("debounce.bin", 5 * 64 * 1024);
        let r2 = engine.record_read("debounce.bin", 6 * 64 * 1024);
        let triggered = [r1, r2].iter().filter(|r| r.is_some()).count();
        assert!(triggered <= 1);
    }

    #[test]
    fn test_adaptive_window() {
        let mut engine = PrefetchEngine::new(64 * 1024, 128, 16, 1, 64);
        let initial = engine.current_window_mb();
        // Report high effectiveness
        for _ in 0..80 {
            engine.report_useful();
        }
        for _ in 0..20 {
            engine.report_wasted();
        }
        assert!(
            engine.current_window_mb() >= initial,
            "Window should grow with high effectiveness"
        );
    }

    #[test]
    fn test_reset() {
        let mut engine = PrefetchEngine::new(64 * 1024, 512, 16, 1, 64);
        for i in 0..10 {
            engine.record_read("file.bin", i * 64 * 1024);
        }
        assert!(engine.tracked_files() > 0);
        engine.reset();
        assert_eq!(engine.tracked_files(), 0);
        assert_eq!(engine.prefetch_count(), 0);
    }
}
