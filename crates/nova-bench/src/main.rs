use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rand::Rng;
use windows::Win32::Foundation::*;
use windows::Win32::Storage::FileSystem::*;

const BENCH_FILE_NAME: &str = "nova_l2_bench.tmp";
const WARMUP_RUNS: usize = 1;
const MEASURE_RUNS: usize = 3;
const BENCH_DURATION_SECS: u64 = 5;
const SECTOR_SIZE: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq)]
enum TestType {
    Sequential,
    Random,
}

#[derive(Debug, Clone)]
struct BenchConfig {
    label: &'static str,
    test_type: TestType,
    block_size: usize,
    queue_depth: usize,
}

impl BenchConfig {
    fn seq_1m_q8t1() -> Self {
        Self {
            label: "SEQ1M Q8T1",
            test_type: TestType::Sequential,
            block_size: 1024 * 1024,
            queue_depth: 8,
        }
    }
    fn seq_1m_q1t1() -> Self {
        Self {
            label: "SEQ1M Q1T1",
            test_type: TestType::Sequential,
            block_size: 1024 * 1024,
            queue_depth: 1,
        }
    }
    fn rnd4k_q32t1() -> Self {
        Self {
            label: "RND4K Q32T1",
            test_type: TestType::Random,
            block_size: 4096,
            queue_depth: 32,
        }
    }
    fn rnd4k_q1t1() -> Self {
        Self {
            label: "RND4K Q1T1",
            test_type: TestType::Random,
            block_size: 4096,
            queue_depth: 1,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct BenchResult {
    mb_per_sec: f64,
    iops: f64,
    latency_us: f64,
}

/// Sector-aligned buffer for FILE_FLAG_NO_BUFFERING I/O.
struct AlignedBuf {
    ptr: NonNull<u8>,
    layout: Layout,
}

impl AlignedBuf {
    fn new(size: usize) -> Self {
        let layout = Layout::from_size_align(size, SECTOR_SIZE).unwrap();
        let ptr = unsafe { alloc_zeroed(layout) };
        assert!(!ptr.is_null(), "Failed to allocate aligned buffer");
        Self {
            ptr: NonNull::new(ptr).unwrap(),
            layout,
        }
    }

    fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.layout.size()) }
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.layout.size()) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        unsafe {
            dealloc(self.ptr.as_ptr(), self.layout);
        }
    }
}

struct RawFile {
    handle: HANDLE,
}

impl RawFile {
    fn open_read(path: &str) -> Option<Self> {
        let path_wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            let handle = CreateFileW(
                windows::core::PCWSTR(path_wide.as_ptr()),
                GENERIC_READ.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(FILE_FLAG_NO_BUFFERING.0 | FILE_FLAG_WRITE_THROUGH.0),
                HANDLE::default(),
            );
            match handle {
                Ok(h) if !h.is_invalid() => {
                    let mut size: i64 = 0;
                    let _ = GetFileSizeEx(h, &mut size);
                    Some(Self { handle: h })
                }
                _ => None,
            }
        }
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> bool {
        unsafe {
            let mut pos: i64 = 0;
            let _ = SetFilePointerEx(self.handle, offset as i64, Some(&mut pos), FILE_BEGIN);
            let mut bytes_read = 0u32;
            let result = ReadFile(self.handle, Some(buf), Some(&mut bytes_read), None);
            result.is_ok() && bytes_read as usize == buf.len()
        }
    }

    fn flush(&self) {
        unsafe {
            let _ = FlushFileBuffers(self.handle);
        }
    }
}

impl Drop for RawFile {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

fn create_bench_file(path: &Path, size_bytes: u64) -> anyhow::Result<()> {
    let path_str = path.to_string_lossy().to_string();
    let path_wide: Vec<u16> = path_str.encode_utf16().chain(std::iter::once(0)).collect();

    unsafe {
        let handle = CreateFileW(
            windows::core::PCWSTR(path_wide.as_ptr()),
            GENERIC_READ.0 | GENERIC_WRITE.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            CREATE_ALWAYS,
            FILE_FLAGS_AND_ATTRIBUTES(FILE_FLAG_NO_BUFFERING.0 | FILE_FLAG_WRITE_THROUGH.0),
            HANDLE::default(),
        )?;

        let fill_size = 1024 * 1024;
        let mut buf = AlignedBuf::new(fill_size);
        rand::rng().fill(buf.as_mut_slice());

        let mut written = 0u64;
        let mut last_pct = 0u64;
        while written < size_bytes {
            let to_write = std::cmp::min(fill_size as u64, size_bytes - written) as usize;
            let mut bytes_written = 0u32;
            let result = WriteFile(
                handle,
                Some(&buf.as_slice()[..to_write]),
                Some(&mut bytes_written),
                None,
            );
            if result.is_err() || bytes_written as usize != to_write {
                let _ = CloseHandle(handle);
                return Err(anyhow::anyhow!("Write failed at offset {}", written));
            }
            written += to_write as u64;
            let pct = written * 100 / size_bytes;
            if pct / 10 > last_pct / 10 {
                println!("  Creating test file... {}%", pct);
                last_pct = pct;
            }
        }
        println!("  Creating test file... 100%");

        FlushFileBuffers(handle)?;
        let _ = CloseHandle(handle);
    }

    Ok(())
}

fn worker_thread(
    path: String,
    file_size: u64,
    block_size: usize,
    test_type: TestType,
    stop: Arc<AtomicBool>,
) -> (u64, u64, u128) {
    let mut total_bytes: u64 = 0;
    let mut total_ios: u64 = 0;
    let mut total_latency_ns: u128 = 0;
    let mut buf = AlignedBuf::new(block_size);
    let mut rng = rand::rng();
    let total_blocks = file_size / block_size as u64;

    let f = match RawFile::open_read(&path) {
        Some(f) => f,
        None => return (0, 0, 0),
    };

    while !stop.load(Ordering::Relaxed) {
        let offset = match test_type {
            TestType::Random => {
                let block = rng.random_range(0..total_blocks);
                block * block_size as u64
            }
            TestType::Sequential => 0,
        };

        let t = Instant::now();
        let ok = f.read_at(offset, buf.as_mut_slice());
        let elapsed = t.elapsed();

        if ok {
            total_latency_ns += elapsed.as_nanos();
            total_bytes += block_size as u64;
            total_ios += 1;
        } else {
            break;
        }
    }

    (total_bytes, total_ios, total_latency_ns)
}

fn run_io_test_sequential(
    path: &str,
    file_size: u64,
    config: &BenchConfig,
    stop: &Arc<AtomicBool>,
) -> BenchResult {
    if file_size == 0 {
        return BenchResult::default();
    }

    let block_size = config.block_size;
    let total_blocks = file_size / block_size as u64;
    if total_blocks == 0 {
        return BenchResult::default();
    }

    let effective_threads = std::cmp::max(config.queue_depth, 1);

    if effective_threads <= 1 {
        let f = match RawFile::open_read(path) {
            Some(f) => f,
            None => return BenchResult::default(),
        };

        let bench_start = Instant::now();
        let mut total_bytes: u64 = 0;
        let mut total_ios: u64 = 0;
        let mut total_latency_ns: u128 = 0;
        let mut buf = AlignedBuf::new(block_size);
        let mut rng = rand::rng();
        let mut offset: u64 = 0;

        while !stop.load(Ordering::Relaxed) {
            let read_offset = match config.test_type {
                TestType::Random => {
                    let block = rng.random_range(0..total_blocks);
                    block * block_size as u64
                }
                TestType::Sequential => {
                    let o = offset;
                    offset += block_size as u64;
                    if offset + block_size as u64 > file_size {
                        offset = 0;
                    }
                    o
                }
            };

            let t = Instant::now();
            let ok = f.read_at(read_offset, buf.as_mut_slice());
            let elapsed = t.elapsed();

            if ok {
                total_latency_ns += elapsed.as_nanos();
                total_bytes += block_size as u64;
                total_ios += 1;
            } else {
                break;
            }
        }

        let bench_elapsed = bench_start.elapsed().as_secs_f64();
        if bench_elapsed == 0.0 || total_bytes == 0 {
            return BenchResult::default();
        }

        return BenchResult {
            mb_per_sec: (total_bytes as f64 / (1024.0 * 1024.0)) / bench_elapsed,
            iops: total_ios as f64 / bench_elapsed,
            latency_us: if total_ios > 0 {
                (total_latency_ns as f64 / total_ios as f64) / 1000.0
            } else {
                0.0
            },
        };
    }

    let bench_start = Instant::now();
    let path_owned = path.to_string();
    let mut handles = Vec::new();

    for _ in 0..effective_threads {
        let p = path_owned.clone();
        let s = stop.clone();
        let tt = config.test_type;
        handles.push(std::thread::spawn(move || {
            worker_thread(p, file_size, block_size, tt, s)
        }));
    }

    let mut total_bytes: u64 = 0;
    let mut total_ios: u64 = 0;
    let mut total_latency_ns: u128 = 0;

    for h in handles {
        let (bytes, ios, latency) = h.join().unwrap_or((0, 0, 0));
        total_bytes += bytes;
        total_ios += ios;
        total_latency_ns += latency;
    }

    let elapsed = bench_start.elapsed().as_secs_f64();
    if elapsed == 0.0 || total_bytes == 0 {
        return BenchResult::default();
    }

    BenchResult {
        mb_per_sec: (total_bytes as f64 / (1024.0 * 1024.0)) / elapsed,
        iops: total_ios as f64 / elapsed,
        latency_us: if total_ios > 0 {
            (total_latency_ns as f64 / total_ios as f64) / 1000.0
        } else {
            0.0
        },
    }
}

fn format_mb(v: f64) -> String {
    if v >= 1000.0 {
        format!("{:.2} GB/s", v / 1000.0)
    } else {
        format!("{:.2} MB/s", v)
    }
}

fn format_iops(v: f64) -> String {
    if v >= 1_000_000.0 {
        format!("{:.2}M", v / 1_000_000.0)
    } else if v >= 1_000.0 {
        format!("{:.1}K", v / 1_000.0)
    } else {
        format!("{:.0}", v)
    }
}

fn format_us(v: f64) -> String {
    if v >= 1000.0 {
        format!("{:.2} ms", v / 1000.0)
    } else {
        format!("{:.1} us", v)
    }
}

fn run_benchmarks(drive: char, l2_size_gb: u64, pause_cache: bool) -> anyhow::Result<()> {
    let bench_dir = PathBuf::from(format!("{}:\\", drive));
    let bench_path = bench_dir.join(BENCH_FILE_NAME);
    let bench_size = l2_size_gb * 1024 * 1024 * 1024;

    println!();
    println!("  ==========================================");
    println!("    NovaCache L2 Benchmark v2.0");
    println!("  ==========================================");
    println!();
    println!("  Drive:        {}:\\", drive);
    println!("  Test file:    {}", bench_path.display());
    println!("  Size:         {} GB", l2_size_gb);
    println!("  Mode:         Direct I/O + WriteThrough (bypass all caches)");
    println!("  Warmup:       {} run(s)", WARMUP_RUNS);
    println!("  Measure:      {} run(s), best of", MEASURE_RUNS);
    println!("  Duration:     {}s per run", BENCH_DURATION_SECS);
    if pause_cache {
        println!("  Cache:        PAUSED (raw disk only)");
    }
    println!();

    // Pause NovaCache if requested
    if pause_cache {
        print!("  Pausing NovaCache service... ");
        std::io::stdout().flush()?;
        if let Err(e) = send_ipc_command(r#"{"type":"set_write_back","enabled":false}"#) {
            println!("warning: could not pause cache: {}", e);
        } else {
            println!("OK");
        }
        std::thread::sleep(Duration::from_secs(1));
    }

    println!("  Creating test file ({} GB)...", l2_size_gb);
    create_bench_file(&bench_path, bench_size)?;
    println!("OK");

    // Evict file data from Windows standby cache by flushing + brief sleep
    print!("  Evicting file from OS cache... ");
    std::io::stdout().flush()?;
    {
        let f = RawFile::open_read(&bench_path.to_string_lossy().as_ref());
        if let Some(f) = f {
            f.flush();
            drop(f);
        }
    }
    std::thread::sleep(Duration::from_secs(2));
    println!("OK");

    let file_size = std::fs::metadata(&bench_path)?.len();

    let configs = vec![
        BenchConfig::seq_1m_q8t1(),
        BenchConfig::seq_1m_q1t1(),
        BenchConfig::rnd4k_q32t1(),
        BenchConfig::rnd4k_q1t1(),
    ];

    println!();
    println!(
        "  {:<16} {:>14} {:>12} {:>14}",
        "Test", "Throughput", "IOPS", "Latency"
    );
    println!("  {}", "-".repeat(60));

    let path_str = bench_path.to_string_lossy().to_string();

    for config in &configs {
        let mut best = BenchResult::default();

        for run in 0..(WARMUP_RUNS + MEASURE_RUNS) {
            let is_warmup = run < WARMUP_RUNS;
            let stop = Arc::new(AtomicBool::new(false));
            let stop_clone = stop.clone();
            let bench_duration = Duration::from_secs(BENCH_DURATION_SECS);

            std::thread::spawn(move || {
                std::thread::sleep(bench_duration);
                stop_clone.store(true, Ordering::Relaxed);
            });

            let start = Instant::now();
            let result = run_io_test_sequential(&path_str, file_size, config, &stop);
            let _elapsed = start.elapsed().as_secs_f64();

            if !is_warmup && result.mb_per_sec > best.mb_per_sec {
                best = result;
            }

            if !is_warmup {
                print!(".");
                std::io::stdout().flush()?;
            }
        }

        println!();
        println!(
            "  {:<16} {:>14} {:>12} {:>14}",
            config.label,
            format_mb(best.mb_per_sec),
            format_iops(best.iops),
            format_us(best.latency_us)
        );
    }

    println!();
    print!("  Cleaning up... ");
    std::io::stdout().flush()?;
    drop(std::fs::remove_file(&bench_path));
    println!("OK");

    if pause_cache {
        print!("  Resuming NovaCache service... ");
        std::io::stdout().flush()?;
        if let Err(e) = send_ipc_command(r#"{"type":"set_write_back","enabled":true}"#) {
            println!("warning: could not resume cache: {}", e);
        } else {
            println!("OK");
        }
    }

    println!();

    println!("  ==========================================");
    println!("    Benchmark complete!");
    println!("  ==========================================");
    println!();

    Ok(())
}

fn send_ipc_command(json: &str) -> anyhow::Result<()> {
    let pipe_name = r"\\.\pipe\NovaCacheIpc";
    let mut pipe = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(pipe_name)?;
    let mut req = json.to_string();
    req.push('\n');
    std::io::Write::write_all(&mut pipe, req.as_bytes())?;
    std::io::Write::flush(&mut pipe)?;
    let mut reader = std::io::BufReader::new(pipe);
    let mut response = String::new();
    std::io::BufRead::read_line(&mut reader, &mut response)?;
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mut drive: char = 'C';
    let mut size_gb: u64 = 2;
    let mut pause_cache = false;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--drive" | "-d" => {
                i += 1;
                if let Some(v) = args.get(i) {
                    drive = v
                        .chars()
                        .next()
                        .unwrap_or('C')
                        .to_uppercase()
                        .next()
                        .unwrap_or('C');
                }
            }
            "--size" | "-s" => {
                i += 1;
                if let Some(v) = args.get(i) {
                    size_gb = v.parse().unwrap_or(2);
                }
            }
            "--no-cache" => {
                pause_cache = true;
            }
            "--help" | "-h" => {
                println!("NovaCache L2 Benchmark v2.0");
                println!();
                println!("Usage: nova-bench.exe [OPTIONS]");
                println!();
                println!("Options:");
                println!("  -d, --drive <LETTER>  Drive letter (default: C)");
                println!("  -s, --size <GB>       Test file size in GB (default: 2)");
                println!("  --no-cache            Pause NovaCache during benchmark (raw disk)");
                println!("  -h, --help            Show this help");
                return Ok(());
            }
            _ => {}
        }
        i += 1;
    }

    run_benchmarks(drive, size_gb, pause_cache)
}
