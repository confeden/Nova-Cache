use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::windows::named_pipe::NamedPipeServer;
use tracing::{error, info, warn};

use windows::core::PCWSTR;
use windows::Win32::Foundation::BOOL;
use windows::Win32::Security::{
    InitializeSecurityDescriptor, SetSecurityDescriptorDacl, PSECURITY_DESCRIPTOR,
    SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR,
};
use windows::Win32::System::Pipes::{
    CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
};

use crate::orchestrator::ServiceStateShared;
use nova_cache_core::pool::MemoryPool;

const PIPE_NAME: &str = r"\\.\pipe\NovaCacheIpc";

fn create_pipe_with_dacl() -> Result<NamedPipeServer> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    let pipe_name_wide: Vec<u16> = OsStr::new(PIPE_NAME)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let mut sd: SECURITY_DESCRIPTOR = unsafe { std::mem::zeroed() };
    unsafe {
        let psd = PSECURITY_DESCRIPTOR(&mut sd as *mut _ as *mut _);
        InitializeSecurityDescriptor(psd, 1)?;
        SetSecurityDescriptorDacl(psd, BOOL::from(true), None, BOOL::from(false))?;
    }

    let mut sa: SECURITY_ATTRIBUTES = unsafe { std::mem::zeroed() };
    sa.nLength = std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32;
    sa.lpSecurityDescriptor = &mut sd as *mut _ as *mut _;

    // PIPE_ACCESS_DUPLEX (0x3) | FILE_FLAG_OVERLAPPED (0x40000000)
    let open_mode = windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES(0x40000003);

    let handle = unsafe {
        CreateNamedPipeW(
            PCWSTR(pipe_name_wide.as_ptr()),
            open_mode,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            1,
            65536,
            65536,
            0,
            Some(&sa),
        )
    };

    if handle.is_invalid() {
        return Err(std::io::Error::last_os_error().into());
    }

    let server = unsafe { NamedPipeServer::from_raw_handle(handle.0 as *mut _) }?;
    Ok(server)
}

pub struct IpcServer {
    #[allow(dead_code)]
    state: Arc<ServiceStateShared>,
    task_handle: Option<tokio::task::JoinHandle<()>>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IpcRequest {
    Ping,
    GetStats,
    GetConfig,
    SetConfig {
        l1_size_mb: Option<u64>,
        l2_size_gb: Option<u64>,
    },
    SetL2Path {
        path: String,
    },
    SetL2Backends {
        paths: Vec<String>,
    },
    ApplyL2Changes {
        l2_size_gb: Option<u64>,
    },
    GetMigrationProgress,
    GetL2Backends,
    GetVolumes,
    AddVolume {
        volume: String,
        enabled: Option<bool>,
    },
    RemoveVolume {
        volume: String,
    },
    SetVolumeEnabled {
        volume: String,
        enabled: bool,
    },
    SetWriteBack {
        enabled: bool,
    },
    SetFlushInterval {
        interval_ms: u64,
    },
    GetFlushStatus,
    FlushNow,
    Shutdown,
}

#[derive(Debug, Serialize)]
pub struct IpcResponse {
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl IpcResponse {
    fn ok(data: serde_json::Value) -> Self {
        Self {
            status: "ok".into(),
            data: Some(data),
            error: None,
        }
    }

    fn error<E: Into<String>>(msg: E) -> Self {
        Self {
            status: "error".into(),
            data: None,
            error: Some(msg.into()),
        }
    }
}

impl IpcServer {
    pub fn start(state: Arc<ServiceStateShared>) -> Self {
        let state_clone = state.clone();
        let task_handle = tokio::spawn(async move {
            if let Err(e) = Self::run_server(state_clone).await {
                error!("IPC Server stopped with error: {:?}", e);
            }
        });

        Self {
            state,
            task_handle: Some(task_handle),
        }
    }

    pub async fn stop(&mut self) -> Result<()> {
        if let Some(handle) = self.task_handle.take() {
            handle.abort();
            let _ = handle.await;
        }
        Ok(())
    }

    async fn run_server(state: Arc<ServiceStateShared>) -> Result<()> {
        info!("IPC Named Pipe Server starting...");

        loop {
            let server = match create_pipe_with_dacl() {
                Ok(s) => s,
                Err(e) => {
                    error!("IPC pipe create failed ({}), retrying in 2s...", e);
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    continue;
                }
            };

            info!("IPC pipe created, waiting for client...");

            // Wait for client to connect
            if let Err(e) = server.connect().await {
                error!("IPC pipe connect failed ({}), retrying...", e);
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }

            let state_clone = state.clone();
            // Handle client synchronously — don't create new pipe until this client disconnects
            if let Err(e) = Self::handle_client(server, state_clone).await {
                error!("Error handling IPC client: {:?}", e);
            }
            // Client disconnected — loop back to create new pipe
        }
    }

    async fn handle_client(server: NamedPipeServer, state: Arc<ServiceStateShared>) -> Result<()> {
        let (reader, mut writer) = tokio::io::split(server);
        let mut reader = BufReader::new(reader);
        let mut line = String::new();

        loop {
            line.clear();
            let len = reader.read_line(&mut line).await?;
            if len == 0 {
                break; // Connection closed
            }

            let req: IpcRequest = match serde_json::from_str(&line) {
                Ok(r) => r,
                Err(e) => {
                    let resp = IpcResponse::error(format!("Invalid request: {:?}", e));
                    let resp_str = serde_json::to_string(&resp)? + "\n";
                    writer.write_all(resp_str.as_bytes()).await?;
                    continue;
                }
            };

            let resp = Self::dispatch(req, &state).await;
            let resp_str = serde_json::to_string(&resp)? + "\n";
            writer.write_all(resp_str.as_bytes()).await?;
        }

        Ok(())
    }

    fn compute_volume_bitmap(conf: &nova_cache_core::config::NovaCacheConfig) -> u32 {
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
            }
        }
        bitmap
    }

    async fn dispatch(req: IpcRequest, state: &Arc<ServiceStateShared>) -> IpcResponse {
        match req {
            IpcRequest::Ping => IpcResponse::ok(json!({ "message": "pong" })),
            IpcRequest::GetStats => {
                let core_stats = state.arc_cache.read().stats();
                let pt = state.perf_tracker.lock();
                let snapshot = pt.get_snapshot();
                drop(pt);
                let conf = state.config.read();
                let block_size_bytes = conf.cache.block_size_kb as u64 * 1024;
                let l1_total_blocks = if block_size_bytes > 0 {
                    (conf.cache.l1_size_mb as u64 * 1024 * 1024) / block_size_bytes
                } else {
                    0
                };
                let l2_total_blocks = if block_size_bytes > 0 {
                    std::fs::metadata(&conf.cache.l2.path)
                        .map(|m| m.len() / block_size_bytes)
                        .unwrap_or(0)
                } else {
                    0
                };
                drop(conf);
                IpcResponse::ok(json!({
                    "hits_t1": core_stats.hits_t1,
                    "hits_t2": core_stats.hits_t2,
                    "misses": core_stats.misses,
                    "ghost_hits_b1": core_stats.ghost_hits_b1,
                    "ghost_hits_b2": core_stats.ghost_hits_b2,
                    "evictions": core_stats.evictions,
                    "perf_multiplier": format!("{:.2}", snapshot.perf_multiplier),
                    "hdd_read_latency_us": format!("{}", snapshot.hdd_read_latency_us.round() as u64),
                    "hdd_write_latency_us": format!("{}", snapshot.hdd_write_latency_us.round() as u64),
                    "l1_read_latency_us": format!("{}", snapshot.l1_read_latency_us.round() as u64),
                    "driver_cache_hits": snapshot.driver_cache_hits,
                    "driver_total_reads": snapshot.driver_total_reads,
                    "driver_total_writes": snapshot.driver_total_writes,
                    "l2_block_count": state.stats.l2_block_count.load(std::sync::atomic::Ordering::Relaxed),
                    "l1_block_count": state.stats.l1_block_count.load(std::sync::atomic::Ordering::Relaxed),
                    "l1_total_blocks": l1_total_blocks,
                    "l2_total_blocks": l2_total_blocks,
                }))
            }
            IpcRequest::GetConfig => {
                let conf = state.config.read();
                IpcResponse::ok(json!({
                    "l1_size_mb": conf.cache.l1_size_mb,
                    "l2_size_gb": conf.cache.l2.size_gb,
                    "block_size_kb": conf.cache.block_size_kb,
                    "l2_path": conf.cache.l2.path.to_string_lossy(),
                }))
            }
            IpcRequest::SetConfig {
                l1_size_mb,
                l2_size_gb,
            } => {
                let mut conf = state.config.write();
                let mut l1_changed = false;
                let mut l2_changed = false;
                if let Some(l1) = l1_size_mb {
                    if conf.cache.l1_size_mb != l1 as u32 {
                        conf.cache.l1_size_mb = l1 as u32;
                        l1_changed = true;
                    }
                }
                if let Some(l2) = l2_size_gb {
                    if conf.cache.l2.size_gb != l2 as u32 {
                        conf.cache.l2.size_gb = l2 as u32;
                        l2_changed = true;
                    }
                }

                // Save config to disk
                let config_path = std::env::current_dir()
                    .map(|d| d.join("config").join("nova_cache.toml"))
                    .unwrap_or_default();
                if config_path.exists() {
                    if let Err(e) = conf.save(&config_path) {
                        error!("Failed to save config: {:?}", e);
                    } else {
                        info!("Config saved to {}", config_path.display());
                    }
                }

                let mut messages = Vec::new();

                if l1_changed {
                    let new_l1 = conf.cache.l1_size_mb;
                    let block_size = conf.cache.block_size_kb as usize * 1024;
                    let new_capacity =
                        (new_l1 as u64 * 1024 * 1024) / (conf.cache.block_size_kb as u64 * 1024);
                    let new_num_slots = (new_l1 as usize * 1024 * 1024) / block_size;

                    drop(conf);

                    // Resize ARC cache
                    state.arc_cache.read().resize(new_capacity as usize);
                    info!("ARC cache resized to {} blocks", new_capacity);

                    // Replace memory pool
                    let new_pool = Arc::new(MemoryPool::new(new_num_slots, block_size));
                    *state.pool.write() = new_pool;
                    info!(
                        "Memory pool replaced: {} slots x {} bytes",
                        new_num_slots, block_size
                    );

                    messages.push(format!("L1 resized to {} MB", new_l1));
                } else {
                    drop(conf);
                }

                if l2_changed {
                    let l2_gb = state.config.read().cache.l2.size_gb;
                    info!("L2 size changed. Restart service to apply.");
                    messages.push(format!(
                        "L2 size set to {} GB. Restart service to apply.",
                        l2_gb
                    ));
                }

                IpcResponse::ok(json!({ "message": messages.join(". ") }))
            }
            IpcRequest::SetL2Path { path } => {
                let new_path = std::path::PathBuf::from(&path);
                {
                    let mut conf = state.config.write();
                    conf.cache.l2.path = new_path.clone();
                    let config_path = std::env::current_dir()
                        .map(|d| d.join("config").join("nova_cache.toml"))
                        .unwrap_or_default();
                    if config_path.exists() {
                        if let Err(e) = conf.save(&config_path) {
                            error!("Failed to save config: {:?}", e);
                        } else {
                            info!("Config saved to {}", config_path.display());
                        }
                    }
                }
                info!(
                    "L2 path changed to {}. Click Apply to activate.",
                    new_path.display()
                );
                IpcResponse::ok(json!({
                    "message": format!("L2 path set to {}. Click Apply to activate.", new_path.display()),
                }))
            }
            IpcRequest::SetL2Backends { paths } => {
                let new_paths: Vec<std::path::PathBuf> =
                    paths.iter().map(|p| std::path::PathBuf::from(p)).collect();
                {
                    let mut conf = state.config.write();
                    if let Some(first) = new_paths.first() {
                        conf.cache.l2.path = first.clone();
                    } else {
                        conf.cache.l2.path = std::path::PathBuf::new();
                    }
                    conf.cache.l2.backends = if new_paths.len() > 1 {
                        new_paths[1..].to_vec()
                    } else {
                        Vec::new()
                    };
                    let config_path = std::env::current_dir()
                        .map(|d| d.join("config").join("nova_cache.toml"))
                        .unwrap_or_default();
                    if config_path.exists() {
                        if let Err(e) = conf.save(&config_path) {
                            error!("Failed to save config: {:?}", e);
                        } else {
                            info!("Config saved to {}", config_path.display());
                        }
                    }
                }
                IpcResponse::ok(json!({
                    "message": format!("{} L2 backends configured. Click Apply to activate.", new_paths.len()),
                }))
            }
            IpcRequest::GetL2Backends => {
                let pool_guard = state.l2_pool.read();
                let backends: Vec<serde_json::Value> = pool_guard
                    .backend_info()
                    .iter()
                    .map(|(path, speed, free, total)| {
                        json!({
                            "path": path.to_string_lossy(),
                            "speed_mbps": speed,
                            "free_slots": free,
                            "total_slots": total,
                        })
                    })
                    .collect();
                IpcResponse::ok(json!({
                    "backends": backends,
                    "total_free": pool_guard.total_free_slots(),
                    "total_slots": pool_guard.total_slots(),
                    "healthy": pool_guard.is_healthy(),
                }))
            }
            IpcRequest::ApplyL2Changes { l2_size_gb } => {
                let progress = state.l2_migration_progress.clone();
                if progress.load(std::sync::atomic::Ordering::Acquire) >= 0 {
                    return IpcResponse::error(String::from("Migration already in progress"));
                }
                {
                    let mut conf = state.config.write();
                    if let Some(size) = l2_size_gb {
                        conf.cache.l2.size_gb = size as u32;
                    }
                    let config_path = std::env::current_dir()
                        .map(|d| d.join("config").join("nova_cache.toml"))
                        .unwrap_or_default();
                    if config_path.exists() {
                        if let Err(e) = conf.save(&config_path) {
                            error!("Failed to save config: {:?}", e);
                        }
                    }
                }

                let new_config = {
                    let conf = state.config.read();
                    let mut paths = Vec::new();
                    if !conf.cache.l2.path.as_os_str().is_empty() {
                        paths.push(conf.cache.l2.path.clone());
                    }
                    paths.extend(conf.cache.l2.backends.iter().cloned());
                    (paths, conf.cache.l2.size_gb, conf.cache.block_size_kb)
                };

                let l2_pool = state.l2_pool.clone();
                let arc_cache = state.arc_cache.clone();
                let stats = state.stats.clone();
                let progress_clone = progress.clone();

                std::thread::spawn(move || {
                    crate::migration::migrate_l2(
                        &l2_pool,
                        &arc_cache,
                        &stats,
                        &new_config.0,
                        new_config.1,
                        new_config.2,
                        &progress_clone,
                    );
                });

                IpcResponse::ok(json!({
                    "message": "L2 migration started",
                }))
            }
            IpcRequest::GetMigrationProgress => {
                let progress = state
                    .l2_migration_progress
                    .load(std::sync::atomic::Ordering::Acquire);
                IpcResponse::ok(json!({
                    "progress": progress,
                }))
            }
            IpcRequest::GetVolumes => {
                let conf = state.config.read();
                let volumes: Vec<serde_json::Value> = conf
                    .volumes
                    .iter()
                    .map(|v| {
                        json!({
                            "volume": v.volume,
                            "enabled": v.enabled,
                            "l1_override_mb": v.l1_size_mb_override,
                            "l2_override_gb": v.l2_size_gb_override,
                        })
                    })
                    .collect();
                IpcResponse::ok(json!({
                    "volumes": volumes,
                    "l1_size_mb": conf.cache.l1_size_mb,
                    "l2_size_gb": conf.cache.l2.size_gb,
                }))
            }
            IpcRequest::AddVolume { volume, enabled } => {
                let mut conf = state.config.write();
                let vol_upper = volume.to_uppercase();
                if conf
                    .volumes
                    .iter()
                    .any(|v| v.volume.to_uppercase() == vol_upper)
                {
                    return IpcResponse::error(format!(
                        "Volume {} is already configured",
                        vol_upper
                    ));
                }
                let new_vol = nova_cache_core::config::VolumeConfig {
                    volume: vol_upper.clone(),
                    enabled: enabled.unwrap_or(true),
                    l1_size_mb_override: None,
                    l2_size_gb_override: None,
                    block_size_kb_override: None,
                };
                conf.volumes.push(new_vol);
                let bitmap = Self::compute_volume_bitmap(&conf);
                drop(conf);

                // Update driver bitmap
                // Note: shared_ring is not in ServiceStateShared; the bitmap update
                // happens on next service restart. For now we just save to config.
                info!(
                    "Added volume {}. Bitmap will apply on next restart.",
                    vol_upper
                );
                IpcResponse::ok(json!({
                    "message": format!("Volume {} added. Restart service to apply.", vol_upper),
                    "volume_bitmap": bitmap,
                }))
            }
            IpcRequest::RemoveVolume { volume } => {
                let mut conf = state.config.write();
                let vol_upper = volume.to_uppercase();
                let before = conf.volumes.len();
                conf.volumes
                    .retain(|v| v.volume.to_uppercase() != vol_upper);
                if conf.volumes.len() == before {
                    return IpcResponse::error(format!("Volume {} not found", vol_upper));
                }
                let bitmap = Self::compute_volume_bitmap(&conf);
                drop(conf);
                info!(
                    "Removed volume {}. Bitmap will apply on next restart.",
                    vol_upper
                );
                IpcResponse::ok(json!({
                    "message": format!("Volume {} removed. Restart service to apply.", vol_upper),
                    "volume_bitmap": bitmap,
                }))
            }
            IpcRequest::SetVolumeEnabled { volume, enabled } => {
                let mut conf = state.config.write();
                let vol_upper = volume.to_uppercase();
                if let Some(vol) = conf
                    .volumes
                    .iter_mut()
                    .find(|v| v.volume.to_uppercase() == vol_upper)
                {
                    vol.enabled = enabled;
                    let bitmap = Self::compute_volume_bitmap(&conf);
                    drop(conf);
                    info!(
                        "Volume {} enabled={}. Bitmap will apply on next restart.",
                        vol_upper, enabled
                    );
                    IpcResponse::ok(json!({
                        "message": format!("Volume {} {}.", vol_upper, if enabled { "enabled" } else { "disabled" }),
                        "volume_bitmap": bitmap,
                    }))
                } else {
                    IpcResponse::error(format!("Volume {} not found", vol_upper))
                }
            }
            IpcRequest::Shutdown => {
                info!("Shutdown requested by GUI client");
                state
                    .shutdown_requested
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                IpcResponse::ok(json!({ "message": "Shutdown initiated" }))
            }
            IpcRequest::SetWriteBack { enabled } => {
                let ring = state.shared_ring.read();
                ring.set_write_back_enabled(enabled);
                drop(ring);

                info!(
                    "Write-back {} via IPC (driver-level, config no longer stores write policy)",
                    if enabled { "enabled" } else { "disabled" }
                );
                IpcResponse::ok(json!({
                    "message": format!("Write-back {} (driver-level)", if enabled { "enabled" } else { "disabled" }),
                    "write_back_enabled": enabled,
                }))
            }
            IpcRequest::SetFlushInterval { interval_ms } => {
                if interval_ms < 50 || interval_ms > 60000 {
                    return IpcResponse::error("interval_ms must be between 50 and 60000");
                }
                let mut conf = state.config.write();
                conf.cache.flush_interval_ms = interval_ms;
                let config_path = std::env::current_dir()
                    .map(|d| d.join("config").join("nova_cache.toml"))
                    .unwrap_or_default();
                if config_path.exists() {
                    if let Err(e) = conf.save(&config_path) {
                        error!("Failed to save config: {:?}", e);
                    }
                }
                drop(conf);

                info!(
                    "Flush interval set to {}ms. Restart service to apply.",
                    interval_ms
                );
                IpcResponse::ok(json!({
                    "message": format!("Flush interval set to {}ms. Restart service to apply.", interval_ms),
                    "flush_interval_ms": interval_ms,
                }))
            }
            IpcRequest::GetFlushStatus => {
                let dirty_count = state.dirty_blocks.lock().len();
                let journal_uncommitted = state.journal.uncommitted_count();
                let journal_size_kb = state.journal.file_size() / 1024;
                let ring = state.shared_ring.read();
                let write_back_enabled = ring.get_write_back_enabled();
                let driver_dirty_count = ring.get_dirty_count();
                drop(ring);
                IpcResponse::ok(json!({
                    "write_back_enabled": write_back_enabled,
                    "dirty_blocks": dirty_count,
                    "driver_dirty_count": driver_dirty_count,
                    "journal_uncommitted": journal_uncommitted,
                    "journal_size_kb": journal_size_kb,
                }))
            }
            IpcRequest::FlushNow => {
                let dirty_count = state.dirty_blocks.lock().len();
                if dirty_count == 0 {
                    info!("FlushNow: no dirty blocks to flush");
                    return IpcResponse::ok(json!({ "flushed": 0, "remaining": 0 }));
                }

                info!("FlushNow: flushing {} dirty blocks to L2...", dirty_count);
                let batch: Vec<crate::flush_thread::DirtyBlock> = {
                    let mut map = state.dirty_blocks.lock();
                    map.drain().map(|(_, b)| b).collect()
                };

                let total = batch.len();
                let (flushed_sequences, failed_ids) =
                    crate::flush_thread::flush_batch_to_disk(&state.l2_pool, &batch);

                if !flushed_sequences.is_empty() {
                    if let Err(e) = state.journal.commit_batch(&flushed_sequences) {
                        error!(
                            "FlushNow: failed to commit {} journal entries: {:?}",
                            flushed_sequences.len(),
                            e
                        );
                    }
                }

                if !failed_ids.is_empty() {
                    let mut map = state.dirty_blocks.lock();
                    for block in &batch {
                        if failed_ids.contains(&block.block_id) {
                            map.insert(block.block_id, block.clone());
                        }
                    }
                    warn!("FlushNow: {} blocks failed, re-queued", failed_ids.len());
                }

                let remaining = state.dirty_blocks.lock().len();
                info!(
                    "FlushNow: flushed {}, remaining {}",
                    total - failed_ids.len(),
                    remaining
                );
                IpcResponse::ok(json!({
                    "flushed": total - failed_ids.len(),
                    "remaining": remaining,
                }))
            }
        }
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        if let Some(handle) = self.task_handle.take() {
            handle.abort();
        }
    }
}
