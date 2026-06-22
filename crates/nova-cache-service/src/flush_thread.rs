use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::{error, info, warn};

#[cfg(windows)]
use windows::Win32::System::Memory::SetProcessWorkingSetSizeEx;
#[cfg(windows)]
use windows::Win32::System::Threading::GetCurrentProcess;

use crate::journal::Journal;

#[derive(Debug, Clone)]
pub struct DirtyBlock {
    pub block_id: u64,
    pub volume_id: u32,
    pub offset: u64,
    pub data: Vec<u8>,
    pub journal_sequence: u64,
    pub slot_id: u32,
}

pub struct FlushThread {
    dirty_blocks: Arc<Mutex<HashMap<u64, DirtyBlock>>>,
    journal: Arc<Journal>,
    flush_interval_ms: u64,
    flush_interval_max_ms: u64,
    flush_dirty_threshold: u32,
    max_dirty_blocks: usize,
    l2_pool: Arc<parking_lot::RwLock<nova_cache_core::l2_pool::L2Pool>>,
    block_size: usize,
    running: Arc<AtomicBool>,
    l2_mmap_flush_interval_ms: u64,
}

impl FlushThread {
    pub fn new(
        dirty_blocks: Arc<Mutex<HashMap<u64, DirtyBlock>>>,
        journal: Arc<Journal>,
        flush_interval_ms: u64,
        flush_interval_max_ms: u64,
        flush_dirty_threshold: u32,
        max_dirty_blocks: usize,
        l2_pool: Arc<parking_lot::RwLock<nova_cache_core::l2_pool::L2Pool>>,
        block_size: usize,
    ) -> Self {
        Self {
            dirty_blocks,
            journal,
            flush_interval_ms,
            flush_interval_max_ms,
            flush_dirty_threshold,
            max_dirty_blocks,
            l2_pool,
            block_size,
            running: Arc::new(AtomicBool::new(true)),
            l2_mmap_flush_interval_ms: 30000,
        }
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    pub fn running_handle(&self) -> Arc<AtomicBool> {
        self.running.clone()
    }

    fn adaptive_sleep(dirty_count: usize, base_ms: u64, max_ms: u64, threshold: u32) -> u64 {
        if dirty_count == 0 {
            return max_ms;
        }
        let ratio = dirty_count as f64 / threshold as f64;
        if ratio >= 1.0 {
            base_ms
        } else if ratio >= 0.5 {
            // Between 50-100% of threshold: aggressive flush
            base_ms / 2
        } else {
            let ms = base_ms as f64 + (max_ms as f64 - base_ms as f64) * (1.0 - ratio);
            ms.clamp(base_ms as f64, max_ms as f64) as u64
        }
    }

    pub fn start(&self) -> std::thread::JoinHandle<()> {
        let dirty_blocks = self.dirty_blocks.clone();
        let journal = self.journal.clone();
        let base_ms = self.flush_interval_ms;
        let max_ms = self.flush_interval_max_ms;
        let threshold = self.flush_dirty_threshold;
        let max_dirty = self.max_dirty_blocks;
        let l2_pool = self.l2_pool.clone();
        let _block_size = self.block_size;
        let running = self.running.clone();
        let mmap_flush_ms = self.l2_mmap_flush_interval_ms;

        std::thread::Builder::new()
            .name("nova-flush".to_string())
            .spawn(move || {
                info!(
                    "Flush thread started (base={}ms, max={}ms, threshold={}, max_dirty={}, mmap_flush={}ms)",
                    base_ms, max_ms, threshold, max_dirty, mmap_flush_ms
                );

                let mut last_mmap_flush = std::time::Instant::now();

                while running.load(Ordering::Relaxed) {
                    let current_dirty = dirty_blocks.lock().len();

                    // If over max dirty limit, skip sleep and flush immediately
                    if current_dirty >= max_dirty {
                        warn!(
                            "Dirty block cap reached ({}/{}), flushing all",
                            current_dirty, max_dirty
                        );
                    } else {
                        let sleep_ms =
                            Self::adaptive_sleep(current_dirty, base_ms, max_ms, threshold);
                        std::thread::sleep(std::time::Duration::from_millis(sleep_ms));
                    }

                    // Periodically flush memory-mapped L2 cache to disk
                    if last_mmap_flush.elapsed() >= std::time::Duration::from_millis(mmap_flush_ms) {
                        l2_pool.read().flush();
                        #[cfg(windows)]
                        unsafe {
                            let _ = SetProcessWorkingSetSizeEx(GetCurrentProcess(), usize::MAX, usize::MAX, Default::default());
                        }
                        last_mmap_flush = std::time::Instant::now();
                    }

                    // 1. Pick keys to flush — flush ALL if over cap, else dynamic batch
                    let keys: Vec<u64> = {
                        let map = dirty_blocks.lock();
                        if current_dirty >= max_dirty {
                            map.keys().copied().collect()
                        } else {
                            // Dynamic batch: more blocks when dirty count is high
                            let batch_size = if current_dirty > threshold as usize * 4 {
                                128
                            } else if current_dirty > threshold as usize * 2 {
                                64
                            } else {
                                32
                            };
                            map.keys().copied().take(batch_size).collect()
                        }
                    };

                    if keys.is_empty() {
                        continue;
                    }

                    // 2. Clone blocks for flushing (dirty_blocks retains ownership)
                    let batch: Vec<DirtyBlock> = {
                        let map = dirty_blocks.lock();
                        keys.iter().filter_map(|k| map.get(k).cloned()).collect()
                    };

                    if batch.is_empty() {
                        continue;
                    }

                    info!("Flush thread: flushing {} dirty blocks", batch.len());

                    // 3. Write to L2 via memory-mapped L2 pool
                    let (flushed_sequences, failed_ids) = flush_batch_to_disk(&l2_pool, &batch);

                    if !flushed_sequences.is_empty() {
                        info!(
                            "Flush thread: wrote {} blocks to L2, committing journal...",
                            flushed_sequences.len()
                        );
                        if let Err(e) = journal.commit_batch(&flushed_sequences) {
                            error!(
                                "Failed to batch commit {} journal entries: {:?}",
                                flushed_sequences.len(),
                                e
                            );
                        } else {
                            info!(
                                "Flush thread: committed {} journal entries",
                                flushed_sequences.len()
                            );
                        }
                    }

                    // 4. ONLY NOW remove successfully flushed blocks from dirty_blocks
                    {
                        let mut map = dirty_blocks.lock();
                        for block in &batch {
                            if !failed_ids.contains(&block.block_id) {
                                map.remove(&block.block_id);
                            }
                        }
                        // Re-queue failed blocks only if no newer entry exists
                        for block in &batch {
                            if failed_ids.contains(&block.block_id) {
                                map.entry(block.block_id).or_insert_with(|| block.clone());
                            }
                        }
                    }

                    let remaining = dirty_blocks.lock().len();
                    if remaining > 0 {
                        info!("Flush thread: {} blocks still dirty", remaining);
                    } else if journal.file_size() > 1024 * 1024 {
                        info!(
                            "Flush thread: all dirty blocks flushed, truncating journal ({}KB)",
                            journal.file_size() / 1024
                        );
                        if let Err(e) = journal.truncate() {
                            warn!("Failed to truncate journal: {:?}", e);
                        } else {
                            info!("Journal truncated to {}KB", journal.file_size() / 1024);
                        }
                    }
                }
            })
            .expect("Failed to spawn flush thread")
    }
}

pub fn flush_batch_to_disk(
    l2_pool: &Arc<parking_lot::RwLock<nova_cache_core::l2_pool::L2Pool>>,
    batch: &[DirtyBlock],
) -> (Vec<u64>, Vec<u64>) {
    if batch.is_empty() {
        return (Vec::new(), Vec::new());
    }

    let pool = l2_pool.read();
    if !pool.is_healthy() {
        return (Vec::new(), batch.iter().map(|b| b.block_id).collect());
    }

    let mut flushed = Vec::new();
    let mut failed = Vec::new();

    for block in batch {
        let slot = nova_cache_core::l2_pool::L2Slot {
            backend: 0,
            slot: block.slot_id,
        };

        match pool.write(&slot, &block.data) {
            Ok(_) => {
                flushed.push(block.journal_sequence);
            }
            Err(e) => {
                failed.push(block.block_id);
                error!(
                    "Failed to flush block 0x{:016X} to L2 slot {}: {:?}",
                    block.block_id, block.slot_id, e
                );
            }
        }
    }

    (flushed, failed)
}
