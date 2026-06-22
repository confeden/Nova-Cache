use anyhow::{anyhow, Result};

use parking_lot::RwLock;

use std::path::PathBuf;

use std::sync::{Arc, Mutex};

use tracing::{error, info, warn};

fn resolve_volume_guid(drive_letter: char) -> Option<String> {
    let mount_point = format!("{}:\\\0", drive_letter);
    let mount_wide: Vec<u16> = mount_point.encode_utf16().collect();
    let mut volume_name = [0u16; 260];
    unsafe {
        if GetVolumeNameForVolumeMountPointW(PCWSTR(mount_wide.as_ptr()), &mut volume_name).is_ok()
        {
            let len = volume_name.iter().position(|&c| c == 0).unwrap_or(260);
            Some(String::from_utf16_lossy(&volume_name[..len]))
        } else {
            None
        }
    }
}

use nova_cache_core::persistence;
use nova_cache_core::persistence::CachedEntry;

use nova_cache_core::{
    arc::ArcCache,
    config::NovaCacheConfig,
    l2_pool::{L2Pool, L2Slot},
    pool::MemoryPool,
    stats::CacheStats,
};

#[derive(Debug, Clone)]
pub enum SlotLocation {
    Pool(usize),
    Ssd(L2Slot),
}

#[derive(Debug, Clone)]
pub struct CachedBlockData {
    pub size: usize,
    pub slot: SlotLocation,
    pub l2_persistent: Option<L2Slot>,
}

#[inline]
pub fn make_block_id(volume_id: u32, offset: u64, file_object: u64) -> u64 {
    if file_object == 0 {
        return ((volume_id as u64) << 32) | offset;
    }

    let mut x = ((volume_id as u64) << 56)
        ^ offset.rotate_left(17)
        ^ file_object.rotate_left(33)
        ^ 0x9E3779B97F4A7C15u64;
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58476D1CE4E5B9u64);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D049BB133111EBu64);
    x ^ (x >> 31)
}

use nova_cache_driver_comm::{
    messages::{ConnectionContext, DriverRequest},
    port::FilterPort,
    shared_mem::SharedMemoryRing,
};

use nova_cache_kdu::{
    dse::{is_dse_enabled, is_test_mode},
    hvci::{is_hvci_enabled, is_vbs_enabled, get_windows_build_number},
    loader::{disable_dse, enable_dse},
    scm,
};

use nova_cache_monitor::etw::EtwMonitor;
use nova_cache_monitor::heatmap::Heatmap;

use windows::core::PCWSTR;
use windows::Win32::Storage::FileSystem::GetVolumeNameForVolumeMountPointW;

pub struct ServiceStateShared {
    pub cache_generation: u64,
    pub config: RwLock<NovaCacheConfig>,
    pub arc_cache: Arc<RwLock<Arc<ArcCache<u64, CachedBlockData>>>>,
    pub pool: RwLock<Arc<MemoryPool>>,
    pub l2_pool: Arc<RwLock<L2Pool>>,
    pub stats: Arc<CacheStats>,
    pub shared_ring: RwLock<Arc<SharedMemoryRing>>,
    pub l2_migration_progress: Arc<std::sync::atomic::AtomicI32>,
    pub perf_tracker: Arc<parking_lot::Mutex<crate::perf_tracker::PerformanceTracker>>,
    pub journal: Arc<crate::journal::Journal>,
    pub dirty_blocks:
        Arc<parking_lot::Mutex<std::collections::HashMap<u64, crate::flush_thread::DirtyBlock>>>,
    pub shutdown_requested: Arc<std::sync::atomic::AtomicBool>,
    pub l2_eviction_queue: Arc<parking_lot::Mutex<std::collections::VecDeque<(u64, L2Slot)>>>,
    pub predictor: Arc<nova_cache_core::predictor::MarkovPredictor>,
    pub l2_priority: Arc<nova_cache_core::l2_priority::L2PriorityHeap>,
    pub block_metadata_map: Arc<dashmap::DashMap<u64, crate::prefetch::BlockMetadata>>,
    pub prefetched_blocks: crate::prefetch::PrefetchedSet,
    pub prefetch_engine: Arc<parking_lot::Mutex<nova_cache_monitor::prefetch::PrefetchEngine>>,
    pub prefetch_sender: crossbeam::channel::Sender<crate::prefetch::PrefetchJob>,
    pub l2_writer_tx: crossbeam::channel::Sender<crate::l2_writer::L2WriteJob>,
    pub pending_l2_writes: Arc<dashmap::DashSet<u64>>,
    pub file_object_map: Arc<dashmap::DashMap<u64, u64>>,
}

pub struct ServiceOrchestrator {
    #[allow(dead_code)]
    state: Arc<ServiceStateShared>,

    driver_port: Option<FilterPort>,

    etw_monitor: Option<EtwMonitor>,

    ipc_server: Option<crate::ipc::IpcServer>,

    running: Arc<std::sync::atomic::AtomicBool>,

    flush_thread: Option<std::thread::JoinHandle<()>>,

    flush_running: Arc<std::sync::atomic::AtomicBool>,

    prefetch_threads: Vec<std::thread::JoinHandle<()>>,

    l2_writer_tx: Option<crossbeam::channel::Sender<crate::l2_writer::L2WriteJob>>,

    l2_writer_thread: Option<std::thread::JoinHandle<()>>,
}

impl ServiceOrchestrator {
    pub async fn new() -> Result<Self> {
        info!("Initializing Nova Cache orchestrator...");

        // 1. Verify system environment

        let test_mode = is_test_mode();

        let dse_enabled = is_dse_enabled();

        let vbs_running = is_vbs_enabled();

        let hvci_running = is_hvci_enabled();

        info!(
            "System boot environment: TestSigning={}, DSE={}, VBS={}, HVCI={}",
            test_mode, dse_enabled, vbs_running, hvci_running
        );

        // 2. Load Config

        // Resolve project root: go up from exe dir (target/debug/) to project root
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("."));

        let cwd = std::env::current_dir().unwrap_or_else(|_| exe_dir.clone());

        // Search order: cwd/config, exe_dir/config, exe_dir/../../config (project root)
        let (config_path, base_dir) = if cwd.join("config").join("nova_cache.toml").exists() {
            (cwd.join("config").join("nova_cache.toml"), cwd)
        } else if exe_dir.join("config").join("nova_cache.toml").exists() {
            (exe_dir.join("config").join("nova_cache.toml"), exe_dir)
        } else if let Some(project_root) = exe_dir.parent().and_then(|p| p.parent()) {
            let p = project_root.join("config").join("nova_cache.toml");
            if p.exists() {
                (p, project_root.to_path_buf())
            } else {
                return Err(anyhow!(
                    "Config file not found. Searched: {}/config/, {}/config/, {}/config/",
                    cwd.display(),
                    exe_dir.display(),
                    project_root.display()
                ));
            }
        } else {
            return Err(anyhow!(
                "Config file not found. Expected at: {}/config/nova_cache.toml",
                cwd.display()
            ));
        };

        let mut config = NovaCacheConfig::load(&config_path)?;

        info!("Configuration loaded successfully.");

        // Resolve relative paths against base_dir
        if !config.cache.l2.path.as_os_str().is_empty() && config.cache.l2.path.is_relative() {
            config.cache.l2.path = base_dir.join(&config.cache.l2.path);
        }
        if config.kdu.kdu_path.is_relative() {
            config.kdu.kdu_path = base_dir.join(&config.kdu.kdu_path);
        }
        if config.general.log_path.is_relative() {
            config.general.log_path = base_dir.join(&config.general.log_path);
        }

        info!("Resolved L2 path: {}", config.cache.l2.path.display());
        info!("Resolved KDU path: {}", config.kdu.kdu_path.display());

        // 3. Initialize Core Caching components

        let capacity = (config.cache.l1_size_mb as u64 * 1024 * 1024)
            / (config.cache.block_size_kb as u64 * 1024);
        let arc_cache = Arc::new(ArcCache::new(capacity as usize));

        // TinyLFU admission filter: prevents cache pollution from one-shot reads
        let tinylfu = Arc::new(nova_cache_core::tinylfu::TinyLFU::new(
            (capacity * 4) as usize,
        ));

        let num_slots = (config.cache.l1_size_mb as usize * 1024 * 1024)
            / (config.cache.block_size_kb as usize * 1024);
        let pool = Arc::new(MemoryPool::new(
            num_slots,
            config.cache.block_size_kb as usize * 1024,
        ));

        // Build L2 backends list: skip if L2 disabled or path empty
        let block_size = config.cache.block_size_kb as usize * 1024;
        let mut l2_paths: Vec<PathBuf> = Vec::new();
        if config.cache.l2.enable && config.cache.l2.size_gb > 0 {
            let primary = config.cache.l2.path.clone();
            if !primary.as_os_str().is_empty() {
                l2_paths.push(primary);
                for extra in &config.cache.l2.backends {
                    if !l2_paths.contains(extra) {
                        l2_paths.push(extra.clone());
                    }
                }
            }
        }
        let num_backends = l2_paths.len() as u64;
        let size_per_backend = if num_backends > 0 {
            (config.cache.l2.size_gb as u64 * 1024 * 1024 * 1024) / num_backends
        } else {
            (config.cache.l2.size_gb as u64) * 1024 * 1024 * 1024
        };

        let l2_pool = if l2_paths.is_empty() {
            Arc::new(RwLock::new(L2Pool::empty()))
        } else {
            Arc::new(RwLock::new(L2Pool::new(
                &l2_paths,
                size_per_backend,
                block_size,
            )?))
        };

        {
            let pool_guard = l2_pool.read();
            info!(
                "L2 pool: {} backends, {} total slots",
                pool_guard.backend_count(),
                pool_guard.total_slots()
            );
            for (path, speed, free, total) in pool_guard.backend_info() {
                info!(
                    "  Backend {}: {:.1} MB/s, {}/{} slots free",
                    path.display(),
                    speed,
                    free,
                    total
                );
            }
        }

        let stats = Arc::new(CacheStats::new());

        // Create shared ring early so state can hold a reference
        // Ring capacity is independent of cache capacity — ring is staging only
        let block_size_bytes = config.cache.block_size_kb as usize * 1024;
        let ring_capacity: usize = 4096;
        let l2_total_slots = l2_pool.read().total_slots();
        let shared_ring = Arc::new(SharedMemoryRing::create(
            "Global\\NovaCacheSharedMem",
            ring_capacity,
            capacity as usize,
            l2_total_slots,
            block_size_bytes,
        )?);

        // Generate random cache generation — changes every service start, invalidates stale L2 entries
        let cache_generation = {
            let ticks = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64;
            let pid = std::process::id() as u64;
            ticks.wrapping_mul(6364136223846793005).wrapping_add(pid)
        };

        // Use primary L2 path for journal/index, or temp dir if L2 disabled
        let l2_primary = l2_paths
            .first()
            .cloned()
            .unwrap_or_else(|| std::env::temp_dir().join("NovaCache").join("l2_disabled"));

        // Phase 3.1a: Open journal first — replays uncommitted entries to L2
        let journal_path = l2_primary.with_file_name("nova_journal.bin");
        let journal = Arc::new(crate::journal::Journal::open(
            journal_path,
            l2_pool.clone(),
            block_size,
        )?);

        // Phase 3.1b: Restore L1 cache index from disk
        let index_path = l2_primary.with_file_name("cache_index.bin");
        let mut index_loaded = false;
        match persistence::load_cache_index(&index_path) {
            Ok(Some((_block_size, saved_gen, entries))) => {
                if saved_gen != cache_generation {
                    info!(
                        "Cache index generation mismatch: saved=0x{:016X}, current=0x{:016X}. Discarding {} stale entries.",
                        saved_gen,
                        cache_generation,
                        entries.len()
                    );
                } else {
                    let mut restored_l2: usize = 0;
                    let mut restored_arc_entries = Vec::new();
                    let pool_guard = l2_pool.read();
                    for entry in &entries {
                        let slot = L2Slot {
                            backend: entry.backend_index,
                            slot: entry.slot_id,
                        };
                        if pool_guard.is_slot_valid(&slot) && pool_guard.is_healthy() {
                            let block_data = CachedBlockData {
                                size: block_size,
                                slot: SlotLocation::Ssd(slot.clone()),
                                l2_persistent: Some(slot),
                            };
                            restored_arc_entries.push((entry.block_id, block_data));
                            restored_l2 += 1;
                        }
                    }
                    drop(pool_guard);

                    if !restored_arc_entries.is_empty() {
                        // Reserve L2 slots so allocator doesn't overwrite recovered data
                        let mut reserved: std::collections::HashSet<usize> =
                            std::collections::HashSet::new();
                        for entry in &entries {
                            reserved.insert(entry.slot_id as usize);
                        }
                        let reserved_vec: Vec<usize> = reserved.into_iter().collect();
                        l2_pool.read().reserve_slots_for_backend(0, &reserved_vec);

                        arc_cache.restore_entries(restored_arc_entries);
                        stats
                            .l2_block_count
                            .fetch_add(restored_l2 as u64, std::sync::atomic::Ordering::Relaxed);
                        info!(
                            "Restored {} L2 entries from cache index ({} slots reserved)",
                            restored_l2,
                            reserved_vec.len()
                        );
                        index_loaded = true;
                    } else {
                        info!("Cache index loaded but no valid entries could be restored");
                    }
                }
            }
            Ok(None) => {
                info!("No cache index found, will rebuild from journal if possible");
            }
            Err(e) => {
                warn!(
                    "Failed to load cache index: {:?}, will rebuild from journal",
                    e
                );
            }
        }

        // Phase 3.1c: If cache_index.bin was missing/empty, rebuild from journal committed entries
        if !index_loaded {
            match journal.scan_committed_entries() {
                Ok(committed) if !committed.is_empty() => {
                    info!(
                        "Journal has {} committed entries, rebuilding cache index...",
                        committed.len()
                    );

                    let l2_file_size = std::fs::metadata(&l2_primary)
                        .map(|m| m.len())
                        .unwrap_or(0);

                    let meta: Vec<(u64, u32, u64, u32, u32)> = committed
                        .iter()
                        .map(|e| (e.block_id, e.volume_id, e.offset, e.length, e.slot_id))
                        .collect();

                    if let Err(e) = persistence::rebuild_cache_index(
                        &index_path,
                        cache_generation,
                        block_size as u32,
                        l2_file_size,
                        &meta,
                    ) {
                        warn!("Failed to rebuild cache index from journal: {:?}", e);
                    } else {
                        match persistence::load_cache_index(&index_path) {
                            Ok(Some((_block_size, saved_gen, entries))) => {
                                if saved_gen != cache_generation {
                                    info!(
                                        "Rebuilt cache index generation mismatch: saved=0x{:016X}, current=0x{:016X}. Discarding {} entries.",
                                        saved_gen, cache_generation, entries.len()
                                    );
                                } else {
                                    let mut restored_l2: usize = 0;
                                    let mut restored_arc_entries = Vec::new();
                                    let pool_guard = l2_pool.read();
                                    for entry in &entries {
                                        let slot = L2Slot {
                                            backend: entry.backend_index,
                                            slot: entry.slot_id,
                                        };
                                        if pool_guard.is_slot_valid(&slot) && pool_guard.is_healthy() {
                                            let block_data = CachedBlockData {
                                                size: block_size,
                                                slot: SlotLocation::Ssd(slot.clone()),
                                                l2_persistent: Some(slot),
                                            };
                                            restored_arc_entries.push((entry.block_id, block_data));
                                            restored_l2 += 1;
                                        }
                                    }
                                    drop(pool_guard);

                                    if !restored_arc_entries.is_empty() {
                                        let mut reserved: std::collections::HashSet<usize> =
                                            std::collections::HashSet::new();
                                        for entry in &entries {
                                            reserved.insert(entry.slot_id as usize);
                                        }
                                        let reserved_vec: Vec<usize> = reserved.into_iter().collect();
                                        l2_pool.read().reserve_slots_for_backend(0, &reserved_vec);

                                        arc_cache.restore_entries(restored_arc_entries);
                                        stats.l2_block_count.fetch_add(
                                            restored_l2 as u64,
                                            std::sync::atomic::Ordering::Relaxed,
                                        );
                                        info!(
                                            "Restored {} L2 entries from rebuilt cache index ({} slots reserved)",
                                            restored_l2,
                                            reserved_vec.len()
                                        );
                                    }
                                }
                            }
                            _ => {
                                warn!("Failed to load rebuilt cache index");
                            }
                        }
                    }
                }
                Ok(_) => {
                    info!("Journal has no committed entries, starting with empty cache");
                }
                Err(e) => {
                    warn!("Failed to scan journal for rebuild: {:?}", e);
                }
            }
        }
        let dirty_blocks = Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));

        let l2_pool_slots = l2_pool.read().total_slots();
        let predictor = Arc::new(nova_cache_core::predictor::MarkovPredictor::new());
        let l2_priority = Arc::new(nova_cache_core::l2_priority::L2PriorityHeap::new(
            l2_pool_slots,
        ));
        let (prefetch_sender, prefetch_receiver) = crossbeam::channel::bounded(4096);
        let block_metadata_map = Arc::new(dashmap::DashMap::new());
        let prefetched_blocks = crate::prefetch::new_prefetched_set();

        let worker_count = config.prefetch.worker_threads;

        let prefetch_engine = {
            let pc = &config.prefetch;
            let block_size = config.cache.block_size_kb as u64 * 1024;
            Arc::new(parking_lot::Mutex::new(
                nova_cache_monitor::prefetch::PrefetchEngine::new(
                    block_size,
                    pc.sequential_threshold_kb,
                    pc.prefetch_ahead_mb,
                    pc.prefetch_min_window_mb,
                    pc.prefetch_max_window_mb,
                ),
            ))
        };

        let (l2_writer_tx, l2_writer_rx) = crossbeam::channel::unbounded::<crate::l2_writer::L2WriteJob>();
        let pending_l2_writes = Arc::new(dashmap::DashSet::new());
        let file_object_map = Arc::new(dashmap::DashMap::new());

        let state = Arc::new(ServiceStateShared {
            cache_generation,
            config: RwLock::new(config),
            arc_cache: Arc::new(RwLock::new(arc_cache)),
            pool: RwLock::new(pool),
            l2_pool,
            stats,
            shared_ring: RwLock::new(shared_ring.clone()),
            l2_migration_progress: Arc::new(std::sync::atomic::AtomicI32::new(-1)),
            perf_tracker: Arc::new(parking_lot::Mutex::new(
                crate::perf_tracker::PerformanceTracker::new(),
            )),
            journal: journal.clone(),
            dirty_blocks: dirty_blocks.clone(),
            shutdown_requested: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            l2_eviction_queue: Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new())),
            predictor,
            l2_priority,
            block_metadata_map,
            prefetched_blocks,
            prefetch_engine,
            prefetch_sender,
            l2_writer_tx: l2_writer_tx.clone(),
            pending_l2_writes: pending_l2_writes.clone(),
            file_object_map: file_object_map.clone(),
        });

        let prefetch_handles = crate::prefetch::start_prefetch_workers(
            state.clone(),
            prefetch_receiver,
            block_size_bytes,
            worker_count,
        );

        let l2_writer_handle = crate::l2_writer::start_l2_writer(l2_writer_rx, state.clone());

        // Start IPC server early so GUI can connect while driver loads
        let ipc_server = Some(crate::ipc::IpcServer::start(state.clone()));

        // 4. Register and load driver

        let driver_path = base_dir
            .join("driver")
            .join("novacache")
            .join("Release")
            .join("Novacache.sys");

        // 4a. Stop and fully unload any previous driver instance
        info!("Stopping any previous Novacache driver instance...");
        scm::stop_driver_service("Novacache");

        // 4a+. Ensure driver binary is signed (test certificate required even in test signing mode)
        info!("Ensuring driver binary is signed...");
        if let Err(e) = scm::sign_driver_binary(&driver_path) {
            warn!(
                "Driver signing failed (may already be signed or cert unavailable): {}",
                e
            );
        }

        // 4b. Register the minifilter service and its Instances keys

        info!("Registering Novacache minifilter service...");

        scm::register_minifilter_service("Novacache", &driver_path)?;

        // 4c. Load the driver safely

        let start_result = if test_mode || !dse_enabled {
            if test_mode {
                info!("Test Signing Mode is active. Loading signed driver natively...");
            } else {
                info!("Driver Signature Enforcement (DSE) is disabled. Loading driver natively...");
            }

            scm::start_driver_service("Novacache")
        } else {
            // DSE is active and Test Signing is NOT active

            // Under Hyper-V/VBS, or on Windows 11 / build 22000+, DSE bypass via KDU will BSOD the system
            let build_num = get_windows_build_number().unwrap_or(0);
            let is_modern_windows = build_num >= 22000;

            if hvci_running || vbs_running || is_modern_windows {
                error!("Driver Signature Enforcement (DSE) is active, and Test Signing Mode is not active.");
                if is_modern_windows {
                    error!("Because this system is running Windows 11 (build {}), the kernel g_CiOptions is write-protected. Attempting a DSE bypass via KDU will cause a system crash (BSOD).", build_num);
                } else {
                    error!("Because Hyper-V/VBS is enabled on this system, attempting a DSE bypass via KDU will cause a system crash (BSOD).");
                }

                return Err(anyhow!(
                    "DSE is active and Test Signing is disabled. Please restart your PC to apply the Test Signing Mode enabled in BCD, then start the service again."
                ));
            }

            warn!(
                "DSE is active, but VBS/Hyper-V is not detected. Falling back to KDU DSE bypass..."
            );

            let (kdu_path, primary_provider, fallbacks) = {
                let conf = state.config.read();
                (
                    conf.kdu.kdu_path.clone(),
                    conf.kdu.provider_id,
                    conf.kdu.fallback_providers.clone(),
                )
            };

            let mut providers = vec![primary_provider];
            providers.extend(fallbacks);

            let mut active_provider = None;
            let mut last_error = None;

            for &prov_id in &providers {
                info!("Trying to temporarily disable DSE using KDU provider {}...", prov_id);
                match disable_dse(&kdu_path, prov_id) {
                    Ok(_) => {
                        active_provider = Some(prov_id);
                        break;
                    }
                    Err(e) => {
                        warn!("KDU provider {} failed: {:?}. Trying fallback...", prov_id, e);
                        last_error = Some(e);
                    }
                }
            }

            let active_prov_id = match active_provider {
                Some(id) => id,
                None => {
                    return Err(anyhow!(
                        "Failed to disable DSE. All KDU providers failed. Last error: {:?}",
                        last_error
                    ));
                }
            };

            let res = scm::start_driver_service("Novacache");

            // Always re-enable DSE
            info!("Re-enabling DSE via KDU using provider {}...", active_prov_id);
            if let Err(e) = enable_dse(&kdu_path, active_prov_id) {
                error!(
                    "CRITICAL: Failed to re-enable DSE: {:?}. System security may be degraded!",
                    e
                );
            }

            res
        };

        // Now propagate any start error

        start_result?;

        info!("Novacache driver loaded successfully.");

        // Set volume bitmap from config — each enabled volume letter sets its bit
        {
            let conf = state.config.read();
            let mut bitmap: u32 = 0;
            for vol in &conf.volumes {
                if vol.enabled {
                    let ch = vol.volume.chars().next().unwrap_or('\0');
                    let idx = if ch >= 'A' && ch <= 'Z' {
                        (ch as u32) - ('A' as u32)
                    } else if ch >= 'a' && ch <= 'z' {
                        (ch as u32) - ('a' as u32)
                    } else {
                        continue;
                    };
                    bitmap |= 1u32 << idx;
                    info!("Volume {} enabled for caching (bit {})", ch, idx);
                }
            }
            state.shared_ring.read().set_volume_bitmap(bitmap);
            state.shared_ring.read().set_write_back_enabled(true);
            info!("Volume bitmap: 0x{:08X}, write-back: enabled", bitmap);
        }

        let l2_path_str = format!(r"\??\{}", state.config.read().cache.l2.path.to_string_lossy());
        let ctx = ConnectionContext::new(
            state.shared_ring.read().get_section_handle().0 as u64,
            &format!(
                "\\BaseNamedObjects\\{}",
                state.shared_ring.read().get_event_name()
            ),
            &l2_path_str,
        );

        // 6. Connect to Driver Communication Port

        let driver_port = match FilterPort::connect(r"\NovaCachePort", &ctx) {
            Ok(p) => {
                info!("Connected to Novacache driver port.");

                Some(p)
            }

            Err(e) => {
                warn!(
                    "Could not connect to driver port: {:?}. Driver communication is disabled.",
                    e
                );

                None
            }
        };

        // 6a. Send StartCaching command for each enabled volume
        if let Some(ref port) = driver_port {
            let conf = state.config.read();
            let cache_size_mb = conf.cache.l1_size_mb;
            let block_size_kb = conf.cache.block_size_kb;
            for vol in &conf.volumes {
                if vol.enabled {
                    let ch = vol.volume.chars().next().unwrap_or('\0');
                    match resolve_volume_guid(ch) {
                        Some(guid) => {
                            let req = DriverRequest::new_start_caching(
                                &guid,
                                cache_size_mb,
                                block_size_kb,
                            );
                            let mut resp_buf = [0u8; std::mem::size_of::<
                                nova_cache_driver_comm::messages::DriverResponse,
                            >()];
                            match port.send_message(bytemuck::bytes_of(&req), &mut resp_buf) {
                                Ok(_) => {
                                    info!("StartCaching sent for volume {} ({})", ch, guid);
                                }
                                Err(e) => {
                                    warn!("Failed to send StartCaching for volume {}: {:?}", ch, e);
                                }
                            }
                        }
                        None => {
                            warn!("Could not resolve volume GUID for drive {}:", ch);
                        }
                    }
                }
            }
        }

        // Spawn Shared Memory drain task

        let running = Arc::new(std::sync::atomic::AtomicBool::new(true));

        let state_clone = state.clone();

        let running_clone = running.clone();

        let tinylfu_clone = tinylfu.clone();
        let hot_blocks: Arc<parking_lot::Mutex<std::collections::HashSet<u64>>> =
            Arc::new(parking_lot::Mutex::new(std::collections::HashSet::new()));
        let hot_blocks_consumer = hot_blocks.clone();

        tokio::task::spawn_blocking(move || {
            use windows::Win32::Foundation::WAIT_OBJECT_0;

            use windows::Win32::System::Threading::WaitForSingleObject;

            info!("Shared Memory consumer thread started.");

            let mut pop_count: u64 = 0;
            let mut wait_timeout_count: u64 = 0;
            let mut last_index_save = std::time::Instant::now();
            const INDEX_SAVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

            let mut last_block_id: u64 = u64::MAX;
            let mut priority_age_counter: u32 = 0;
            let mut last_read_offsets = std::collections::HashMap::<u64, u64>::new();
            let mut epoch_total_reads: u64 = 0;
            let mut epoch_cache_hits: u64 = 0;
            let mut epoch_seq_reads: u64 = 0;
            let mut current_age_interval: u32 = 10000;

            while running_clone.load(std::sync::atomic::Ordering::Relaxed) {
                // Re-read current state each iteration for runtime resize support

                let (event, ring, pool, l2_pool, arc_cache, stats) = {
                    let ring = state_clone.shared_ring.read().clone();

                    let event = ring.get_data_event();

                    let pool = state_clone.pool.read().clone();

                    let l2_pool = state_clone.l2_pool.clone();

                    let arc_cache = state_clone.arc_cache.read().clone();

                    let stats = state_clone.stats.clone();

                    (event, ring, pool, l2_pool, arc_cache, stats)
                };

                let wait_res = unsafe { WaitForSingleObject(event, 500) };

                // Process ring buffer on BOTH event signal AND timeout.
                // The event may not be reliably signaled if the driver's
                // KeSetEvent fires before consumer starts waiting, or if
                // the event handle wasn't properly passed.
                {
                    let mut batch_count: u64 = 0;
                    while let Some((desc, data)) = ring.pop() {
                        pop_count += 1;
                        batch_count += 1;

                        if desc.flags & 0x08 != 0 {
                            let mut blocks_to_remove = Vec::new();
                            for entry in state_clone.block_metadata_map.iter() {
                                if entry.value().volume_id == desc.volume_id && entry.value().file_object == desc.file_object {
                                    blocks_to_remove.push(*entry.key());
                                }
                            }
                            
                            for block_id in blocks_to_remove {
                                let mut evicted_from_l1 = false;
                                if let Some(cached_block) = arc_cache.remove(&block_id) {
                                    evicted_from_l1 = true;
                                    match cached_block.slot {
                                        SlotLocation::Pool(slot) => {
                                            pool.free(slot);
                                            stats.l1_block_count.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                                            state_clone.shared_ring.read().invalidate_l1_cache(
                                                desc.volume_id,
                                                state_clone.block_metadata_map.get(&block_id).map(|m| m.offset).unwrap_or(0),
                                                desc.file_object as usize,
                                            );
                                            if let Some(ref l2_slot) = cached_block.l2_persistent {
                                                l2_pool.read().free(l2_slot);
                                                state_clone.l2_priority.clear_slot(l2_slot.slot);
                                                stats.l2_block_count.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                                                state_clone.shared_ring.read().insert_l2_cache(
                                                    desc.volume_id,
                                                    state_clone.block_metadata_map.get(&block_id).map(|m| m.offset).unwrap_or(0),
                                                    0,
                                                    desc.file_object as usize,
                                                    l2_slot.slot,
                                                    false,
                                                );
                                            }
                                        }
                                        SlotLocation::Ssd(ref slot) => {
                                            l2_pool.read().free(slot);
                                            state_clone.l2_priority.clear_slot(slot.slot);
                                            stats.l2_block_count.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                                            state_clone.shared_ring.read().insert_l2_cache(
                                                desc.volume_id,
                                                state_clone.block_metadata_map.get(&block_id).map(|m| m.offset).unwrap_or(0),
                                                0,
                                                desc.file_object as usize,
                                                slot.slot,
                                                false,
                                            );
                                        }
                                    }
                                }
                                
                                if !evicted_from_l1 {
                                    if let Some(slot_id) = state_clone.l2_priority.find_and_clear_block(block_id) {
                                        let slot = L2Slot {
                                            backend: 0,
                                            slot: slot_id,
                                        };
                                        l2_pool.read().free(&slot);
                                        stats.l2_block_count.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                                        state_clone.shared_ring.read().insert_l2_cache(
                                            desc.volume_id,
                                            state_clone.block_metadata_map.get(&block_id).map(|m| m.offset).unwrap_or(0),
                                            0,
                                            desc.file_object as usize,
                                            slot_id,
                                            false,
                                        );
                                    }
                                }
                                
                                state_clone.block_metadata_map.remove(&block_id);
                            }
                            continue;
                        }

                        let block_id = make_block_id(desc.volume_id, desc.offset, desc.file_object);

                        // Dynamic sequential read detection
                        let is_sequential = {
                            if last_read_offsets.len() > 8192 {
                                last_read_offsets.clear();
                            }
                            let last_end = last_read_offsets
                                .insert(desc.file_object, desc.offset + desc.length as u64);
                            if let Some(end) = last_end {
                                desc.offset == end
                            } else {
                                false
                            }
                        };

                        epoch_total_reads += 1;
                        if is_sequential {
                            epoch_seq_reads += 1;
                        }

                        // Check if this block was prefetched (effectiveness tracking)
                        crate::prefetch::check_prefetch_effectiveness(
                            block_id,
                            &state_clone.prefetched_blocks,
                            &state_clone.stats,
                            &state_clone.prefetch_engine,
                        );

                        // Compute CRC32 for data integrity
                        let block_crc32 = crate::perf_tracker::crc32c(&data);

                        // Record HDD latency from driver timestamps
                        {
                            let mut pt = state_clone.perf_tracker.lock();
                            if (desc.flags & 0x1) != 0 || (desc.flags & 0x2) != 0 {
                                // Write-through or Write-back
                                pt.record_hdd_write(desc.pre_op_tick, desc.post_op_tick);
                            } else {
                                // Read miss
                                pt.record_hdd_read(desc.pre_op_tick, desc.post_op_tick);
                            }
                            // Debug: log first few timestamps
                            if pop_count <= 3 {
                                tracing::info!(
                                    "Pop #{}: pre_op_tick={}, post_op_tick={}, flags=0x{:x}",
                                    pop_count,
                                    desc.pre_op_tick,
                                    desc.post_op_tick,
                                    desc.flags
                                );
                            }
                        }

                        // Map block_id to its volume, offset, and file_object for prefetching
                        state_clone.block_metadata_map.insert(
                            block_id,
                            crate::prefetch::BlockMetadata {
                                volume_id: desc.volume_id,
                                offset: desc.offset,
                                file_object: desc.file_object,
                            },
                        );

                        // Cap block_metadata_map to prevent unbounded growth (every ~1000 pops)
                        if pop_count % 1000 == 0 {
                            let max = state_clone.config.read().prefetch.max_block_metadata;
                            let current = state_clone.block_metadata_map.len();
                            if current > max {
                                let to_remove = current - max;
                                let keys: Vec<u64> = state_clone
                                    .block_metadata_map
                                    .iter()
                                    .take(to_remove)
                                    .map(|e| *e.key())
                                    .collect();
                                for key in keys {
                                    state_clone.block_metadata_map.remove(&key);
                                }
                                tracing::debug!(
                                    "Trimmed {} entries from block_metadata_map ({} -> {})",
                                    to_remove, current, max
                                );
                            }
                        }

                        // Record file_object for ETW sequential prefetch
                        let vol_off_key = ((desc.volume_id as u64) << 32)
                            | (desc.offset & !(block_size as u64 - 1));
                        state_clone
                            .file_object_map
                            .insert(vol_off_key, desc.file_object);

                        // Record Markov transition
                        state_clone
                            .predictor
                            .record_transition(last_block_id, block_id);
                        last_block_id = block_id;

                        // Predict and prefetch from L2/HDD to L1 asynchronously
                        if let Some(predicted_id) = state_clone.predictor.predict(block_id) {
                            if predicted_id != block_id {
                                if let Some(metadata) =
                                    state_clone.block_metadata_map.get(&predicted_id)
                                {
                                    let job = crate::prefetch::PrefetchJob {
                                        block_id: predicted_id,
                                        volume_id: metadata.volume_id,
                                        offset: metadata.offset,
                                        file_object: metadata.file_object,
                                    };
                                    if let Err(_e) = state_clone.prefetch_sender.try_send(job) {
                                        // Silent discard on queue full / block path safety
                                    }
                                }
                            }
                        }
                        let is_write = (desc.flags & 0x1 != 0) || (desc.flags & 0x2 != 0);
                        if is_write {
                            let mut l2_slot_freed = false;
                            if let Some(cached_block) = arc_cache.remove(&block_id) {
                                match cached_block.slot {
                                    SlotLocation::Pool(slot) => {
                                        pool.free(slot);
                                        stats.l1_block_count.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                                        if let Some(meta) = state_clone.block_metadata_map.get(&block_id) {
                                            state_clone.shared_ring.read().invalidate_l1_cache(
                                                meta.volume_id,
                                                meta.offset,
                                                meta.file_object as usize,
                                            );
                                        }
                                    }
                                    SlotLocation::Ssd(_) => {}
                                }
                                if let Some(ref l2_slot) = cached_block.l2_persistent {
                                    l2_pool.read().free(l2_slot);
                                    state_clone.l2_priority.clear_slot(l2_slot.slot);
                                    stats.l2_block_count.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                                    l2_slot_freed = true;
                                }
                            }

                            let mut dirty_map = state_clone.dirty_blocks.lock();
                            if let Some(old) = dirty_map.remove(&block_id) {
                                if let Err(e) = state_clone.journal.commit(old.journal_sequence) {
                                    warn!("Failed to commit invalidated journal entry {}: {:?}", old.journal_sequence, e);
                                }
                                if !l2_slot_freed {
                                    let slot = L2Slot {
                                        backend: 0,
                                        slot: old.slot_id,
                                    };
                                    l2_pool.read().free(&slot);
                                    state_clone.l2_priority.clear_slot(old.slot_id);
                                    stats.l2_block_count.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                                }
                            }
                            state_clone.shared_ring.read().set_dirty_count(dirty_map.len() as u32);
                            drop(dirty_map);

                            state_clone.shared_ring.read().insert_l2_cache(
                                desc.volume_id,
                                desc.offset,
                                0,
                                desc.file_object as usize,
                                0,
                                false,
                            );

                            if data.len() < block_size {
                                continue;
                            }
                        }

                        let is_sub_block = data.len() < block_size;

                        if is_sub_block {
                            if (desc.flags & 0x1 == 0) && (desc.flags & 0x2 == 0) {
                                // Sub-block read miss: queue prefetch of the full block asynchronously
                                let job = crate::prefetch::PrefetchJob {
                                    block_id,
                                    volume_id: desc.volume_id,
                                    offset: desc.offset,
                                    file_object: desc.file_object,
                                };
                                if let Err(_e) = state_clone.prefetch_sender.try_send(job) {
                                    // Silent discard on queue full / block path safety
                                }
                            } else {
                                // Sub-block write: invalidate L2 slot
                                state_clone.shared_ring.read().insert_l2_cache(
                                    desc.volume_id,
                                    desc.offset,
                                    0,
                                    desc.file_object as usize,
                                    0,
                                    false,
                                );
                            }
                        } else {
                            // Insert into L1 cache so driver can serve future reads from RAM
                            {
                                let ring = state_clone.shared_ring.read();
                                ring.insert_l1_cache(
                                    desc.volume_id,
                                    desc.offset,
                                    &data,
                                    0,
                                    desc.file_object as usize,
                                );
                            }

                            // Track this access in ARC (registers miss if not cached, promotes if cached)
                            if let Some(cached_block) = arc_cache.get(&block_id) {
                                epoch_cache_hits += 1;
                                state_clone.perf_tracker.lock().record_cache_read(1000);
                                if let Some(ref l2_slot) = cached_block.l2_persistent {
                                    state_clone.l2_priority.record_hit(l2_slot.slot);
                                    // CRITICAL: Since the driver missed, we must insert this L2 entry back into the driver's L2 directory
                                    // so that subsequent reads can hit in the driver!
                                    state_clone.shared_ring.read().insert_l2_cache(
                                        desc.volume_id,
                                        desc.offset,
                                        crate::perf_tracker::crc32c(&data),
                                        desc.file_object as usize,
                                        l2_slot.slot,
                                        true,
                                    );
                                }
                            }

                            // Age priorities periodically using adaptive interval
                            priority_age_counter += 1;
                            if priority_age_counter >= current_age_interval {
                                state_clone.l2_priority.age_all();
                                priority_age_counter = 0;
                            }

                            let is_write_back = desc.flags & 0x2 != 0;
                            let mut l2_persistent: Option<L2Slot> = None;

                            if is_write_back {
                                // For write-back:
                                let mut l2_slot = None;
                                if let Some(cached_block) = arc_cache.get(&block_id) {
                                    if let Some(ref slot) = cached_block.l2_persistent {
                                        l2_slot = Some(slot.clone());
                                    }
                                }

                                if l2_slot.is_none() && l2_pool.read().is_healthy() {
                                    let pool_guard = l2_pool.read();
                                    l2_slot = pool_guard.allocate();
                                    drop(pool_guard);
                                    
                                    if l2_slot.is_none() {
                                        // Evict worst block using Smart Priority
                                        let evicted_entry = state_clone.l2_priority.evict_worst();
                                        if let Some((evicted_slot_id, evicted_block_id)) = evicted_entry {
                                            info!("L2 full: PriorityHeap evicting block 0x{:016X} from slot {}", evicted_block_id, evicted_slot_id);
                                            let evicted_l2_slot = L2Slot {
                                                backend: 0,
                                                slot: evicted_slot_id,
                                            };
                                            l2_pool.read().free(&evicted_l2_slot);
                                            stats.l2_block_count.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);

                                            if let Some(meta) = state_clone.block_metadata_map.get(&evicted_block_id) {
                                                state_clone.shared_ring.read().insert_l2_cache(
                                                    meta.volume_id,
                                                    meta.offset,
                                                    0,
                                                    meta.file_object as usize,
                                                    evicted_slot_id,
                                                    false,
                                                );
                                            }

                                            if let Some(evicted_data) = arc_cache.remove(&evicted_block_id) {
                                                match evicted_data.slot {
                                                    SlotLocation::Pool(slot) => {
                                                        pool.free(slot);
                                                        stats.l1_block_count.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                                                        if let Some(meta) = state_clone.block_metadata_map.get(&evicted_block_id) {
                                                            state_clone.shared_ring.read().invalidate_l1_cache(
                                                                meta.volume_id,
                                                                meta.offset,
                                                                meta.file_object as usize,
                                                            );
                                                        }
                                                    }
                                                    SlotLocation::Ssd(_) => {}
                                                }
                                            }

                                            let pool_guard = l2_pool.read();
                                            l2_slot = pool_guard.allocate();
                                            drop(pool_guard);
                                        }
                                    }
                                    
                                    if let Some(ref slot) = l2_slot {
                                        state_clone.l2_priority.insert(
                                            slot.slot,
                                            block_id,
                                            is_sequential,
                                        );
                                        stats.l2_block_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    }
                                }

                                if let Some(slot) = l2_slot {
                                    let journal_seq = match state_clone.journal.append(
                                        block_id,
                                        desc.volume_id,
                                        desc.offset,
                                        slot.slot,
                                        &data,
                                    ) {
                                        Ok(seq) => seq,
                                        Err(e) => {
                                            error!("Failed to write journal entry for block 0x{:016X}: {:?}", block_id, e);
                                            continue;
                                        }
                                    };

                                    let mut dirty_map = state_clone.dirty_blocks.lock();
                                    if let Some(old) = dirty_map.insert(
                                        block_id,
                                        crate::flush_thread::DirtyBlock {
                                            block_id,
                                            volume_id: desc.volume_id,
                                            offset: desc.offset,
                                            data: data.clone(),
                                            journal_sequence: journal_seq,
                                            slot_id: slot.slot,
                                        },
                                    ) {
                                        if let Err(e) = state_clone.journal.commit(old.journal_sequence) {
                                            warn!("Failed to commit superseded journal entry {}: {:?}", old.journal_sequence, e);
                                        }
                                    }
                                    state_clone.shared_ring.read().set_dirty_count(dirty_map.len() as u32);
                                    drop(dirty_map);

                                    l2_persistent = Some(slot);
                                }
                            } else {
                                // For reads or write-through:
                                let should_admit_l2 = !is_sequential
                                    || state_clone.config.read().cache.l2.cache_sequential;
                                if should_admit_l2 && l2_pool.read().is_healthy() {
                                    let pool_guard = l2_pool.read();
                                    if let Some(slot) = pool_guard.allocate() {
                                        drop(pool_guard);
                                        state_clone.pending_l2_writes.insert(block_id);
                                        let _ = state_clone.l2_writer_tx.send(crate::l2_writer::L2WriteJob {
                                            block_id,
                                            volume_id: desc.volume_id,
                                            offset: desc.offset,
                                            file_object: desc.file_object as usize,
                                            slot: slot.clone(),
                                            data: data.clone(),
                                            is_sequential,
                                        });
                                        l2_persistent = Some(slot);
                                    } else {
                                        drop(pool_guard);
                                        // Evict worst block using Smart Priority
                                        let evicted_entry = state_clone.l2_priority.evict_worst();
                                        if let Some((evicted_slot_id, evicted_block_id)) = evicted_entry {
                                            info!("L2 full: PriorityHeap evicting block 0x{:016X} from slot {}", evicted_block_id, evicted_slot_id);
                                            let evicted_l2_slot = L2Slot {
                                                backend: 0,
                                                slot: evicted_slot_id,
                                            };
                                            l2_pool.read().free(&evicted_l2_slot);
                                            stats.l2_block_count.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);

                                            if let Some(evicted_data) = arc_cache.remove(&evicted_block_id) {
                                                match evicted_data.slot {
                                                    SlotLocation::Pool(slot) => {
                                                        pool.free(slot);
                                                        stats.l1_block_count.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                                                        if let Some(meta) = state_clone.block_metadata_map.get(&evicted_block_id) {
                                                            state_clone.shared_ring.read().invalidate_l1_cache(
                                                                meta.volume_id,
                                                                meta.offset,
                                                                meta.file_object as usize,
                                                            );
                                                        }
                                                    }
                                                    SlotLocation::Ssd(_) => {}
                                                }
                                            }

                                            if let Some(meta) = state_clone.block_metadata_map.get(&evicted_block_id) {
                                                state_clone.shared_ring.read().insert_l2_cache(
                                                    meta.volume_id,
                                                    meta.offset,
                                                    0,
                                                    meta.file_object as usize,
                                                    evicted_slot_id,
                                                    false,
                                                );
                                            }
                                            let pool_guard = l2_pool.read();
                                            if let Some(slot) = pool_guard.allocate() {
                                                drop(pool_guard);
                                                state_clone.pending_l2_writes.insert(block_id);
                                                let _ = state_clone.l2_writer_tx.send(crate::l2_writer::L2WriteJob {
                                                    block_id,
                                                    volume_id: desc.volume_id,
                                                    offset: desc.offset,
                                                    file_object: desc.file_object as usize,
                                                    slot: slot.clone(),
                                                    data: data.clone(),
                                                    is_sequential,
                                                });
                                                l2_persistent = Some(slot);
                                            }
                                        }
                                    }
                                }
                            }

                            // We skip TinyLFU admission check for L1 because we want all blocks
                            // to at least hit L1, and then ARC handles L1 eviction.
                            // Wait, TinyLFU was previously protecting L1 from sequential scans.
                            // We will keep TinyLFU for L1, but L2 is 100% written.
                            if !tinylfu_clone.should_admit(block_id) {
                                tinylfu_clone
                                    .rejected
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            } else {
                                tinylfu_clone
                                    .admitted
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

                                let block_info = if let Some(slot) = pool.allocate() {
                                    let _ = pool.write(slot, &data);
                                    stats
                                        .l1_block_count
                                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    CachedBlockData {
                                        size: data.len(),
                                        slot: SlotLocation::Pool(slot),
                                        l2_persistent,
                                    }
                                } else if l2_persistent.is_some() {
                                    CachedBlockData {
                                        size: data.len(),
                                        slot: SlotLocation::Ssd(
                                            l2_persistent.as_ref().unwrap().clone(),
                                        ),
                                        l2_persistent,
                                    }
                                } else {
                                    warn!("L1 full and L2 full/unhealthy, dropping block for offset={}", desc.offset);
                                    continue;
                                };

                                // Use T2 (hot-file priority) if this block is tracked as hot by ETW
                                // ETW uses simple (volume_id << 32) | offset keys
                                let hot_key = ((desc.volume_id as u64) << 32) | desc.offset;
                                let is_hot = hot_blocks_consumer.lock().contains(&hot_key);
                                let evicted = if is_hot {
                                    arc_cache.insert_t2(block_id, block_info)
                                } else {
                                    arc_cache.insert(block_id, block_info)
                                };

                                if let Some((_evicted_id, evicted_data)) = evicted {
                                    match evicted_data.slot {
                                        SlotLocation::Pool(slot) => {
                                            pool.free(slot);
                                            stats
                                                .l1_block_count
                                                .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                                            if let Some(meta) = state_clone.block_metadata_map.get(&_evicted_id) {
                                                state_clone.shared_ring.read().invalidate_l1_cache(
                                                    meta.volume_id,
                                                    meta.offset,
                                                    meta.file_object as usize,
                                                );
                                            }
                                        }
                                        SlotLocation::Ssd(ref slot) => {
                                            l2_pool.read().free(slot);
                                            stats
                                                .l2_block_count
                                                .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                                            
                                            if let Some(meta) = state_clone.block_metadata_map.get(&_evicted_id) {
                                                state_clone.shared_ring.read().insert_l2_cache(
                                                    meta.volume_id,
                                                    meta.offset,
                                                    0,
                                                    meta.file_object as usize,
                                                    slot.slot,
                                                    false,
                                                );
                                            }

                                            if let Some(ref l2_slot) = evicted_data.l2_persistent {
                                                if l2_slot.backend != slot.backend
                                                    || l2_slot.slot != slot.slot
                                                {
                                                    l2_pool.read().free(l2_slot);
                                                    if let Some(meta) = state_clone.block_metadata_map.get(&_evicted_id) {
                                                        state_clone.shared_ring.read().insert_l2_cache(
                                                            meta.volume_id,
                                                            meta.offset,
                                                            0,
                                                            meta.file_object as usize,
                                                            l2_slot.slot,
                                                            false,
                                                        );
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        // Epoch adaptive parameter tuner
                        if epoch_total_reads >= 1000 {
                            let hit_ratio = epoch_cache_hits as f64 / epoch_total_reads as f64;
                            let seq_ratio = epoch_seq_reads as f64 / epoch_total_reads as f64;

                            let penalty = if seq_ratio > 0.4 {
                                2 // Severe penalty for sequential reads
                            } else if seq_ratio > 0.2 {
                                5 // Standard penalty
                            } else {
                                10 // Light penalty (keep sequential data longer if rare)
                            };

                            let bonus = if hit_ratio > 0.8 {
                                20 // Shrink bonus to let other entries compete
                            } else if hit_ratio < 0.3 {
                                100 // Boost bonus to strongly lock rare repeat hits
                            } else {
                                50 // Standard bonus
                            };

                            current_age_interval = if hit_ratio < 0.2 {
                                2000 // age rapidly under chaotic workloads
                            } else if hit_ratio > 0.7 {
                                20000 // age slowly under stable workloads
                            } else {
                                10000 // standard aging
                            };

                            state_clone.l2_priority.set_sequential_penalty(penalty);
                            state_clone.l2_priority.set_hit_bonus(bonus);

                            tracing::info!(
                                "Tuner Epoch: hit_ratio={:.2}, seq_ratio={:.2} | parameters adjusted: seq_penalty={}, hit_bonus={}, age_interval={}",
                                hit_ratio, seq_ratio, penalty, bonus, current_age_interval
                            );

                            epoch_total_reads = 0;
                            epoch_cache_hits = 0;
                            epoch_seq_reads = 0;
                        }
                    }

                    // Sync driver-side counters and perf frequency
                    {
                        let ring_ref = state_clone.shared_ring.read();
                        let freq = ring_ref.get_perf_counter_freq();
                        let pt = state_clone.perf_tracker.lock();
                        if freq > 0 {
                            pt.set_perf_counter_freq(freq);
                        }
                        let (hits, reads, writes) = ring_ref.get_driver_counters();
                        pt.update_driver_counters(reads, writes, hits);
                    }

                    if batch_count > 0 && pop_count % 50 == 0 {
                        info!(
                            "Consumer: popped {} blocks (total: {})",
                            batch_count, pop_count
                        );
                    }

                    // Periodic cache index save (every 30s)
                    if last_index_save.elapsed() >= INDEX_SAVE_INTERVAL {
                        // Adjust TinyLFU admission threshold based on hit rate
                        let cache_stats = arc_cache.stats();
                        let hit_rate = cache_stats.hit_rate();
                        tinylfu_clone.adjust_threshold(hit_rate);

                        // Log TinyLFU admission stats
                        let (tl_accesses, tl_admitted, tl_rejected) = tinylfu_clone.stats();
                        info!(
                            "TinyLFU: threshold={}, accesses={}, admitted={}, rejected={}, ARC hit_rate={:.1}%",
                            tinylfu_clone.threshold(), tl_accesses, tl_admitted, tl_rejected, hit_rate * 100.0
                        );

                        // Purge stale ghost entries (TTL = 10000 ticks ≈ 10K accesses)
                        let purged = arc_cache.purge_ghosts(10_000);
                        if purged > 0 {
                            info!("Ghost TTL purge: {} stale entries removed", purged);
                        }

                        let entries = arc_cache.entries();
                        let mut persistence_entries: Vec<CachedEntry> = Vec::new();
                        let mut l2_slot_set = std::collections::HashSet::new();
                        // Save ALL L2 persistent slots from the eviction queue (includes TinyLFU-rejected blocks)
                        for &(ref block_id, ref l2_slot) in
                            state_clone.l2_eviction_queue.lock().iter()
                        {
                            let key = (l2_slot.backend, l2_slot.slot);
                            if l2_slot_set.insert(key) {
                                persistence_entries.push(CachedEntry {
                                    block_id: *block_id,
                                    backend_index: l2_slot.backend,
                                    slot_id: l2_slot.slot,
                                    in_t2: false,
                                });
                            }
                        }
                        for (block_id, cached_data) in &entries {
                            // Also save L2 persistent slots from ARC entries not already tracked
                            if let Some(ref l2_slot) = cached_data.l2_persistent {
                                let key = (l2_slot.backend, l2_slot.slot);
                                if l2_slot_set.insert(key) {
                                    persistence_entries.push(CachedEntry {
                                        block_id: *block_id,
                                        backend_index: l2_slot.backend,
                                        slot_id: l2_slot.slot,
                                        in_t2: false,
                                    });
                                }
                            }
                            if let SlotLocation::Ssd(ref l2_slot) = cached_data.slot {
                                let key = (l2_slot.backend, l2_slot.slot);
                                if l2_slot_set.insert(key) {
                                    persistence_entries.push(CachedEntry {
                                        block_id: *block_id,
                                        backend_index: l2_slot.backend,
                                        slot_id: l2_slot.slot,
                                        in_t2: false,
                                    });
                                }
                            }
                        }
                        if !persistence_entries.is_empty() {
                            let l2_path = state_clone.config.read().cache.l2.path.clone();
                            let idx_path = l2_path.with_file_name("cache_index.bin");
                            if let Err(e) = persistence::save_cache_index(
                                &idx_path,
                                state_clone.cache_generation,
                                block_size as u32,
                                &persistence_entries,
                            ) {
                                warn!("Failed to periodic-save cache index: {:?}", e);
                            } else {
                                info!(
                                    "Periodic cache index saved: {} entries",
                                    persistence_entries.len()
                                );
                            }
                        }
                        last_index_save = std::time::Instant::now();
                    }

                    if wait_res != WAIT_OBJECT_0 {
                        wait_timeout_count += 1;
                        if wait_timeout_count % 60 == 0 {
                            info!(
                                "Consumer: {} timeouts, {} total pops, waiting...",
                                wait_timeout_count, pop_count
                            );
                        }
                    }
                }
            }

            info!(
                "Shared Memory consumer thread stopped. Total pops: {}",
                pop_count
            );
        });

        // 5b. Start flush thread for write-back mode
        let (flush_thread_handle, flush_running) = {
            let conf = state.config.read();
            let flush_interval_ms = conf.cache.flush_interval_ms;
            let flush_interval_max_ms = conf.cache.flush_interval_max_ms;
            let flush_dirty_threshold = conf.cache.flush_dirty_threshold;
            let max_dirty_blocks = conf.cache.max_dirty_blocks;
            let flush_block_size = conf.cache.block_size_kb as usize * 1024;
            let flush_thread = crate::flush_thread::FlushThread::new(
                dirty_blocks.clone(),
                journal.clone(),
                flush_interval_ms,
                flush_interval_max_ms,
                flush_dirty_threshold,
                max_dirty_blocks,
                state.l2_pool.clone(),
                flush_block_size,
            );
            let running = flush_thread.running_handle();
            (Some(flush_thread.start()), running)
        };

        // 6. Start ETW Monitoring

        let block_size_bytes_etw = state.config.read().cache.block_size_kb as u64 * 1024;
        let heatmap = Arc::new(Mutex::new(Heatmap::new(block_size_bytes_etw)));
        let heatmap_clone = heatmap.clone();
        let hot_blocks_clone = hot_blocks.clone();
        let block_size_u64 = block_size_bytes_etw;
        let state_clone_etw = state.clone();
        let prefetch_engine_clone = state.prefetch_engine.clone();

        let etw_monitor = match EtwMonitor::start(move |event| {
            let hm = heatmap_clone.lock().unwrap();
            let mut pf = prefetch_engine_clone.lock();
            if event.is_write {
                hm.record_write(&event.file_name, event.offset, event.size);
            } else {
                hm.record_read(&event.file_name, event.offset, event.size);
                // Track hot-file blocks for T2 priority boost
                if hm.is_file_hot(&event.file_name) {
                    let block_offset = event.offset & !(block_size_u64 - 1);
                    let block_id = ((event.volume_id as u64) << 32) | block_offset;
                    hot_blocks_clone.lock().insert(block_id);
                }
                if let Some(request) = pf.record_read(&event.file_name, event.offset) {
                    // Look up the actual file_object for this volume+offset range
                    let block_size = request.block_size;
                    let first_offset = request.start_offset;
                    let lookup_key =
                        ((event.volume_id as u64) << 32) | (first_offset & !(block_size - 1));
                    let file_object = state_clone_etw
                        .file_object_map
                        .get(&lookup_key)
                        .map(|v| *v.value())
                        .unwrap_or(0);

                    for block_offset in request.block_offsets() {
                        let block_id = make_block_id(event.volume_id, block_offset, file_object);
                        let job = crate::prefetch::PrefetchJob {
                            block_id,
                            volume_id: event.volume_id,
                            offset: block_offset,
                            file_object,
                        };
                        let _ = state_clone_etw.prefetch_sender.try_send(job);
                    }
                }
            }
        }) {
            Ok(m) => {
                info!("ETW Monitor started successfully.");

                Some(m)
            }

            Err(e) => {
                warn!("Could not start ETW Monitor: {:?}", e);

                None
            }
        };

        Ok(Self {
            state,

            driver_port,

            etw_monitor,

            ipc_server,

            running,

            flush_thread: flush_thread_handle,

            flush_running,

            prefetch_threads: prefetch_handles,

            l2_writer_tx: Some(l2_writer_tx),

            l2_writer_thread: Some(l2_writer_handle),
        })
    }

    pub fn shutdown_flag(&self) -> Arc<std::sync::atomic::AtomicBool> {
        self.state.shutdown_requested.clone()
    }

    pub async fn shutdown(&mut self) -> Result<()> {
        info!("Shutting down Nova Cache orchestrator...");

        // Phase 3.1: Save L1 cache index to disk before shutdown
        {
            let entries = self.state.arc_cache.read().entries();
            let block_size = self.state.config.read().cache.block_size_kb as u32 * 1024;
            let mut persistence_entries: Vec<CachedEntry> = Vec::new();
            for (block_id, cached_data) in &entries {
                if let SlotLocation::Ssd(ref l2_slot) = cached_data.slot {
                    persistence_entries.push(CachedEntry {
                        block_id: *block_id,
                        backend_index: l2_slot.backend,
                        slot_id: l2_slot.slot,
                        in_t2: false,
                    });
                }
            }

            if !persistence_entries.is_empty() {
                let l2_path = self.state.config.read().cache.l2.path.clone();
                let index_path = l2_path.with_file_name("cache_index.bin");
                match persistence::save_cache_index(&index_path, self.state.cache_generation, block_size, &persistence_entries) {
                    Ok(()) => {
                        info!(
                            "Saved {} L2 entries to cache index",
                            persistence_entries.len()
                        );
                    }
                    Err(e) => {
                        warn!("Failed to save cache index: {:?}", e);
                    }
                }
            } else {
                info!("No L2 entries to persist");
            }
        }

        if let Some(mut ipc) = self.ipc_server.take() {
            info!("Stopping IPC server...");

            ipc.stop().await?;
        }

        if let Some(mut etw) = self.etw_monitor.take() {
            info!("Stopping ETW Monitor...");

            etw.stop();
        }

        // Stop the Shared Memory consumer thread
        self.running
            .store(false, std::sync::atomic::Ordering::Relaxed);

        // Stop flush thread and drain remaining dirty blocks
        self.flush_running
            .store(false, std::sync::atomic::Ordering::Relaxed);
        if let Some(handle) = self.flush_thread.take() {
            let _ = handle.join();
        }
        for handle in self.prefetch_threads.drain(..) {
            let _ = handle.join();
        }

        // Drain and stop L2 writer
        drop(self.l2_writer_tx.take());
        if let Some(handle) = self.l2_writer_thread.take() {
            let _ = handle.join();
        }

        {
            let remaining = self.state.dirty_blocks.lock().len();
            if remaining > 0 {
                warn!(
                    "Shutdown: {} dirty blocks remaining, flushing to L2...",
                    remaining
                );
                let batch: Vec<crate::flush_thread::DirtyBlock> = {
                    let mut map = self.state.dirty_blocks.lock();
                    map.drain().map(|(_, b)| b).collect()
                };
                let total = batch.len();
                let (flushed, failed) = crate::flush_thread::flush_batch_to_disk(&self.state.l2_pool, &batch);
                if !flushed.is_empty() {
                    if let Err(e) = self.state.journal.commit_batch(&flushed) {
                        warn!(
                            "Shutdown: failed to commit {} journal entries: {:?}",
                            flushed.len(),
                            e
                        );
                    }
                }
                if !failed.is_empty() {
                    warn!("Shutdown: {} blocks failed to flush", failed.len());
                }
                info!(
                    "Shutdown: flushed {} blocks to L2 ({} failed).",
                    flushed.len(),
                    failed.len()
                );
            }
        }

        // Final L2 mmap flush to ensure dirty pages are on disk
        info!("Shutdown: flushing L2 memory-mapped cache to disk...");
        self.state.l2_pool.read().flush();
        info!("Shutdown: L2 cache flush complete.");

        // Final cache index save on shutdown
        {
            let entries = self.state.arc_cache.read().entries();
            let mut persistence_entries: Vec<CachedEntry> = Vec::new();
            let mut l2_slot_set = std::collections::HashSet::new();
            for &(ref block_id, ref l2_slot) in self.state.l2_eviction_queue.lock().iter() {
                let key = (l2_slot.backend, l2_slot.slot);
                if l2_slot_set.insert(key) {
                    persistence_entries.push(CachedEntry {
                        block_id: *block_id,
                        backend_index: l2_slot.backend,
                        slot_id: l2_slot.slot,
                        in_t2: false,
                    });
                }
            }
            for (block_id, cached_data) in &entries {
                if let Some(ref l2_slot) = cached_data.l2_persistent {
                    let key = (l2_slot.backend, l2_slot.slot);
                    if l2_slot_set.insert(key) {
                        persistence_entries.push(CachedEntry {
                            block_id: *block_id,
                            backend_index: l2_slot.backend,
                            slot_id: l2_slot.slot,
                            in_t2: false,
                        });
                    }
                }
                if let SlotLocation::Ssd(ref l2_slot) = cached_data.slot {
                    let key = (l2_slot.backend, l2_slot.slot);
                    if l2_slot_set.insert(key) {
                        persistence_entries.push(CachedEntry {
                            block_id: *block_id,
                            backend_index: l2_slot.backend,
                            slot_id: l2_slot.slot,
                            in_t2: false,
                        });
                    }
                }
            }
            if !persistence_entries.is_empty() {
                let conf = self.state.config.read();
                let l2_path = conf.cache.l2.path.clone();
                let block_size = conf.cache.block_size_kb as u32 * 1024;
                let idx_path = l2_path.with_file_name("cache_index.bin");
                if let Err(e) =
                    persistence::save_cache_index(&idx_path, self.state.cache_generation, block_size, &persistence_entries)
                {
                    warn!("Failed to save cache index on shutdown: {:?}", e);
                } else {
                    info!(
                        "Shutdown: cache index saved ({} L2 entries)",
                        persistence_entries.len()
                    );
                }
            }
        }

        // Close the driver port BEFORE stopping the driver

        info!("Closing driver port connection...");

        self.driver_port = None;

        // Stop and unregister the driver service

        info!("Stopping and unregistering Novacache driver...");

        scm::stop_driver_service("Novacache");

        scm::delete_driver_service("Novacache");

        info!("Nova Cache orchestrator shutdown complete.");

        Ok(())
    }
}
