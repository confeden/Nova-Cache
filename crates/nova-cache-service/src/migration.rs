use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;

use parking_lot::RwLock;
use tracing::{info, warn};

use nova_cache_core::l2_pool::{L2Pool, L2Slot};
use nova_cache_core::persistence::{self, CachedEntry};
use nova_cache_core::stats::CacheStats;

use crate::orchestrator::{CachedBlockData, SlotLocation};

pub fn migrate_l2(
    l2_pool: &Arc<RwLock<L2Pool>>,
    arc_cache: &Arc<RwLock<Arc<nova_cache_core::arc::ArcCache<u64, CachedBlockData>>>>,
    _stats: &Arc<CacheStats>,
    new_paths: &[PathBuf],
    new_size_gb: u32,
    block_size_kb: u32,
    progress: &Arc<AtomicI32>,
) {
    let block_size = block_size_kb as usize * 1024;

    let old_entries: Vec<(u64, CachedBlockData)> = {
        let cache = arc_cache.read();
        cache.entries()
    };

    let l2_entries: Vec<(u64, L2Slot)> = old_entries
        .iter()
        .filter_map(|(id, data)| {
            if let SlotLocation::Ssd(slot) = &data.slot {
                Some((*id, slot.clone()))
            } else {
                None
            }
        })
        .collect();

    info!("L2 migration: {} L2 entries to migrate", l2_entries.len());

    if new_paths.is_empty() {
        info!("No L2 backends configured, clearing cache");
        let new_cache = Arc::new(nova_cache_core::arc::ArcCache::new(1));
        *arc_cache.write() = new_cache;
        progress.store(-1, Ordering::Release);
        return;
    }

    let size_per_backend =
        (new_size_gb as u64 * 1024 * 1024 * 1024) / new_paths.len().max(1) as u64;

    let new_pool = match L2Pool::new(new_paths, size_per_backend, block_size) {
        Ok(pool) => pool,
        Err(e) => {
            warn!("Failed to create new L2 pool: {:?}", e);
            progress.store(-1, Ordering::Release);
            return;
        }
    };

    let total = l2_entries.len() as i32;
    let mut migrated = 0i32;
    let mut new_arc_entries = Vec::new();
    let mut persistence_entries = Vec::new();

    progress.store(0, Ordering::Release);

    for (block_id, old_slot) in &l2_entries {
        let mut buf = vec![0u8; block_size];
        let read_result = {
            let pool = l2_pool.read();
            pool.read(old_slot, &mut buf)
        };

        if let Err(e) = read_result {
            warn!("Failed to read block {} from old L2: {:?}", block_id, e);
            migrated += 1;
            if total > 0 {
                progress.store((migrated * 100 / total).min(99), Ordering::Release);
            }
            continue;
        }

        match new_pool.allocate() {
            Some(slot) => {
                if let Err(e) = new_pool.write(&slot, &buf) {
                    warn!("Failed to write block {} to new L2: {:?}", block_id, e);
                    new_pool.free(&slot);
                } else {
                    new_arc_entries.push((
                        *block_id,
                        CachedBlockData {
                            size: block_size,
                            slot: SlotLocation::Ssd(slot.clone()),
                            l2_persistent: Some(slot.clone()),
                        },
                    ));
                    persistence_entries.push(CachedEntry {
                        block_id: *block_id,
                        backend_index: slot.backend,
                        slot_id: slot.slot,
                        in_t2: false,
                    });
                }
            }
            None => {
                info!(
                    "New L2 pool full at {}/{} blocks, evicting oldest",
                    migrated, total
                );
                break;
            }
        }

        migrated += 1;
        if total > 0 {
            progress.store((migrated * 100 / total).min(99), Ordering::Release);
        }
    }

    {
        let mut pool_guard = l2_pool.write();
        *pool_guard = new_pool;
    }

    {
        let old_count = {
            let cache = arc_cache.read();
            cache.len()
        };
        let new_cache = Arc::new(nova_cache_core::arc::ArcCache::new(
            old_count.max(new_arc_entries.len()),
        ));
        new_cache.restore_entries(new_arc_entries);
        *arc_cache.write() = new_cache;
    }

    let new_l2_path = &new_paths[0];
    let index_path = new_l2_path.with_file_name("cache_index.bin");
    if let Err(e) =
        persistence::save_cache_index(&index_path, 0, block_size as u32, &persistence_entries)
    {
        warn!("Failed to save cache index: {:?}", e);
    } else {
        info!(
            "Saved {} migrated entries to cache index",
            persistence_entries.len()
        );
    }

    info!(
        "Migration complete: {} blocks migrated, {} evicted",
        migrated,
        total - migrated
    );
    progress.store(100, Ordering::Release);

    std::thread::sleep(std::time::Duration::from_millis(500));
    progress.store(-1, Ordering::Release);
}
