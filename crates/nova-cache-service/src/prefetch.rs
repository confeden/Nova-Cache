use crossbeam::channel::Receiver;
use dashmap::DashSet;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek};
use std::os::windows::fs::OpenOptionsExt;
use std::sync::Arc;
use tracing::info;

use crate::orchestrator::{CachedBlockData, SlotLocation};

#[derive(Debug, Clone, Copy)]
pub struct BlockMetadata {
    pub volume_id: u32,
    pub offset: u64,
    pub file_object: u64,
}

#[derive(Debug, Clone)]
pub struct PrefetchJob {
    pub block_id: u64,
    pub volume_id: u32,
    pub offset: u64,
    pub file_object: u64,
}

/// Maximum number of contiguous blocks to merge into a single HDD read.
const COALESCE_MAX_BLOCKS: usize = 8;

/// Blocks that have been prefetched but not yet consumed by the application.
pub type PrefetchedSet = Arc<DashSet<u64>>;

pub fn new_prefetched_set() -> PrefetchedSet {
    Arc::new(DashSet::new())
}

/// Check if this block was prefetched. If so, report effectiveness and remove from tracking.
pub fn check_prefetch_effectiveness(
    block_id: u64,
    prefetched: &PrefetchedSet,
    stats: &nova_cache_core::stats::CacheStats,
    prefetch_engine: &Arc<parking_lot::Mutex<nova_cache_monitor::prefetch::PrefetchEngine>>,
) {
    if prefetched.remove(&block_id).is_some() {
        stats.prefetch_useful.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if let Some(mut engine) = prefetch_engine.try_lock() {
            engine.report_useful();
        }
    }
}

fn process_prefetch_job(
    state: &crate::orchestrator::ServiceStateShared,
    job: PrefetchJob,
    block_size: usize,
) {
    let already_in_l1 = {
        let cache = state.arc_cache.read().clone();
        if let Some(cached) = cache.get(&job.block_id) {
            matches!(cached.slot, SlotLocation::Pool(_))
        } else {
            false
        }
    };

    if already_in_l1 {
        return;
    }

    let cached_l2_slot = {
        let cache = state.arc_cache.read().clone();
        cache.get(&job.block_id).and_then(|c| c.l2_persistent.clone())
    };

    if let Some(l2_slot) = cached_l2_slot {
        if !state.pending_l2_writes.contains(&job.block_id) {
            let mut data = vec![0u8; block_size];
            if state.l2_pool.read().read(&l2_slot, &mut data).is_ok() {
                let l2_persistent = Some(l2_slot);
                insert_into_cache(state, job.block_id, data, l2_persistent);
                return;
            }
        }
    }

    fetch_from_hdd(state, job);
}

/// Fetch from HDD, coalescing nearby blocks into a single read.
fn fetch_from_hdd(
    state: &crate::orchestrator::ServiceStateShared,
    job: PrefetchJob,
) {
    let block_size = state.config.read().cache.block_size_kb as u32 * 1024;
    let chunk_offset = job.offset - (job.offset % block_size as u64);

    let max_coalesce = (state.prefetch_sender.len() > 128).then_some(COALESCE_MAX_BLOCKS).unwrap_or(1);

    let mut file_result = {
        let path = format!("\\\\?\\Volume{{{}}}\\{}", job.volume_id, job.file_object);
        File::open(&path)
    };

    if file_result.is_err() && job.file_object == 0 {
        let drive_char = (job.volume_id as u8 + b'A') as char;
        let vol_path = format!("\\\\.\\{}:", drive_char);
        file_result = OpenOptions::new()
            .read(true)
            .custom_flags(0x80000000 | 0x00000001)
            .open(&vol_path);
    }

    let mut file_obj = match file_result {
        Ok(f) => f,
        Err(_) => return,
    };

    if file_obj.seek(std::io::SeekFrom::Start(chunk_offset)).is_err() {
        return;
    }

    // Read the first block (always required)
    let mut data = vec![0u8; block_size as usize];
    let bytes_read = match file_obj.read(&mut data) {
        Ok(n) => n,
        Err(_) => return,
    };
    data.truncate(bytes_read);
    let crc32 = crate::perf_tracker::crc32c(&data);

    let ring = state.shared_ring.read().clone();
    ring.insert_l1_cache(job.volume_id, chunk_offset, &data, crc32, job.file_object as usize);

    insert_into_cache(state, job.block_id, data, None);

    // Coalesce: read additional contiguous blocks while the file is seeked
    if max_coalesce > 1 && bytes_read as usize == block_size as usize {
        for i in 1..max_coalesce {
            let next_offset = chunk_offset + i as u64 * block_size as u64;
            let next_block_id = crate::orchestrator::make_block_id(
                job.volume_id,
                next_offset,
                job.file_object,
            );

            // Stop if already in L1
            let already = {
                let cache = state.arc_cache.read().clone();
                cache.get(&next_block_id).map(|c| matches!(c.slot, SlotLocation::Pool(_))).unwrap_or(false)
            };
            if already {
                break;
            }

            // Stop if this block is already pending a prefetch or write
            if state.pending_l2_writes.contains(&next_block_id) {
                continue;
            }

            let mut extra = vec![0u8; block_size as usize];
            match file_obj.read(&mut extra) {
                Ok(n) if n > 0 => {
                    extra.truncate(n);
                    let crc32_extra = crate::perf_tracker::crc32c(&extra);
                    ring.insert_l1_cache(
                        job.volume_id,
                        next_offset,
                        &extra,
                        crc32_extra,
                        job.file_object as usize,
                    );
                    insert_into_cache(state, next_block_id, extra, None);
                }
                _ => break,
            }
        }
    }
}

fn insert_into_cache(
    state: &crate::orchestrator::ServiceStateShared,
    block_id: u64,
    data: Vec<u8>,
    l2_persistent: Option<nova_cache_core::l2_pool::L2Slot>,
) {
    let pool = state.pool.read().clone();
    if let Some(l1_slot) = pool.allocate() {
        if pool.write(l1_slot, &data).is_ok() {
            state.stats.l1_block_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            let block_info = CachedBlockData {
                size: data.len(),
                slot: SlotLocation::Pool(l1_slot),
                l2_persistent,
            };

            let arc_cache = state.arc_cache.read().clone();
            state.stats.prefetch_ops.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            let evicted = arc_cache.insert(block_id, block_info);

            if let Some((_evicted_id, evicted_data)) = evicted {
                match evicted_data.slot {
                    SlotLocation::Pool(slot) => {
                        pool.free(slot);
                        state.stats.l1_block_count.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    SlotLocation::Ssd(ref slot) => {
                        state.l2_pool.read().free(slot);
                        state.stats.l2_block_count.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                        if let Some(ref l2_slot) = evicted_data.l2_persistent {
                            if l2_slot.backend != slot.backend || l2_slot.slot != slot.slot {
                                state.l2_pool.read().free(l2_slot);
                            }
                        }
                    }
                }
                // Track prefetch waste: evicted block might never be consumed
                if state.prefetched_blocks.contains(&_evicted_id) {
                    state.stats.prefetch_wasted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    state.prefetched_blocks.remove(&_evicted_id);
                    if let Some(mut engine) = state.prefetch_engine.try_lock() {
                        engine.report_wasted();
                    }
                }
            }
        } else {
            pool.free(l1_slot);
        }
    }
}

fn prefetch_worker_loop(
    state: Arc<crate::orchestrator::ServiceStateShared>,
    receiver: Receiver<PrefetchJob>,
    block_size: usize,
    worker_id: usize,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name(format!("nova-prefetch-{}", worker_id))
        .spawn(move || {
            info!("Prefetch worker {} started.", worker_id);
            use std::time::Duration;

            while !state.shutdown_requested.load(std::sync::atomic::Ordering::Relaxed) {
                match receiver.recv_timeout(Duration::from_millis(500)) {
                    Ok(job) => {
                        state.prefetched_blocks.insert(job.block_id);
                        process_prefetch_job(&state, job, block_size);
                    }
                    Err(crossbeam::channel::RecvTimeoutError::Timeout) => {}
                    Err(crossbeam::channel::RecvTimeoutError::Disconnected) => break,
                }
            }
            info!("Prefetch worker {} exiting.", worker_id);
        })
        .expect("Failed to spawn prefetch worker thread")
}

pub fn start_prefetch_workers(
    state: Arc<crate::orchestrator::ServiceStateShared>,
    receiver: Receiver<PrefetchJob>,
    block_size: usize,
    worker_count: usize,
) -> Vec<std::thread::JoinHandle<()>> {
    let mut handles = Vec::with_capacity(worker_count);
    for i in 0..worker_count {
        let h = prefetch_worker_loop(state.clone(), receiver.clone(), block_size, i);
        handles.push(h);
    }
    info!("Started {} parallel prefetch workers", worker_count);
    handles
}
