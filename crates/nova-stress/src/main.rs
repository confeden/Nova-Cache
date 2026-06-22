use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use rand::Rng;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, DeleteFileW, ReadFile, WriteFile, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL,
    FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};

const TEST_FILE_PREFIX: &str = "nova_stress_";

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn create_path(drive: char, name: &str) -> String {
    format!("{}:\\{}", drive, name)
}

unsafe fn open_write(path: &str) -> Result<HANDLE> {
    let wide = to_wide(path);
    let h = CreateFileW(
        PCWSTR(wide.as_ptr()),
        GENERIC_WRITE.0,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
        None,
        CREATE_ALWAYS,
        FILE_ATTRIBUTE_NORMAL,
        None,
    )?;
    Ok(h)
}

unsafe fn open_read(path: &str) -> Result<HANDLE> {
    let wide = to_wide(path);
    let h = CreateFileW(
        PCWSTR(wide.as_ptr()),
        GENERIC_READ.0,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
        None,
        OPEN_EXISTING,
        FILE_ATTRIBUTE_NORMAL,
        None,
    )?;
    Ok(h)
}

fn write_file(path: &str, data: &[u8]) -> Result<()> {
    unsafe {
        let h = open_write(path)?;
        let mut written = 0u32;
        WriteFile(h, Some(data), Some(&mut written), None)?;
        CloseHandle(h)?;
    }
    Ok(())
}

fn read_file(path: &str, buf: &mut [u8]) -> Result<usize> {
    unsafe {
        let h = open_read(path)?;
        let mut total = 0usize;
        while total < buf.len() {
            let chunk = &mut buf[total..];
            let mut read = 0u32;
            ReadFile(h, Some(chunk), Some(&mut read), None)?;
            if read == 0 {
                break;
            }
            total += read as usize;
        }
        CloseHandle(h)?;
        Ok(total)
    }
}

fn delete_file_safe(path: &str) -> Result<()> {
    let wide = to_wide(path);
    unsafe {
        DeleteFileW(PCWSTR(wide.as_ptr()))?;
    }
    Ok(())
}

fn simple_crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFFFFFF;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB88320
            } else {
                crc >> 1
            };
        }
    }
    crc ^ 0xFFFFFFFF
}

struct Stats {
    reads: AtomicU64,
    writes: AtomicU64,
    read_bytes: AtomicU64,
    write_bytes: AtomicU64,
    integrity_errors: AtomicU64,
    io_errors: AtomicU64,
}

impl Stats {
    fn new() -> Self {
        Self {
            reads: AtomicU64::new(0),
            writes: AtomicU64::new(0),
            read_bytes: AtomicU64::new(0),
            write_bytes: AtomicU64::new(0),
            integrity_errors: AtomicU64::new(0),
            io_errors: AtomicU64::new(0),
        }
    }
}

fn write_test_file(path: &str, idx: usize, file_size: usize, fill_byte: u8) -> Result<()> {
    let mut data = vec![0u8; file_size];
    data[4..].fill(fill_byte);
    let data_crc = simple_crc32(&data[4..]);
    data[..4].copy_from_slice(&data_crc.to_le_bytes());
    write_file(path, &data)
}

fn verify_file(path: &str, file_size: usize) -> Result<bool> {
    let mut buf = vec![0u8; file_size];
    let n = read_file(path, &mut buf)?;
    if n != file_size {
        return Ok(false);
    }
    let stored_crc = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let actual_crc = simple_crc32(&buf[4..]);
    Ok(stored_crc == actual_crc)
}

fn reader_thread(
    drive: char,
    file_count: usize,
    file_size: usize,
    stop: Arc<AtomicBool>,
    stats: Arc<Stats>,
    _tid: usize,
) {
    let mut rng = rand::rng();
    let mut buf = vec![0u8; file_size];

    while !stop.load(Ordering::Relaxed) {
        let idx = rng.random_range(0..file_count);
        let path = create_path(drive, &format!("{}{}", TEST_FILE_PREFIX, idx));

        match read_file(&path, &mut buf) {
            Ok(n) => {
                stats.reads.fetch_add(1, Ordering::Relaxed);
                stats.read_bytes.fetch_add(n as u64, Ordering::Relaxed);
                if n == file_size {
                    let stored_crc = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
                    let actual_crc = simple_crc32(&buf[4..]);
                    if stored_crc != actual_crc {
                        stats.integrity_errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            Err(_) => {
                stats.io_errors.fetch_add(1, Ordering::Relaxed);
            }
        }
        thread::yield_now();
    }
}

fn writer_thread(
    drive: char,
    file_count: usize,
    file_size: usize,
    stop: Arc<AtomicBool>,
    stats: Arc<Stats>,
    _tid: usize,
) {
    let mut rng = rand::rng();
    let mut data = vec![0u8; file_size];

    while !stop.load(Ordering::Relaxed) {
        let idx = rng.random_range(0..file_count);
        let path = create_path(drive, &format!("{}{}", TEST_FILE_PREFIX, idx));

        rng.fill(&mut data[4..]);
        let data_crc = simple_crc32(&data[4..]);
        data[..4].copy_from_slice(&data_crc.to_le_bytes());

        if write_file(&path, &data).is_err() {
            stats.io_errors.fetch_add(1, Ordering::Relaxed);
        } else {
            stats.writes.fetch_add(1, Ordering::Relaxed);
            stats
                .write_bytes
                .fetch_add(data.len() as u64, Ordering::Relaxed);
        }
        thread::yield_now();
    }
}

fn cleanup_files(drive: char, count: usize) {
    for i in 0..count {
        let path = create_path(drive, &format!("{}{}", TEST_FILE_PREFIX, i));
        let _ = delete_file_safe(&path);
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let drive = args.get(1).and_then(|s| s.chars().next()).unwrap_or('G');
    let duration: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(10);
    let num_files: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(32);
    let file_size: usize = args
        .get(4)
        .and_then(|s| s.parse().ok())
        .unwrap_or(256 * 1024);
    let readers: usize = args.get(5).and_then(|s| s.parse().ok()).unwrap_or(4);
    let writers: usize = args.get(6).and_then(|s| s.parse().ok()).unwrap_or(2);

    println!("=== Nova Cache Stress Test ===");
    println!("Drive:      {}:", drive);
    println!("Duration:   {}s", duration);
    println!("Files:      {} x {} KB", num_files, file_size / 1024);
    println!("Readers:    {}", readers);
    println!("Writers:    {}", writers);
    println!();

    println!("Phase 1: Creating test files...");
    for idx in 0..num_files {
        let path = create_path(drive, &format!("{}{}", TEST_FILE_PREFIX, idx));
        if let Err(e) = write_test_file(&path, idx, file_size, 0xAB) {
            eprintln!("  Failed to create file {}: {}", idx, e);
            return Err(e.into());
        }
    }
    println!("  Created {} test files\n", num_files);

    let stats = Arc::new(Stats::new());
    let stop = Arc::new(AtomicBool::new(false));

    println!("Phase 2: Running stress test ({}s)...", duration);
    let start = Instant::now();
    let mut handles = Vec::new();

    for i in 0..readers {
        let s = stats.clone();
        let st = stop.clone();
        let fc = num_files;
        handles.push(thread::spawn(move || {
            reader_thread(drive, fc, file_size, st, s, i)
        }));
    }
    for i in 0..writers {
        let s = stats.clone();
        let st = stop.clone();
        let fc = num_files;
        handles.push(thread::spawn(move || {
            writer_thread(drive, fc, file_size, st, s, i)
        }));
    }

    while start.elapsed().as_secs() < duration {
        thread::sleep(Duration::from_secs(1));
        let elapsed = start.elapsed().as_secs_f64();
        let r = stats.reads.load(Ordering::Relaxed);
        let w = stats.writes.load(Ordering::Relaxed);
        let rb = stats.read_bytes.load(Ordering::Relaxed);
        let wb = stats.write_bytes.load(Ordering::Relaxed);
        let ie = stats.integrity_errors.load(Ordering::Relaxed);
        print!("\r  [{:>5.1}s] reads={:>6} ({:>5.1} MB/s) writes={:>6} ({:>5.1} MB/s) integrity_errs={}",
            elapsed, r, rb as f64 / elapsed / 1048576.0,
            w, wb as f64 / elapsed / 1048576.0, ie);
        std::io::stdout().flush().ok();
    }

    stop.store(true, Ordering::Relaxed);
    println!("\n");
    for h in handles {
        h.join().ok();
    }

    let elapsed = start.elapsed().as_secs_f64();
    let r = stats.reads.load(Ordering::Relaxed);
    let w = stats.writes.load(Ordering::Relaxed);
    let rb = stats.read_bytes.load(Ordering::Relaxed);
    let wb = stats.write_bytes.load(Ordering::Relaxed);
    let ie = stats.integrity_errors.load(Ordering::Relaxed);
    let io_err = stats.io_errors.load(Ordering::Relaxed);

    println!("Phase 3: Results");
    println!(
        "  Total reads:     {} ({:.1} MB/s)",
        r,
        rb as f64 / elapsed / 1048576.0
    );
    println!(
        "  Total writes:    {} ({:.1} MB/s)",
        w,
        wb as f64 / elapsed / 1048576.0
    );
    println!("  IO errors:       {}", io_err);
    println!(
        "  Integrity errors (concurrent): {} (expected — writer overwrites mid-read)",
        ie
    );

    println!("\nPhase 4: Sequential integrity verification (write-then-read, no concurrency)...");
    cleanup_files(drive, num_files);

    let mut write_errors = 0u64;
    for idx in 0..num_files {
        let path = create_path(drive, &format!("{}{}", TEST_FILE_PREFIX, idx));
        if write_test_file(&path, idx, file_size, 0xAB).is_err() {
            write_errors += 1;
        }
    }
    println!("  Wrote {} files ({} errors)", num_files, write_errors);

    thread::sleep(Duration::from_millis(100));

    let mut verified = 0usize;
    let mut read_errors = 0u64;
    for idx in 0..num_files {
        let path = create_path(drive, &format!("{}{}", TEST_FILE_PREFIX, idx));
        match verify_file(&path, file_size) {
            Ok(true) => {
                verified += 1;
            }
            Ok(false) => {
                read_errors += 1;
                eprintln!("  FAIL: file {} CRC mismatch", idx);
            }
            Err(e) => {
                read_errors += 1;
                eprintln!("  FAIL: file {} read error: {}", idx, e);
            }
        }
    }

    println!(
        "  Verified: {}/{} files ({} errors)",
        verified, num_files, read_errors
    );

    println!("\nPhase 5: Cleanup...");
    cleanup_files(drive, num_files);
    println!("  Deleted {} test files\n", num_files);

    if read_errors > 0 {
        println!(
            "*** FAIL: {} sequential integrity errors detected ***",
            read_errors
        );
        std::process::exit(1);
    } else {
        println!("*** PASS: All sequential data integrity checks passed ***");
    }

    Ok(())
}
