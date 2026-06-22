use std::sync::Arc;
use tracing::{error, info};

use nova_cache_core::l2_pool::L2Slot;

#[derive(Debug)]
pub struct L2WriteJob {
    pub block_id: u64,
    pub volume_id: u32,
    pub offset: u64,
    pub file_object: usize,
    pub slot: L2Slot,
    pub data: Vec<u8>,
    pub is_sequential: bool,
}

pub fn start_l2_writer(
    rx: crossbeam::channel::Receiver<L2WriteJob>,
    state: Arc<crate::orchestrator::ServiceStateShared>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("nova-l2-writer".to_string())
        .spawn(move || {
            info!("L2 writer started");

            while !state.shutdown_requested.load(std::sync::atomic::Ordering::Relaxed) {
                match rx.recv_timeout(std::time::Duration::from_millis(500)) {
                    Ok(job) => {
                        let pool_guard = state.l2_pool.read();
                        match pool_guard.write(&job.slot, &job.data) {
                            Ok(_) => {
                                drop(pool_guard);
                                state
                                    .stats
                                    .l2_block_count
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                state
                                    .l2_priority
                                    .insert(job.slot.slot, job.block_id, job.is_sequential);
                                let crc32 = crate::perf_tracker::crc32c(&job.data);
                                state.shared_ring.read().insert_l2_cache(
                                    job.volume_id,
                                    job.offset,
                                    crc32,
                                    job.file_object,
                                    job.slot.slot,
                                    true,
                                );
                            }
                            Err(e) => {
                                drop(pool_guard);
                                error!(
                                    "L2 writer: write failed for block 0x{:016X} slot {}: {} — leaving slot allocated",
                                    job.block_id, job.slot.slot, e
                                );
                            }
                        }
                        state.pending_l2_writes.remove(&job.block_id);
                    }
                    Err(crossbeam::channel::RecvTimeoutError::Timeout) => {}
                    Err(crossbeam::channel::RecvTimeoutError::Disconnected) => break,
                }
            }

            // Drain remaining jobs on shutdown
            info!("L2 writer draining remaining {} jobs", rx.len());
            while let Ok(job) = rx.try_recv() {
                let pool_guard = state.l2_pool.read();
                match pool_guard.write(&job.slot, &job.data) {
                    Ok(_) => {
                        drop(pool_guard);
                        state
                            .stats
                            .l2_block_count
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        state
                            .l2_priority
                            .insert(job.slot.slot, job.block_id, job.is_sequential);
                        let crc32 = crate::perf_tracker::crc32c(&job.data);
                        state.shared_ring.read().insert_l2_cache(
                            job.volume_id,
                            job.offset,
                            crc32,
                            job.file_object,
                            job.slot.slot,
                            true,
                        );
                    }
                    Err(e) => {
                        drop(pool_guard);
                        error!(
                            "L2 writer: drain write failed for block 0x{:016X}: {}",
                            job.block_id, e
                        );
                    }
                }
                state.pending_l2_writes.remove(&job.block_id);
            }

            info!("L2 writer exiting");
        })
        .expect("Failed to spawn L2 writer thread")
}
