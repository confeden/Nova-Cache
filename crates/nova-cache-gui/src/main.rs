use std::ffi::c_void;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use eframe::egui;
use serde::{Deserialize, Serialize};

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use windows::core::PCWSTR;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, GetDiskFreeSpaceExW, GetLogicalDrives, GetVolumeInformationW,
    FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;

const PIPE_NAME: &str = r"\\.\pipe\NovaCacheIpc";
const IOCTL_STORAGE_QUERY_PROPERTY: u32 = 0x2D1400;

#[derive(Debug, Clone)]
struct DriveInfo {
    letter: char,
    label: String,
    fs_type: String,
    total_gb: f64,
    free_gb: f64,
    drive_type: String,
}

fn query_bus_type(letter: char) -> String {
    let path = format!("\\\\.\\{}:", letter);
    let path_wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();

    unsafe {
        let handle = match CreateFileW(
            PCWSTR(path_wide.as_ptr()),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        ) {
            Ok(h) if !h.is_invalid() => h,
            _ => return "Unknown".into(),
        };

        let query: [u32; 2] = [0, 0];
        let mut descriptor = [0u8; 1024];
        let mut bytes_returned = 0u32;

        let ok = DeviceIoControl(
            handle,
            IOCTL_STORAGE_QUERY_PROPERTY,
            Some(query.as_ptr() as *const c_void),
            8,
            Some(descriptor.as_mut_ptr() as *mut c_void),
            descriptor.len() as u32,
            Some(&mut bytes_returned),
            None,
        );

        let _ = CloseHandle(handle);

        if ok.is_ok() && bytes_returned >= 32 {
            let bus_type = descriptor[28];
            match bus_type {
                0x11 => "NVMe SSD".into(),
                0x0B => "SATA SSD/HDD".into(),
                0x0A => "SAS".into(),
                0x02 => "SCSI".into(),
                _ => format!("BusType {}", bus_type),
            }
        } else {
            "Unknown".into()
        }
    }
}

fn get_drive_type_label(letter: char) -> String {
    let root = format!("{}:\\", letter);
    let root_wide: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();
    let drive_type =
        unsafe { windows::Win32::Storage::FileSystem::GetDriveTypeW(PCWSTR(root_wide.as_ptr())) };
    match drive_type {
        2 => "Removable".into(),
        4 => "Network".into(),
        5 => "CD/DVD".into(),
        6 => "RAM Disk".into(),
        3 => query_bus_type(letter),
        _ => "Unknown".into(),
    }
}

fn format_drive_type(dt: &str) -> (String, egui::Color32) {
    match dt {
        "NVMe SSD" => ("NVMe".into(), egui::Color32::from_rgb(80, 150, 80)),
        s if s.starts_with("SATA") => ("SATA".into(), egui::Color32::from_rgb(80, 120, 170)),
        "USB Flash" => ("USB".into(), egui::Color32::from_rgb(160, 120, 60)),
        "SCSI" => ("SCSI".into(), egui::Color32::from_rgb(140, 100, 180)),
        "SAS" => ("SAS".into(), egui::Color32::from_rgb(140, 100, 180)),
        "CD/DVD" => ("Optical".into(), egui::Color32::from_rgb(160, 160, 160)),
        _ => ("?".into(), egui::Color32::from_rgb(100, 100, 100)),
    }
}

fn enumerate_drives() -> Vec<DriveInfo> {
    let mut drives = Vec::new();
    unsafe {
        let mask = GetLogicalDrives();
        for i in 0..26u32 {
            if mask & (1 << i) != 0 {
                let letter = (b'A' + i as u8) as char;
                let root = format!("{}:\\", letter);
                let root_wide: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();

                let mut vol_name = [0u16; 256];
                let mut fs_name = [0u16; 256];
                let mut serial = 0u32;
                let mut max_comp = 0u32;
                let mut flags = 0u32;

                let vol_ok = GetVolumeInformationW(
                    PCWSTR(root_wide.as_ptr()),
                    Some(&mut vol_name),
                    Some(&mut serial),
                    Some(&mut max_comp),
                    Some(&mut flags),
                    Some(&mut fs_name),
                );

                let label = if vol_ok.is_ok() {
                    let len = vol_name.iter().position(|&c| c == 0).unwrap_or(256);
                    String::from_utf16_lossy(&vol_name[..len])
                } else {
                    String::new()
                };

                let fs_type = if vol_ok.is_ok() {
                    let len = fs_name.iter().position(|&c| c == 0).unwrap_or(256);
                    String::from_utf16_lossy(&fs_name[..len])
                } else {
                    String::new()
                };

                let mut free_bytes = 0u64;
                let mut total_bytes = 0u64;
                let mut total_free = 0u64;
                let _ = GetDiskFreeSpaceExW(
                    PCWSTR(root_wide.as_ptr()),
                    Some(&mut free_bytes),
                    Some(&mut total_bytes),
                    Some(&mut total_free),
                );

                let drive_type = get_drive_type_label(letter);

                drives.push(DriveInfo {
                    letter,
                    label,
                    fs_type,
                    total_gb: total_bytes as f64 / (1024.0 * 1024.0 * 1024.0),
                    free_gb: free_bytes as f64 / (1024.0 * 1024.0 * 1024.0),
                    drive_type,
                });
            }
        }
    }
    drives
}

fn format_gb(gb: f64) -> String {
    if gb >= 1024.0 {
        format!("{:.1} TB", gb / 1024.0)
    } else {
        format!("{:.0} GB", gb)
    }
}

#[derive(Debug, Serialize)]
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

#[derive(Debug, Deserialize)]
pub struct IpcResponse {
    pub status: String,
    pub data: Option<serde_json::Value>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct AppStats {
    pub hits_t1: u64,
    pub hits_t2: u64,
    pub misses: u64,
    pub ghost_hits_b1: u64,
    pub ghost_hits_b2: u64,
    pub evictions: u64,
    pub perf_multiplier: f64,
    pub hdd_read_latency_us: f64,
    pub hdd_write_latency_us: f64,
    pub l1_read_latency_us: f64,
    pub driver_cache_hits: u64,
    pub driver_total_reads: u64,
    pub driver_total_writes: u64,
    pub l2_block_count: u64,
    pub l1_block_count: u64,
    pub l1_total_blocks: u64,
    pub l2_total_blocks: u64,
}

#[derive(Debug, Clone, Default)]
pub struct AppConfig {
    pub l1_size_mb: u32,
    pub l2_size_gb: u32,
    pub block_size_kb: u32,
    pub l2_path: String,
    pub l2_backends: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct AppFlushStatus {
    pub write_back_enabled: bool,
    pub dirty_blocks: u64,
    pub driver_dirty_count: u64,
    pub journal_uncommitted: u64,
}

#[derive(Debug, Clone)]
pub struct VolumeInfo {
    pub volume: String,
    pub enabled: bool,
}

pub enum IpcCommand {
    SetConfig { l1: Option<u64>, l2: Option<u64> },
    SetL2Path { path: String },
    SetL2Backends { paths: Vec<String> },
    AddVolume { volume: String },
    RemoveVolume { volume: String },
    SetVolumeEnabled { volume: String, enabled: bool },
    SetWriteBack { enabled: bool },
    SetFlushInterval { interval_ms: u64 },
    FlushNow,
    Shutdown,
}

pub struct SharedAppState {
    pub connected: bool,
    pub stats: AppStats,
    pub config: AppConfig,
    pub volumes: Vec<VolumeInfo>,
    pub flush_status: AppFlushStatus,
    pub error_msg: Option<String>,
    pub status_msg: Option<(String, bool)>,
}

pub struct NovaCacheApp {
    shared_state: Arc<Mutex<SharedAppState>>,
    command_tx: Sender<IpcCommand>,
    l1_input: String,
    l2_input: String,
    status_msg: Option<(String, bool)>,
    system_drives: Vec<DriveInfo>,
    window_rect: (f32, f32, f32, f32),
    bench_rx: Option<Receiver<String>>,
    bench_output: Vec<String>,
    test_mode: TestMode,
    corrupted_drives: Vec<char>,
    needs_reboot: bool,
}

#[derive(PartialEq)]
enum TestMode {
    None,
    Performance,
    Benchmark,
}

impl Drop for NovaCacheApp {
    fn drop(&mut self) {
        save_gui_state(
            self.window_rect.0,
            self.window_rect.1,
            self.window_rect.2,
            self.window_rect.3,
        );
        let _ = self.command_tx.send(IpcCommand::FlushNow);
        thread::sleep(Duration::from_millis(100));
        let _ = self.command_tx.send(IpcCommand::Shutdown);
        thread::sleep(Duration::from_millis(50));
    }
}

impl NovaCacheApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        auto_launch_service: bool,
        window_rect: (f32, f32, f32, f32),
    ) -> Self {
        let mut style = (*cc.egui_ctx.style()).clone();
        style.visuals.dark_mode = true;
        style.visuals.panel_fill = egui::Color32::from_rgb(22, 26, 33);
        style.visuals.window_fill = egui::Color32::from_rgb(22, 26, 33);
        style.visuals.widgets.noninteractive.bg_fill = egui::Color32::from_rgb(22, 26, 33);
        style.visuals.widgets.noninteractive.fg_stroke.color =
            egui::Color32::from_rgb(160, 165, 175);
        style.visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(30, 36, 44);
        style.visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(45, 52, 64);
        style.visuals.widgets.active.bg_fill = egui::Color32::from_rgb(55, 65, 80);
        cc.egui_ctx.set_style(style);

        let shared_state = Arc::new(Mutex::new(SharedAppState {
            connected: false,
            stats: AppStats::default(),
            config: AppConfig::default(),
            volumes: Vec::new(),
            flush_status: AppFlushStatus::default(),
            error_msg: None,
            status_msg: None,
        }));

        let (command_tx, command_rx) = mpsc::channel();
        let state_clone = shared_state.clone();

        thread::spawn(move || {
            run_ipc_worker(state_clone, command_rx, auto_launch_service);
        });

        let system_drives = enumerate_drives();

        Self {
            shared_state,
            command_tx,
            l1_input: String::new(),
            l2_input: String::new(),
            status_msg: None,
            system_drives,
            window_rect,
            bench_rx: None,
            bench_output: Vec::new(),
            test_mode: TestMode::None,
            corrupted_drives: Vec::new(),
            needs_reboot: false,
        }
    }
}

fn run_ipc_worker(
    state: Arc<Mutex<SharedAppState>>,
    command_rx: Receiver<IpcCommand>,
    auto_launch_service: bool,
) {
    let service_launched = Arc::new(AtomicBool::new(!auto_launch_service));

    loop {
        let mut pipe = match OpenOptions::new().read(true).write(true).open(PIPE_NAME) {
            Ok(p) => p,
            Err(_) => {
                {
                    let mut lock = state.lock().unwrap_or_else(|e| e.into_inner());
                    lock.connected = false;
                    if !service_launched.load(Ordering::Relaxed) {
                        lock.error_msg = Some("Starting Nova Cache service...".into());
                    } else {
                        lock.error_msg =
                            Some("Could not connect to Nova Cache service. Retrying...".into());
                    }
                }

                if !service_launched.load(Ordering::Relaxed) {
                    if let Some(svc_path) = find_service_exe() {
                        tracing::info!("Attempting to launch service: {}", svc_path.display());
                        if launch_service_elevated(&svc_path) {
                            tracing::info!("Service launch requested. Waiting for it to start...");
                            service_launched.store(true, Ordering::Relaxed);
                        } else {
                            tracing::error!("Failed to launch service");
                            let mut lock = state.lock().unwrap_or_else(|e| e.into_inner());
                            lock.error_msg =
                                Some("Failed to start service. Run as administrator.".into());
                        }
                    } else {
                        let mut lock = state.lock().unwrap_or_else(|e| e.into_inner());
                        lock.error_msg = Some("Service executable not found.".into());
                    }
                }

                thread::sleep(Duration::from_millis(200));
                continue;
            }
        };

        let pipe_clone = match pipe.try_clone() {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("Failed to clone pipe handle: {:?}", e);
                thread::sleep(Duration::from_millis(200));
                continue;
            }
        };

        {
            let mut lock = state.lock().unwrap_or_else(|e| e.into_inner());
            lock.connected = true;
            lock.error_msg = None;
        }

        let mut reader = BufReader::new(pipe_clone);
        let mut response_line = String::new();

        if let Err(e) = query_config(&mut pipe, &mut reader, &mut response_line, &state) {
            tracing::error!("Failed to query initial config: {:?}", e);
            continue;
        }
        if let Err(e) = query_l2_backends(&mut pipe, &mut reader, &mut response_line, &state) {
            tracing::error!("Failed to query initial l2 backends: {:?}", e);
        }
        if let Err(e) = query_volumes(&mut pipe, &mut reader, &mut response_line, &state) {
            tracing::error!("Failed to query initial volumes: {:?}", e);
        }

        let mut last_query = Instant::now();

        loop {
            while let Ok(cmd) = command_rx.try_recv() {
                match cmd {
                    IpcCommand::SetConfig { l1, l2 } => {
                        let req = IpcRequest::SetConfig {
                            l1_size_mb: l1,
                            l2_size_gb: l2,
                        };
                        if let Err(e) = send_request(&mut pipe, &req) {
                            tracing::error!("Failed to send set config: {:?}", e);
                            break;
                        }
                        response_line.clear();
                        if let Err(e) = reader.read_line(&mut response_line) {
                            tracing::error!("Failed to read set config response: {:?}", e);
                            break;
                        }
                        handle_status_response(&response_line, &state);
                        let _ = query_config(&mut pipe, &mut reader, &mut response_line, &state);
                    }
                    IpcCommand::SetL2Path { path } => {
                        let req = IpcRequest::SetL2Path { path };
                        if let Err(e) = send_request(&mut pipe, &req) {
                            tracing::error!("Failed to send set l2 path: {:?}", e);
                            break;
                        }
                        response_line.clear();
                        if let Err(e) = reader.read_line(&mut response_line) {
                            tracing::error!("Failed to read set l2 path response: {:?}", e);
                            break;
                        }
                        handle_status_response(&response_line, &state);
                        let _ = query_config(&mut pipe, &mut reader, &mut response_line, &state);
                    }
                    IpcCommand::SetL2Backends { paths } => {
                        let paths_for_local = paths.clone();
                        let req = IpcRequest::SetL2Backends { paths };
                        if let Err(e) = send_request(&mut pipe, &req) {
                            tracing::error!("Failed to send set l2 backends: {:?}", e);
                            break;
                        }
                        response_line.clear();
                        if let Err(e) = reader.read_line(&mut response_line) {
                            tracing::error!("Failed to read set l2 backends response: {:?}", e);
                            break;
                        }
                        handle_status_response(&response_line, &state);
                        let _ = query_config(&mut pipe, &mut reader, &mut response_line, &state);
                        {
                            let mut lock = state.lock().unwrap_or_else(|e| e.into_inner());
                            lock.config.l2_backends = paths_for_local.clone();
                            lock.config.l2_path =
                                paths_for_local.first().cloned().unwrap_or_default();
                        }
                    }
                    IpcCommand::AddVolume { volume } => {
                        let req = IpcRequest::AddVolume {
                            volume,
                            enabled: Some(true),
                        };
                        if let Err(e) = send_request(&mut pipe, &req) {
                            tracing::error!("Failed to send add volume: {:?}", e);
                            break;
                        }
                        response_line.clear();
                        if let Err(e) = reader.read_line(&mut response_line) {
                            tracing::error!("Failed to read add volume response: {:?}", e);
                            break;
                        }
                        handle_status_response(&response_line, &state);
                        let _ = query_volumes(&mut pipe, &mut reader, &mut response_line, &state);
                    }
                    IpcCommand::RemoveVolume { volume } => {
                        let req = IpcRequest::RemoveVolume { volume };
                        if let Err(e) = send_request(&mut pipe, &req) {
                            tracing::error!("Failed to send remove volume: {:?}", e);
                            break;
                        }
                        response_line.clear();
                        if let Err(e) = reader.read_line(&mut response_line) {
                            tracing::error!("Failed to read remove volume response: {:?}", e);
                            break;
                        }
                        handle_status_response(&response_line, &state);
                        let _ = query_volumes(&mut pipe, &mut reader, &mut response_line, &state);
                    }
                    IpcCommand::SetVolumeEnabled { volume, enabled } => {
                        let req = IpcRequest::SetVolumeEnabled { volume, enabled };
                        if let Err(e) = send_request(&mut pipe, &req) {
                            tracing::error!("Failed to send set volume enabled: {:?}", e);
                            break;
                        }
                        response_line.clear();
                        if let Err(e) = reader.read_line(&mut response_line) {
                            tracing::error!("Failed to read set volume enabled response: {:?}", e);
                            break;
                        }
                        handle_status_response(&response_line, &state);
                        let _ = query_volumes(&mut pipe, &mut reader, &mut response_line, &state);
                    }
                    IpcCommand::SetWriteBack { enabled } => {
                        let req = IpcRequest::SetWriteBack { enabled };
                        if let Err(e) = send_request(&mut pipe, &req) {
                            tracing::error!("Failed to send set write-back: {:?}", e);
                            break;
                        }
                        response_line.clear();
                        if let Err(e) = reader.read_line(&mut response_line) {
                            tracing::error!("Failed to read set write-back response: {:?}", e);
                            break;
                        }
                        handle_status_response(&response_line, &state);
                    }
                    IpcCommand::SetFlushInterval { interval_ms } => {
                        let req = IpcRequest::SetFlushInterval { interval_ms };
                        if let Err(e) = send_request(&mut pipe, &req) {
                            tracing::error!("Failed to send set flush interval: {:?}", e);
                            break;
                        }
                        response_line.clear();
                        if let Err(e) = reader.read_line(&mut response_line) {
                            tracing::error!("Failed to read set flush interval response: {:?}", e);
                            break;
                        }
                        handle_status_response(&response_line, &state);
                    }
                    IpcCommand::FlushNow => {
                        tracing::info!("Sending FlushNow to service");
                        let req = IpcRequest::FlushNow;
                        let _ = send_request(&mut pipe, &req);
                        response_line.clear();
                        if let Ok(_) = reader.read_line(&mut response_line) {
                            tracing::info!("FlushNow response: {}", response_line.trim());
                        }
                    }
                    IpcCommand::Shutdown => {
                        tracing::info!("Sending shutdown command to service");
                        let _ = send_request(&mut pipe, &IpcRequest::Shutdown);
                        return;
                    }
                }
            }

            if last_query.elapsed() >= Duration::from_millis(500) {
                let req = IpcRequest::GetStats;
                if let Err(e) = send_request(&mut pipe, &req) {
                    tracing::error!("Failed to send get stats: {:?}", e);
                    break;
                }

                response_line.clear();
                match reader.read_line(&mut response_line) {
                    Ok(len) => {
                        if len == 0 {
                            break;
                        }
                        if let Ok(resp) = serde_json::from_str::<IpcResponse>(&response_line) {
                            if resp.status == "ok" {
                                if let Some(data) = resp.data {
                                    let h_t1 =
                                        serde_json::from_value::<u64>(data["hits_t1"].clone())
                                            .unwrap_or(0);
                                    let h_t2 =
                                        serde_json::from_value::<u64>(data["hits_t2"].clone())
                                            .unwrap_or(0);
                                    let miss =
                                        serde_json::from_value::<u64>(data["misses"].clone())
                                            .unwrap_or(0);
                                    let gh_b1 = serde_json::from_value::<u64>(
                                        data["ghost_hits_b1"].clone(),
                                    )
                                    .unwrap_or(0);
                                    let gh_b2 = serde_json::from_value::<u64>(
                                        data["ghost_hits_b2"].clone(),
                                    )
                                    .unwrap_or(0);
                                    let ev =
                                        serde_json::from_value::<u64>(data["evictions"].clone())
                                            .unwrap_or(0);
                                    let boost = data["perf_multiplier"]
                                        .as_str()
                                        .unwrap_or("1.00")
                                        .parse::<f64>()
                                        .unwrap_or(1.0);
                                    let hdd_r = data["hdd_read_latency_us"]
                                        .as_str()
                                        .unwrap_or("0")
                                        .parse::<f64>()
                                        .unwrap_or(0.0);
                                    let hdd_w = data["hdd_write_latency_us"]
                                        .as_str()
                                        .unwrap_or("0")
                                        .parse::<f64>()
                                        .unwrap_or(0.0);
                                    let l1_r = data["l1_read_latency_us"]
                                        .as_str()
                                        .unwrap_or("0")
                                        .parse::<f64>()
                                        .unwrap_or(0.0);
                                    let dr_hits = serde_json::from_value::<u64>(
                                        data["driver_cache_hits"].clone(),
                                    )
                                    .unwrap_or(0);
                                    let dr_reads = serde_json::from_value::<u64>(
                                        data["driver_total_reads"].clone(),
                                    )
                                    .unwrap_or(0);
                                    let dr_writes = serde_json::from_value::<u64>(
                                        data["driver_total_writes"].clone(),
                                    )
                                    .unwrap_or(0);
                                    let l2_blocks = serde_json::from_value::<u64>(
                                        data["l2_block_count"].clone(),
                                    )
                                    .unwrap_or(0);
                                    let l1_blocks = serde_json::from_value::<u64>(
                                        data["l1_block_count"].clone(),
                                    )
                                    .unwrap_or(0);
                                    let l1_total = serde_json::from_value::<u64>(
                                        data["l1_total_blocks"].clone(),
                                    )
                                    .unwrap_or(0);
                                    let l2_total = serde_json::from_value::<u64>(
                                        data["l2_total_blocks"].clone(),
                                    )
                                    .unwrap_or(0);
                                    let mut lock = state.lock().unwrap_or_else(|e| e.into_inner());
                                    lock.stats = AppStats {
                                        hits_t1: h_t1,
                                        hits_t2: h_t2,
                                        misses: miss,
                                        ghost_hits_b1: gh_b1,
                                        ghost_hits_b2: gh_b2,
                                        evictions: ev,
                                        perf_multiplier: boost,
                                        hdd_read_latency_us: hdd_r,
                                        hdd_write_latency_us: hdd_w,
                                        l1_read_latency_us: l1_r,
                                        driver_cache_hits: dr_hits,
                                        driver_total_reads: dr_reads,
                                        driver_total_writes: dr_writes,
                                        l2_block_count: l2_blocks,
                                        l1_block_count: l1_blocks,
                                        l1_total_blocks: l1_total,
                                        l2_total_blocks: l2_total,
                                    };
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("Failed to read stats response: {:?}", e);
                        break;
                    }
                }
                last_query = Instant::now();
            }

            // Poll flush status every 2 seconds
            thread_local! {
                static LAST_FLUSH_QUERY: std::cell::Cell<Option<std::time::Instant>> = std::cell::Cell::new(None);
            }
            let now = std::time::Instant::now();
            let should_query_flush = LAST_FLUSH_QUERY.with(|cell| match cell.get() {
                None => {
                    cell.set(Some(now));
                    true
                }
                Some(t) if t.elapsed() >= Duration::from_secs(2) => {
                    cell.set(Some(now));
                    true
                }
                _ => false,
            });
            if should_query_flush {
                let req = IpcRequest::GetFlushStatus;
                if let Ok(()) = send_request(&mut pipe, &req) {
                    response_line.clear();
                    if let Ok(_) = reader.read_line(&mut response_line) {
                        if let Ok(resp) = serde_json::from_str::<IpcResponse>(&response_line) {
                            if resp.status == "ok" {
                                if let Some(data) = resp.data {
                                    let wb = data["write_back_enabled"].as_bool().unwrap_or(false);
                                    let dirty = data["dirty_blocks"].as_u64().unwrap_or(0);
                                    let driver_dirty =
                                        data["driver_dirty_count"].as_u64().unwrap_or(0);
                                    let journal_unc =
                                        data["journal_uncommitted"].as_u64().unwrap_or(0);
                                    let mut lock = state.lock().unwrap_or_else(|e| e.into_inner());
                                    lock.flush_status = AppFlushStatus {
                                        write_back_enabled: wb,
                                        dirty_blocks: dirty,
                                        driver_dirty_count: driver_dirty,
                                        journal_uncommitted: journal_unc,
                                    };
                                }
                            }
                        }
                    }
                }
            }

            thread::sleep(Duration::from_millis(50));
        }

        {
            let mut lock = state.lock().unwrap_or_else(|e| e.into_inner());
            lock.connected = false;
        }
        thread::sleep(Duration::from_millis(1000));
    }
}

fn find_service_exe() -> Option<PathBuf> {
    let exe_path = std::env::current_exe().ok()?;
    let exe_dir = exe_path.parent()?;
    let release_dir = exe_dir.join(r"..\..\..\target\release");
    let release_svc = release_dir.join("nova-cache-service.exe");
    if release_svc.exists() {
        return release_svc.canonicalize().ok();
    }
    let local_svc = exe_dir.join("nova-cache-service.exe");
    if local_svc.exists() {
        return local_svc.canonicalize().ok();
    }
    None
}

fn find_project_root() -> Option<PathBuf> {
    let exe_path = std::env::current_exe().ok()?;
    let exe_dir = exe_path.parent()?;
    let root = exe_dir.join(r"..\..\..");
    let root = root.canonicalize().unwrap_or(root);
    if root.join("config").join("nova_cache.toml").exists() {
        return Some(root);
    }
    std::env::current_dir().ok()
}

fn launch_service_elevated(service_path: &PathBuf) -> bool {
    let path_wide: Vec<u16> = OsStr::new(service_path.to_string_lossy().as_ref())
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let verb_wide: Vec<u16> = OsStr::new("runas")
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let args_wide: Vec<u16> = OsStr::new("--console")
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let work_dir = find_project_root()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let dir_wide: Vec<u16> = OsStr::new(&work_dir)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    unsafe {
        let result = ShellExecuteW(
            None,
            windows::core::PCWSTR(verb_wide.as_ptr()),
            windows::core::PCWSTR(path_wide.as_ptr()),
            windows::core::PCWSTR(args_wide.as_ptr()),
            windows::core::PCWSTR(dir_wide.as_ptr()),
            SW_HIDE,
        );
        result.0 as isize > 32
    }
}

fn send_request(pipe: &mut std::fs::File, req: &IpcRequest) -> Result<()> {
    let mut req_str = serde_json::to_string(req)?;
    req_str.push('\n');
    pipe.write_all(req_str.as_bytes())?;
    pipe.flush()?;
    Ok(())
}

fn query_config(
    pipe: &mut std::fs::File,
    reader: &mut BufReader<std::fs::File>,
    response_line: &mut String,
    state: &Arc<Mutex<SharedAppState>>,
) -> Result<()> {
    send_request(pipe, &IpcRequest::GetConfig)?;
    response_line.clear();
    reader.read_line(response_line)?;
    let resp: IpcResponse = serde_json::from_str(response_line)?;
    if resp.status == "ok" {
        if let Some(data) = resp.data {
            let l1 = serde_json::from_value::<u32>(data["l1_size_mb"].clone()).unwrap_or(2048);
            let l2 = serde_json::from_value::<u32>(data["l2_size_gb"].clone()).unwrap_or(2);
            let bs = serde_json::from_value::<u32>(data["block_size_kb"].clone()).unwrap_or(64);
            let l2_path =
                serde_json::from_value::<String>(data["l2_path"].clone()).unwrap_or_default();
            let mut lock = state.lock().unwrap_or_else(|e| e.into_inner());
            lock.config = AppConfig {
                l1_size_mb: l1,
                l2_size_gb: l2,
                block_size_kb: bs,
                l2_path,
                ..lock.config.clone()
            };
        }
    }

    Ok(())
}

fn query_l2_backends(
    pipe: &mut std::fs::File,
    reader: &mut BufReader<std::fs::File>,
    response_line: &mut String,
    state: &Arc<Mutex<SharedAppState>>,
) -> Result<()> {
    send_request(pipe, &IpcRequest::GetL2Backends)?;
    response_line.clear();
    reader.read_line(response_line)?;
    let resp: IpcResponse = serde_json::from_str(response_line)?;
    if resp.status == "ok" {
        if let Some(data) = resp.data {
            if let Some(arr) = data["backends"].as_array() {
                let backends: Vec<String> = arr
                    .iter()
                    .filter_map(|b| b["path"].as_str().map(|s| s.to_string()))
                    .collect();
                let mut lock = state.lock().unwrap_or_else(|e| e.into_inner());
                lock.config.l2_backends = backends;
            }
        }
    }
    Ok(())
}

fn query_volumes(
    pipe: &mut std::fs::File,
    reader: &mut BufReader<std::fs::File>,
    response_line: &mut String,
    state: &Arc<Mutex<SharedAppState>>,
) -> Result<()> {
    send_request(pipe, &IpcRequest::GetVolumes)?;
    response_line.clear();
    reader.read_line(response_line)?;
    let resp: IpcResponse = serde_json::from_str(response_line)?;
    if resp.status == "ok" {
        if let Some(data) = resp.data {
            if let Some(vols) = data["volumes"].as_array() {
                let volumes: Vec<VolumeInfo> = vols
                    .iter()
                    .filter_map(|v| {
                        Some(VolumeInfo {
                            volume: v["volume"].as_str()?.to_string(),
                            enabled: v["enabled"].as_bool().unwrap_or(true),
                        })
                    })
                    .collect();
                let mut lock = state.lock().unwrap_or_else(|e| e.into_inner());
                lock.volumes = volumes;
            }
        }
    }
    Ok(())
}

fn handle_status_response(response_line: &str, state: &Arc<Mutex<SharedAppState>>) {
    if let Ok(resp) = serde_json::from_str::<IpcResponse>(response_line) {
        let mut lock = state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(data) = resp.data {
            if let Some(msg) = data["message"].as_str() {
                lock.status_msg = Some((msg.to_string(), resp.status != "ok"));
            }
        } else if let Some(err) = resp.error {
            lock.status_msg = Some((err, true));
        }
    }
}

impl eframe::App for NovaCacheApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let (connected, stats, config, volumes, _flush_status, error_msg) = {
            let state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
            (
                state.connected,
                state.stats.clone(),
                state.config.clone(),
                state.volumes.clone(),
                state.flush_status.clone(),
                state.error_msg.clone(),
            )
        };

        if self.l1_input.is_empty() && config.l1_size_mb > 0 {
            self.l1_input = config.l1_size_mb.to_string();
        }
        if self.l2_input.is_empty() && config.l2_size_gb > 0 {
            self.l2_input = config.l2_size_gb.to_string();
        }

        ctx.input(|i| {
            if let Some(rect) = i.viewport().inner_rect {
                self.window_rect = (rect.min.x, rect.min.y, rect.width(), rect.height());
            }
        });

        if let Some(rx) = &self.bench_rx {
            let mut done = false;
            loop {
                match rx.try_recv() {
                    Ok(line) => {
                        if self.test_mode == TestMode::Performance {
                            if let Some(stripped) = line.strip_prefix("  ") {
                                if let Some(drive_ch) = stripped.chars().next() {
                                    if drive_ch.is_ascii_alphabetic()
                                        && stripped.starts_with(&format!("{}:\\", drive_ch))
                                    {
                                        if line.contains("DATA CORRUPTED") {
                                            if !self.corrupted_drives.contains(&drive_ch) {
                                                self.corrupted_drives.push(drive_ch);
                                            }
                                        } else if line.contains("DATA OK") || line.contains("PASS")
                                        {
                                            self.corrupted_drives.retain(|&c| c != drive_ch);
                                        }
                                    }
                                }
                            }
                        }
                        self.bench_output.push(line);
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        done = true;
                        break;
                    }
                }
            }
            if done {
                self.bench_rx = None;
                self.test_mode = TestMode::None;
                self.needs_reboot = self
                    .corrupted_drives
                    .iter()
                    .any(|&c| c.to_ascii_uppercase() == 'C');
            }
        }

        let total_requests = stats.hits_t1 + stats.hits_t2 + stats.misses;
        let hit_rate = if total_requests > 0 {
            ((stats.hits_t1 + stats.hits_t2) as f64 / total_requests as f64) * 100.0
        } else {
            0.0
        };

        ctx.request_repaint_after(Duration::from_millis(200));

        egui::SidePanel::left("config_panel").frame(egui::Frame::NONE.inner_margin(egui::Margin::symmetric(12, 8))).resizable(true).default_width(400.0).min_width(350.0).max_width(550.0).show(ctx, |ui| {
            ui.add_space(10.0);
            ui.heading(egui::RichText::new("Settings").color(egui::Color32::from_rgb(180, 185, 195)));
            ui.separator();
            ui.add_space(8.0);

            ui.group(|ui| {
                ui.label(egui::RichText::new("L1 Cache (RAM)").strong().size(14.0).color(egui::Color32::from_rgb(180, 185, 195)));
                ui.add_space(4.0);
                ui.horizontal(|ui| { ui.label(egui::RichText::new("Size (MB):").size(13.0).color(egui::Color32::from_rgb(160, 165, 175))); ui.text_edit_singleline(&mut self.l1_input); });
                ui.add_space(6.0);
                    let btn = egui::Button::new(egui::RichText::new("Apply").strong().color(egui::Color32::from_rgb(220, 220, 220))).fill(egui::Color32::from_rgb(50, 90, 130));
                if ui.add(btn).clicked() {
                    let l1 = self.l1_input.parse::<u64>().ok();
                    let _ = self.command_tx.send(IpcCommand::SetConfig { l1, l2: None });
                }
            });

            ui.add_space(8.0);

            ui.group(|ui| {
                ui.label(egui::RichText::new("L2 Cache (SSD)").strong().size(14.0).color(egui::Color32::from_rgb(180, 185, 195)));
                ui.add_space(4.0);
                ui.horizontal(|ui| { ui.label(egui::RichText::new("Size (GB):").size(13.0).color(egui::Color32::from_rgb(160, 165, 175))); ui.text_edit_singleline(&mut self.l2_input); });
                ui.add_space(4.0);
                if !config.l2_path.is_empty() {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("Active:").strong().color(egui::Color32::from_rgb(160, 165, 175)));
                        let active = if config.l2_backends.is_empty() { config.l2_path.clone() } else { config.l2_backends.join(", ") };
                        ui.label(egui::RichText::new(&active).small().color(egui::Color32::from_rgb(80, 130, 80)));
                    });
                }
                ui.add_space(4.0);
                ui.label(egui::RichText::new("Select L2 drives (check to enable):").strong().size(13.0).color(egui::Color32::from_rgb(160, 165, 175)));
                ui.add_space(2.0);
                let mut changed_paths = false;
                let mut new_backends: Vec<String> = config.l2_backends.clone();
                for drive in &self.system_drives {
                    let (badge, badge_color) = format_drive_type(&drive.drive_type);
                    let drive_path = format!("{}:\\l2_cache.dat", drive.letter);
                    let is_active = config.l2_backends.iter().any(|p| p.starts_with(&format!("{}:\\", drive.letter)));

                    let fs = if drive.fs_type.is_empty() { "?" } else { &drive.fs_type };
                    let _fs_color = if drive.fs_type.eq_ignore_ascii_case("NTFS") { egui::Color32::from_rgb(120, 120, 130) } else { egui::Color32::from_rgb(170, 100, 60) };

                    ui.horizontal(|ui| {
                        let mut checked = is_active;
                        if ui.checkbox(&mut checked, "").changed() {
                            if checked {
                                if !new_backends.iter().any(|p| p.starts_with(&format!("{}:\\", drive.letter))) {
                                    new_backends.push(drive_path.clone());
                                    changed_paths = true;
                                }
                            } else {
                                new_backends.retain(|p| !p.starts_with(&format!("{}:\\", drive.letter)));
                                changed_paths = true;
                            }
                        }
                        ui.label(egui::RichText::new(format!("{}:\\", drive.letter)).strong().color(egui::Color32::from_rgb(180, 180, 180)).size(13.0));
                        ui.label(egui::RichText::new(&badge).strong().color(badge_color).size(12.0));
                        ui.label(egui::RichText::new(fs).color(egui::Color32::from_rgb(120, 120, 130)).size(12.0));
                        let name = if drive.label.is_empty() { "" } else { &drive.label };
                        if !name.is_empty() {
                            ui.label(egui::RichText::new(name).color(egui::Color32::from_rgb(120, 120, 130)).size(12.0));
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(egui::RichText::new(format!("{} free / {}", format_gb(drive.free_gb), format_gb(drive.total_gb))).color(egui::Color32::from_rgb(120, 120, 130)).size(12.0));
                        });
                    });
                }
                if changed_paths {
                    let _ = self.command_tx.send(IpcCommand::SetL2Backends { paths: new_backends });
                }
                ui.add_space(6.0);

                let max_l2_gb: u64 = self.system_drives.iter()
                    .filter(|d| config.l2_backends.iter().any(|p| p.starts_with(&format!("{}:\\", d.letter))))
                    .map(|d| d.free_gb as u64)
                    .sum();
                if max_l2_gb > 0 {
                    ui.label(egui::RichText::new(format!("Max available: {} GB", max_l2_gb)).color(egui::Color32::from_rgb(140, 145, 155)).size(12.0));
                }

                let l2_parsed = self.l2_input.parse::<u64>().ok();
                let l2_valid = match l2_parsed {
                    Some(v) if v > 0 && (max_l2_gb == 0 || v <= max_l2_gb) => true,
                    _ => false,
                };
                let btn_color = if l2_valid { egui::Color32::from_rgb(50, 90, 130) } else { egui::Color32::from_rgb(60, 60, 60) };
                let btn = egui::Button::new(egui::RichText::new("Apply L2 Size").strong().color(egui::Color32::from_rgb(220, 220, 220))).fill(btn_color);
                if ui.add(btn).clicked() {
                    if let Some(l2) = l2_parsed {
                        if l2 > 0 && (max_l2_gb == 0 || l2 <= max_l2_gb) {
                            let _ = self.command_tx.send(IpcCommand::SetConfig { l1: None, l2: Some(l2) });
                        }
                    }
                }
            });

            ui.add_space(8.0);

            ui.add_space(12.0);
            ui.label(egui::RichText::new("Cached Volumes").strong().size(15.0).color(egui::Color32::from_rgb(180, 185, 195)));
            ui.separator();
            ui.add_space(6.0);

            for vol in &volumes {
                ui.horizontal(|ui| {
                    let dot = if vol.enabled { egui::Color32::from_rgb(80, 140, 80) } else { egui::Color32::from_rgb(100, 100, 100) };
                    ui.label(egui::RichText::new("\u{25cf}").color(dot));
                    let drive_info = self.system_drives.iter().find(|d| d.letter.to_string() == vol.volume);
                    let vol_label = drive_info
                        .map(|d| if d.label.is_empty() { format!("Drive {}:\\", vol.volume) } else { format!("Drive {}:\\ {}", vol.volume, d.label) })
                        .unwrap_or_else(|| format!("Drive {}:\\", vol.volume));
                    ui.label(egui::RichText::new(vol_label).color(egui::Color32::from_rgb(180, 180, 180)).size(13.0));
                    if let Some(d) = drive_info {
                        let (badge, badge_color) = format_drive_type(&d.drive_type);
                        ui.label(egui::RichText::new(&badge).small().strong().color(badge_color));
                    }
                    let tog = if vol.enabled { "ON" } else { "OFF" };
                    let tb = egui::Button::new(egui::RichText::new(tog).strong()).fill(if vol.enabled { egui::Color32::from_rgb(40, 70, 40) } else { egui::Color32::from_rgb(70, 40, 40) });
                    if ui.add(tb).clicked() { let _ = self.command_tx.send(IpcCommand::SetVolumeEnabled { volume: vol.volume.clone(), enabled: !vol.enabled }); }
                    let rb = egui::Button::new(egui::RichText::new("X").color(egui::Color32::from_rgb(180, 90, 90))).fill(egui::Color32::from_rgb(50, 25, 25));
                    if ui.add(rb).clicked() { let _ = self.command_tx.send(IpcCommand::RemoveVolume { volume: vol.volume.clone() }); }
                });
                ui.add_space(2.0);
            }

            if volumes.is_empty() {
                ui.label(egui::RichText::new("No volumes configured").color(egui::Color32::from_rgb(140, 140, 150)).italics().size(13.0));
            }

            ui.add_space(8.0);
            ui.group(|ui| {
                ui.label(egui::RichText::new("Add Volume").strong().size(14.0).color(egui::Color32::from_rgb(180, 185, 195)));
                ui.add_space(4.0);
                let configured: Vec<String> = volumes.iter().map(|v| v.volume.clone()).collect();
                let available: Vec<&DriveInfo> = self.system_drives.iter().filter(|d| !configured.contains(&d.letter.to_string())).collect();
                if available.is_empty() {
                    ui.label(egui::RichText::new("All drives configured").color(egui::Color32::from_rgb(140, 140, 150)).italics().size(13.0));
                } else {
                    for drive in &available {
                        let (badge, badge_color) = format_drive_type(&drive.drive_type);
                        let lbl = format!("{}:\\", drive.letter);
                        let _resp = ui.allocate_ui_with_layout(
                            egui::vec2(ui.available_width(), 24.0),
                            egui::Layout::left_to_right(egui::Align::Center),
                            |ui| {
                                let btn = egui::Button::new(egui::RichText::new(&lbl).color(egui::Color32::from_rgb(220, 220, 220)).strong().size(12.0)).fill(egui::Color32::from_rgb(50, 100, 70));
                                if ui.add(btn).clicked() {
                                    let _ = self.command_tx.send(IpcCommand::AddVolume { volume: drive.letter.to_string() });
                                }
                                ui.label(egui::RichText::new(&badge).small().strong().color(badge_color));
                                let fs = if drive.fs_type.is_empty() { "" } else { &drive.fs_type };
                                if !fs.is_empty() {
                                    ui.label(egui::RichText::new(fs).color(egui::Color32::from_rgb(120, 120, 130)).size(12.0));
                                }
                                let name = if drive.label.is_empty() { "" } else { &drive.label };
                        if !name.is_empty() {
                            ui.label(egui::RichText::new(name).color(egui::Color32::from_rgb(120, 120, 130)).size(12.0));
                                }
                            },
                        );
                    }
                }
            });

            ui.add_space(8.0);
            ui.separator();
            ui.add_space(4.0);
            let test_running = self.bench_rx.is_some();
            let test_label = if test_running { "Checking..." } else { "Check All Drives" };
            let test_color = if test_running { egui::Color32::from_rgb(70, 70, 70) } else { egui::Color32::from_rgb(50, 90, 130) };
            let test_btn = egui::Button::new(egui::RichText::new(test_label).color(egui::Color32::from_rgb(220, 220, 220)).strong().size(13.0)).fill(test_color);
            if ui.add(test_btn).clicked() && !test_running {
                if let Some(path) = find_memtest_exe() {
                    match std::process::Command::new(path)
                        .stdout(std::process::Stdio::piped())
                        .stderr(std::process::Stdio::piped())
                        .spawn()
                    {
                        Ok(mut child) => {
                            self.bench_output.clear();
                            self.test_mode = TestMode::Performance;
                            let (tx, rx) = mpsc::channel();
                            self.bench_rx = Some(rx);
                            let stdout = child.stdout.take().unwrap();
                            std::thread::spawn(move || {
                                use std::io::{BufRead, BufReader};
                                let reader = BufReader::new(stdout);
                                for line in reader.lines() {
                                    match line {
                                        Ok(l) => { let _ = tx.send(l); }
                                        Err(_) => break,
                                    }
                                }
                            });
                        }
                        Err(e) => {
                            self.status_msg = Some((format!("Failed to launch memtest: {}", e), true));
                        }
                    }
                } else {
                    self.status_msg = Some(("memtest.exe not found. Run 'cargo build --release' in project root.".into(), true));
                }
            }

            let bench_running = self.bench_rx.is_some();
            let bench_label = if bench_running { "Benchmark Running..." } else { "Benchmark L2 Speed" };
            let bench_color = if bench_running { egui::Color32::from_rgb(70, 70, 70) } else { egui::Color32::from_rgb(50, 90, 130) };
            let bench_btn = egui::Button::new(egui::RichText::new(bench_label).color(egui::Color32::from_rgb(220, 220, 220)).strong().size(13.0)).fill(bench_color);
            if ui.add(bench_btn).clicked() && !bench_running {
                if let Some(path) = find_bench_exe() {
                    let drive = config.l2_path.chars().next().unwrap_or('C');
                    let size = config.l2_size_gb;
                    match std::process::Command::new(path)
                        .arg("--drive").arg(drive.to_string())
                        .arg("--size").arg(size.to_string())
                        .stdout(std::process::Stdio::piped())
                        .stderr(std::process::Stdio::piped())
                        .spawn()
                    {
                        Ok(mut child) => {
                            self.bench_output.clear();
                            self.test_mode = TestMode::Benchmark;
                            let (tx, rx) = mpsc::channel();
                            self.bench_rx = Some(rx);
                            let stdout = child.stdout.take().unwrap();
                            std::thread::spawn(move || {
                                use std::io::{BufRead, BufReader};
                                let reader = BufReader::new(stdout);
                                for line in reader.lines() {
                                    match line {
                                        Ok(l) => { let _ = tx.send(l); }
                                        Err(_) => break,
                                    }
                                }
                            });
                        }
                        Err(e) => {
                            self.status_msg = Some((format!("Failed to launch benchmark: {}", e), true));
                        }
                    }
                } else {
                    self.status_msg = Some(("nova-bench.exe not found. Run 'cargo build --release' in project root.".into(), true));
                }
            }

            if let Some((ref msg, is_error)) = self.status_msg {
                ui.add_space(6.0);
                let c = if is_error { egui::Color32::from_rgb(170, 80, 80) } else { egui::Color32::from_rgb(90, 150, 90) };
                ui.colored_label(c, egui::RichText::new(msg).size(13.0));
            }

            ui.add_space(12.0);
            ui.separator();
            ui.add_space(6.0);
            ui.label(format!("L1: {} MB | L2: {} GB | {} KB blocks", config.l1_size_mb, config.l2_size_gb, config.block_size_kb));
        });

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.inner_margin(egui::Margin::symmetric(16, 8)))
            .show(ctx, |ui| {
                if !connected {
                    ui.vertical_centered(|ui| {
                        ui.add_space(100.0);
                        ui.spinner();
                        ui.add_space(10.0);
                        if let Some(err) = error_msg {
                            ui.colored_label(egui::Color32::from_rgb(170, 80, 80), err);
                        } else {
                            ui.label("Connecting to named pipe...");
                        }
                    });
                    return;
                }

                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new("NOVA CACHE MONITOR")
                        .font(egui::FontId::proportional(18.0))
                        .strong()
                        .color(egui::Color32::from_rgb(90, 130, 170)),
                );
                ui.add_space(12.0);

                egui::Grid::new("stats_grid")
                    .num_columns(2)
                    .spacing([16.0, 10.0])
                    .min_col_width(160.0)
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new("Total I/O Requests")
                                .size(13.0)
                                .color(egui::Color32::from_rgb(140, 140, 140)),
                        );
                        ui.label(
                            egui::RichText::new(total_requests.to_string())
                                .size(16.0)
                                .color(egui::Color32::WHITE)
                                .strong(),
                        );
                        ui.end_row();

                        ui.label(
                            egui::RichText::new("Cache Hit Rate")
                                .size(13.0)
                                .color(egui::Color32::from_rgb(140, 140, 140)),
                        );
                        ui.label(
                            egui::RichText::new(format!("{:.2}%", hit_rate))
                                .size(16.0)
                                .color(egui::Color32::from_rgb(90, 150, 90))
                                .strong(),
                        );
                        ui.end_row();

                        ui.label(
                            egui::RichText::new("L1 Hits (T1 / T2)")
                                .size(13.0)
                                .color(egui::Color32::from_rgb(140, 140, 140)),
                        );
                        ui.label(
                            egui::RichText::new(format!("{} / {}", stats.hits_t1, stats.hits_t2))
                                .size(16.0)
                                .color(egui::Color32::from_rgb(170, 140, 70))
                                .strong(),
                        );
                        ui.end_row();

                        ui.label(
                            egui::RichText::new("Misses / Evictions")
                                .size(13.0)
                                .color(egui::Color32::from_rgb(140, 140, 140)),
                        );
                        ui.label(
                            egui::RichText::new(format!("{} / {}", stats.misses, stats.evictions))
                                .size(16.0)
                                .color(egui::Color32::from_rgb(170, 90, 90))
                                .strong(),
                        );
                        ui.end_row();

                        ui.label(
                            egui::RichText::new("L1 Size")
                                .size(13.0)
                                .color(egui::Color32::from_rgb(140, 140, 140)),
                        );
                        ui.label(
                            egui::RichText::new(format!("{} MB", config.l1_size_mb))
                                .size(14.0)
                                .color(egui::Color32::from_rgb(180, 180, 180)),
                        );
                        ui.end_row();

                        ui.label(
                            egui::RichText::new("L2 Size")
                                .size(13.0)
                                .color(egui::Color32::from_rgb(140, 140, 140)),
                        );
                        ui.label(
                            egui::RichText::new(format!("{} GB", config.l2_size_gb))
                                .size(14.0)
                                .color(egui::Color32::from_rgb(180, 180, 180)),
                        );
                        ui.end_row();

                        ui.label(
                            egui::RichText::new("L2 Fill")
                                .size(13.0)
                                .color(egui::Color32::from_rgb(140, 140, 140)),
                        );
                        let l2_pct = if stats.l2_total_blocks > 0 {
                            stats.l2_block_count as f32 / stats.l2_total_blocks as f32
                        } else {
                            0.0
                        };
                        let l2_pct_capped = l2_pct.min(1.0);
                        ui.horizontal(|ui| {
                            let (rect, _) =
                                ui.allocate_at_least(egui::vec2(140.0, 14.0), egui::Sense::hover());
                            let painter = ui.painter();
                            painter.rect_filled(rect, 3.0, egui::Color32::from_rgb(40, 40, 40));
                            let mut fill_rect = rect;
                            fill_rect.max.x = fill_rect.min.x + rect.width() * l2_pct_capped;
                            let fill_color = if l2_pct_capped > 0.8 {
                                egui::Color32::from_rgb(200, 100, 80)
                            } else {
                                egui::Color32::from_rgb(80, 150, 200)
                            };
                            painter.rect_filled(fill_rect, 3.0, fill_color);
                            ui.label(
                                egui::RichText::new(format!("{:.1}%", l2_pct * 100.0))
                                    .size(12.0)
                                    .color(egui::Color32::WHITE),
                            );
                        });
                        ui.end_row();

                        ui.label(
                            egui::RichText::new("L1 Fill")
                                .size(13.0)
                                .color(egui::Color32::from_rgb(140, 140, 140)),
                        );
                        let l1_pct = if stats.l1_total_blocks > 0 {
                            stats.l1_block_count as f32 / stats.l1_total_blocks as f32
                        } else {
                            0.0
                        };
                        let l1_pct_capped = l1_pct.min(1.0);
                        ui.horizontal(|ui| {
                            let (rect, _) =
                                ui.allocate_at_least(egui::vec2(140.0, 14.0), egui::Sense::hover());
                            let painter = ui.painter();
                            painter.rect_filled(rect, 3.0, egui::Color32::from_rgb(40, 40, 40));
                            let mut fill_rect = rect;
                            fill_rect.max.x = fill_rect.min.x + rect.width() * l1_pct_capped;
                            let fill_color = if l1_pct_capped > 0.8 {
                                egui::Color32::from_rgb(200, 180, 80)
                            } else {
                                egui::Color32::from_rgb(90, 170, 120)
                            };
                            painter.rect_filled(fill_rect, 3.0, fill_color);
                            ui.label(
                                egui::RichText::new(format!("{:.1}%", l1_pct * 100.0))
                                    .size(12.0)
                                    .color(egui::Color32::WHITE),
                            );
                        });
                        ui.end_row();

                        ui.label(
                            egui::RichText::new("Block Size")
                                .size(13.0)
                                .color(egui::Color32::from_rgb(140, 140, 140)),
                        );
                        ui.label(
                            egui::RichText::new(format!("{} KB", config.block_size_kb))
                                .size(14.0)
                                .color(egui::Color32::from_rgb(180, 180, 180)),
                        );
                        ui.end_row();

                        ui.label(
                            egui::RichText::new("Connected Volumes")
                                .size(13.0)
                                .color(egui::Color32::from_rgb(140, 140, 140)),
                        );
                        ui.label(
                            egui::RichText::new(format!("{}", volumes.len()))
                                .size(14.0)
                                .color(egui::Color32::from_rgb(180, 180, 180)),
                        );
                        ui.end_row();

                        ui.label(
                            egui::RichText::new("Boost")
                                .size(13.0)
                                .color(egui::Color32::from_rgb(140, 140, 140)),
                        );
                        let m = stats.perf_multiplier;
                        let boost_color = if m > 1.01 {
                            egui::Color32::from_rgb(90, 170, 120)
                        } else if m < 0.99 {
                            egui::Color32::from_rgb(180, 90, 90)
                        } else {
                            egui::Color32::from_rgb(180, 180, 180)
                        };
                        ui.label(
                            egui::RichText::new(format!("x{:.2}", m))
                                .size(16.0)
                                .color(boost_color)
                                .strong(),
                        );
                        ui.end_row();

                        ui.label(
                            egui::RichText::new("HDD Read Latency")
                                .size(13.0)
                                .color(egui::Color32::from_rgb(140, 140, 140)),
                        );
                        ui.label(
                            egui::RichText::new(format!(
                                "{} us",
                                stats.hdd_read_latency_us.round() as u64
                            ))
                            .size(14.0)
                            .color(egui::Color32::from_rgb(180, 180, 180)),
                        );
                        ui.end_row();

                        ui.label(
                            egui::RichText::new("HDD Write Latency")
                                .size(13.0)
                                .color(egui::Color32::from_rgb(140, 140, 140)),
                        );
                        ui.label(
                            egui::RichText::new(format!(
                                "{} us",
                                stats.hdd_write_latency_us.round() as u64
                            ))
                            .size(14.0)
                            .color(egui::Color32::from_rgb(180, 180, 180)),
                        );
                        ui.end_row();

                        ui.label(
                            egui::RichText::new("L1 Read Latency")
                                .size(13.0)
                                .color(egui::Color32::from_rgb(140, 140, 140)),
                        );
                        ui.label(
                            egui::RichText::new(format!(
                                "{} us",
                                stats.l1_read_latency_us.round() as u64
                            ))
                            .size(14.0)
                            .color(egui::Color32::from_rgb(180, 180, 180)),
                        );
                        ui.end_row();

                        ui.label(
                            egui::RichText::new("Driver Cache Hits")
                                .size(13.0)
                                .color(egui::Color32::from_rgb(140, 140, 140)),
                        );
                        ui.label(
                            egui::RichText::new(format!(
                                "{} / {} reads",
                                stats.driver_cache_hits, stats.driver_total_reads
                            ))
                            .size(14.0)
                            .color(egui::Color32::from_rgb(180, 180, 180)),
                        );
                        ui.end_row();
                    });

                ui.add_space(16.0);
                ui.separator();
                ui.add_space(8.0);

                let header_text = match self.test_mode {
                    TestMode::Performance => "Drive Check Results",
                    TestMode::Benchmark => "L2 Benchmark Results",
                    TestMode::None => "Test Output",
                };
                ui.label(
                    egui::RichText::new(header_text)
                        .font(egui::FontId::proportional(16.0))
                        .strong()
                        .color(egui::Color32::from_rgb(90, 130, 170)),
                );
                ui.add_space(6.0);

                if self.bench_output.is_empty() {
                    ui.label(
                        egui::RichText::new(
                            "Click 'Check All Drives' or 'Benchmark L2 Speed' to start",
                        )
                        .size(13.0)
                        .color(egui::Color32::from_rgb(100, 100, 100)),
                    );
                } else {
                    egui::ScrollArea::vertical()
                        .max_height(300.0)
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            for line in &self.bench_output {
                                let is_separator = line.trim().is_empty()
                                    || line.chars().all(|c| c == '-' || c == ' ');
                                let is_title = line.contains("NovaCache")
                                    || line.contains("========")
                                    || line.contains("L2 Benchmark")
                                    || line.contains("Nova Cache")
                                    || line.contains("Extended Multi-Drive");
                                let is_result = line.contains("MB/s")
                                    || line.contains("GB/s")
                                    || line.contains("IOPS")
                                    || line.contains("us")
                                    || line.contains("ms")
                                    || line.contains("DATA OK")
                                    || line.contains("DATA CORRUPTED");
                                let is_header = (line.contains("Test")
                                    && line.contains("Throughput"))
                                    || line.starts_with("===");
                                let is_ok = line.contains("OK") || line.contains("complete");

                                let is_corrupted = line.contains("DATA CORRUPTED");
                                let is_failed = line.contains("FAIL");

                                let (color, size) = if is_corrupted {
                                    (egui::Color32::from_rgb(180, 70, 70), 14.0)
                                } else if is_failed {
                                    (egui::Color32::from_rgb(180, 70, 70), 13.0)
                                } else if is_title {
                                    (egui::Color32::from_rgb(90, 130, 170), 14.0)
                                } else if is_header {
                                    (egui::Color32::from_rgb(160, 160, 160), 13.0)
                                } else if is_result {
                                    (egui::Color32::from_rgb(140, 170, 140), 14.0)
                                } else if is_ok {
                                    (egui::Color32::from_rgb(100, 150, 100), 13.0)
                                } else if is_separator {
                                    (egui::Color32::from_rgb(90, 90, 90), 12.0)
                                } else {
                                    (egui::Color32::from_rgb(160, 160, 160), 13.0)
                                };
                                ui.label(
                                    egui::RichText::new(line)
                                        .color(color)
                                        .monospace()
                                        .size(size),
                                );
                            }
                        });
                }

                if !self.bench_output.is_empty() && self.bench_rx.is_none() {
                    ui.add_space(4.0);
                    let copy_btn = egui::Button::new(
                        egui::RichText::new("Copy Results")
                            .strong()
                            .color(egui::Color32::from_rgb(220, 220, 220))
                            .size(12.0),
                    )
                    .fill(egui::Color32::from_rgb(50, 70, 90));
                    if ui.add(copy_btn).clicked() {
                        let text = self.bench_output.join("\n");
                        ui.ctx().copy_text(text);
                        self.status_msg = Some(("Results copied to clipboard".into(), false));
                    }
                }

                if !self.corrupted_drives.is_empty() {
                    ui.add_space(12.0);
                    ui.separator();
                    ui.add_space(8.0);
                    ui.colored_label(
                        egui::Color32::from_rgb(180, 70, 70),
                        egui::RichText::new(format!(
                            "Data corruption detected on: {}",
                            self.corrupted_drives
                                .iter()
                                .map(|c| format!("{}:\\", c))
                                .collect::<Vec<_>>()
                                .join(", ")
                        ))
                        .strong()
                        .size(14.0),
                    );
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new("Run chkdsk to repair filesystem errors.")
                            .color(egui::Color32::from_rgb(160, 160, 170))
                            .size(13.0),
                    );
                    ui.add_space(6.0);
                    for &drive in &self.corrupted_drives {
                        ui.horizontal(|ui| {
                            let label = format!("Repair {}:\\", drive);
                            let btn = egui::Button::new(
                                egui::RichText::new(&label)
                                    .strong()
                                    .color(egui::Color32::from_rgb(220, 220, 220))
                                    .size(13.0),
                            )
                            .fill(egui::Color32::from_rgb(80, 50, 30));
                            if ui.add(btn).clicked() {
                                let drive_str = format!("{}:", drive);
                                let _ = std::process::Command::new("chkdsk")
                                    .arg(&drive_str)
                                    .arg("/f")
                                    .arg("/r")
                                    .spawn();
                                self.status_msg = Some((
                                    format!(
                                        "chkdsk {}:\\ /f /r launched. You may need to approve UAC.",
                                        drive
                                    ),
                                    false,
                                ));
                            }
                        });
                        ui.add_space(2.0);
                    }
                    if self.needs_reboot {
                        ui.add_space(6.0);
                        ui.label(
                            egui::RichText::new("System drive (C:\\) requires reboot to repair.")
                                .color(egui::Color32::from_rgb(180, 140, 80))
                                .size(13.0),
                        );
                        let btn = egui::Button::new(
                            egui::RichText::new("Reboot for Repair")
                                .strong()
                                .color(egui::Color32::from_rgb(220, 220, 220))
                                .size(13.0),
                        )
                        .fill(egui::Color32::from_rgb(140, 70, 30));
                        if ui.add(btn).clicked() {
                            let _ = std::process::Command::new("shutdown")
                                .args([
                                    "/r",
                                    "/t",
                                    "10",
                                    "/c",
                                    "NovaCache: Rebooting to repair disk errors",
                                ])
                                .spawn();
                            self.status_msg =
                                Some(("System will reboot in 10 seconds...".into(), false));
                        }
                    }
                }
            });

        egui::TopBottomPanel::bottom("footer_panel")
            .frame(egui::Frame::NONE.inner_margin(egui::Margin::symmetric(0, 6)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.add_space(ui.available_width() - 240.0);
                    let link_color = egui::Color32::from_rgb(80, 140, 210);

                    if ui
                        .add(LinkButton {
                            text: "nova-app.eu".into(),
                            color: link_color,
                        })
                        .clicked()
                    {
                        let _ = std::process::Command::new("cmd")
                            .args(["/C", "start", "https://nova-app.eu"])
                            .spawn();
                    }

                    ui.label(
                        egui::RichText::new("|")
                            .color(egui::Color32::from_rgb(60, 70, 80))
                            .size(12.0),
                    );

                    if ui
                        .add(LinkButton {
                            text: "Telegram".into(),
                            color: link_color,
                        })
                        .clicked()
                    {
                        let _ = std::process::Command::new("cmd")
                            .args(["/C", "start", "https://t.me/nova_txt"])
                            .spawn();
                    }
                });
            });
    }
}

struct LinkButton {
    text: String,
    color: egui::Color32,
}

impl egui::Widget for LinkButton {
    fn ui(self, ui: &mut egui::Ui) -> egui::Response {
        let font_id = egui::FontId::proportional(12.0);
        let galley = ui.fonts(|f| f.layout_no_wrap(self.text.clone(), font_id, self.color));
        let desired = galley.size() + egui::vec2(16.0, 6.0);
        let (rect, response) = ui.allocate_at_least(desired, egui::Sense::click());

        let rounding = egui::CornerRadius::same(6);
        if response.hovered() {
            ui.painter()
                .rect_filled(rect, rounding, egui::Color32::from_rgb(30, 50, 80));
            ui.painter().rect_stroke(
                rect,
                rounding,
                egui::Stroke::new(1.0, self.color),
                egui::StrokeKind::Inside,
            );
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }

        let text_pos = egui::Align2::CENTER_CENTER.align_size_within_rect(galley.size(), rect);
        ui.painter().galley(text_pos.left_top(), galley, self.color);

        response
    }
}

fn load_gui_state() -> (f32, f32, f32, f32) {
    let path = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("config").join("gui_state.json")))
        .unwrap_or_default();
    if let Ok(content) = std::fs::read_to_string(&path) {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) {
            let x = val.get("x").and_then(|v| v.as_f64()).unwrap_or(100.0) as f32;
            let y = val.get("y").and_then(|v| v.as_f64()).unwrap_or(100.0) as f32;
            let w = val.get("width").and_then(|v| v.as_f64()).unwrap_or(1050.0) as f32;
            let h = val.get("height").and_then(|v| v.as_f64()).unwrap_or(600.0) as f32;
            return (x, y, w.max(400.0).min(3000.0), h.max(300.0).min(2000.0));
        }
    }
    (100.0, 100.0, 1250.0, 700.0)
}

fn save_gui_state(x: f32, y: f32, width: f32, height: f32) {
    let path = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("config").join("gui_state.json")))
        .unwrap_or_default();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let json = serde_json::json!({ "x": x, "y": y, "width": width, "height": height });
    let _ = std::fs::write(
        &path,
        serde_json::to_string_pretty(&json).unwrap_or_default(),
    );
}

fn find_memtest_exe() -> Option<PathBuf> {
    let exe_path = std::env::current_exe().ok()?;
    let exe_dir = exe_path.parent()?;
    let release = exe_dir.join(r"..\..\..\target\release\memtest.exe");
    if release.exists() {
        return release.canonicalize().ok();
    }
    let debug = exe_dir.join(r"..\..\..\target\debug\memtest.exe");
    if debug.exists() {
        return debug.canonicalize().ok();
    }
    let local = exe_dir.join("memtest.exe");
    if local.exists() {
        return local.canonicalize().ok();
    }
    None
}

fn find_bench_exe() -> Option<PathBuf> {
    let exe_path = std::env::current_exe().ok()?;
    let exe_dir = exe_path.parent()?;
    let release = exe_dir.join(r"..\..\..\target\release\nova-bench.exe");
    if release.exists() {
        return release.canonicalize().ok();
    }
    let debug = exe_dir.join(r"..\..\..\target\debug\nova-bench.exe");
    if debug.exists() {
        return debug.canonicalize().ok();
    }
    let local = exe_dir.join("nova-bench.exe");
    if local.exists() {
        return local.canonicalize().ok();
    }
    None
}

fn main() -> Result<()> {
    let log_dir = std::path::PathBuf::from("temp");
    let _ = std::fs::create_dir_all(&log_dir);

    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("log.txt"))
        .expect("Failed to open log file");
    let file_subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_ansi(false)
        .with_writer(std::sync::Mutex::new(log_file))
        .finish();
    tracing::subscriber::set_global_default(file_subscriber)
        .expect("setting default subscriber failed");
    tracing::info!("Nova Cache GUI starting...");

    // Single-instance check via named mutex
    {
        use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
        use windows::Win32::System::Threading::CreateMutexW;
        let name: Vec<u16> = "Global\\NovaCacheGuiSingleInstance\0"
            .encode_utf16()
            .collect();
        unsafe {
            let _handle = CreateMutexW(None, true, PCWSTR(name.as_ptr()));
            if GetLastError() == ERROR_ALREADY_EXISTS {
                eprintln!("Nova Cache GUI is already running. Only one instance allowed.");
                std::process::exit(0);
            }
        }
    }

    let auto_launch = !std::env::args().any(|a| a == "--no-launch");
    let (pos_x, pos_y, win_w, win_h) = load_gui_state();

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Nova Cache Dashboard")
            .with_inner_size([win_w, win_h])
            .with_min_inner_size([900.0, 500.0])
            .with_resizable(true)
            .with_position([pos_x, pos_y]),
        ..Default::default()
    };

    let rect = (pos_x, pos_y, win_w, win_h);
    eframe::run_native(
        "Nova Cache Dashboard",
        native_options,
        Box::new(move |cc| Ok(Box::new(NovaCacheApp::new(cc, auto_launch, rect)))),
    )
    .map_err(|e| anyhow!("Eframe running error: {:?}", e))?;

    Ok(())
}
