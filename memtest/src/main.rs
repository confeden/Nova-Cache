use std::time::{Duration, Instant};
use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_BEGIN, FILE_FLAG_NO_BUFFERING,
    FILE_FLAG_WRITE_THROUGH, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_ALWAYS, ReadFile, SetFilePointerEx, WriteFile,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::core::PCWSTR;

const IOCTL_STORAGE_QUERY_PROPERTY: u32 = 0x2D1400;

#[repr(align(4096))]
struct AlignedBuffer {
    data: [u8; 4096],
}

struct DriveInfo {
    letter: char,
    label: String,
    fs_type: String,
    drive_type: String,
    total_gb: f64,
}

fn enumerate_drives() -> Vec<DriveInfo> {
    let mut drives = Vec::new();
    unsafe {
        let mask = windows::Win32::Storage::FileSystem::GetLogicalDrives();
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

                let vol_ok = windows::Win32::Storage::FileSystem::GetVolumeInformationW(
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
                let _ = windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
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
                    drive_type,
                    total_gb: total_bytes as f64 / (1024.0 * 1024.0 * 1024.0),
                });
            }
        }
    }
    drives
}

fn get_drive_type_label(letter: char) -> String {
    let root = format!("{}:\\", letter);
    let root_wide: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();
    let dt =
        unsafe { windows::Win32::Storage::FileSystem::GetDriveTypeW(PCWSTR(root_wide.as_ptr())) };
    match dt {
        2 => "USB".into(),
        5 => "CD/DVD".into(),
        3 => query_bus_type(letter),
        _ => "Unknown".into(),
    }
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
            windows::Win32::Storage::FileSystem::OPEN_EXISTING,
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
            Some(query.as_ptr() as *const std::ffi::c_void),
            8,
            Some(descriptor.as_mut_ptr() as *mut std::ffi::c_void),
            descriptor.len() as u32,
            Some(&mut bytes_returned),
            None,
        );
        let _ = CloseHandle(handle);
        if ok.is_ok() && bytes_returned >= 32 {
            match descriptor[28] {
                0x11 => "NVMe".into(),
                0x0B => "SATA".into(),
                0x0A => "SAS".into(),
                _ => "HDD".into(),
            }
        } else {
            "Unknown".into()
        }
    }
}

fn seek_and_write(handle: HANDLE, offset: u64, data: &[u8]) -> bool {
    unsafe {
        let mut pos: i64 = 0;
        let _ = SetFilePointerEx(handle, offset as i64, Some(&mut pos), FILE_BEGIN);
        let mut written = 0u32;
        WriteFile(handle, Some(data), Some(&mut written), None).is_ok()
            && written == data.len() as u32
    }
}

fn seek_and_read(handle: HANDLE, offset: u64, buf: &mut [u8]) -> bool {
    unsafe {
        let mut pos: i64 = 0;
        let _ = SetFilePointerEx(handle, offset as i64, Some(&mut pos), FILE_BEGIN);
        let mut bytes_read = 0u32;
        ReadFile(handle, Some(buf), Some(&mut bytes_read), None).is_ok()
            && bytes_read == buf.len() as u32
    }
}

fn open_raw(path: &str, for_write: bool) -> Option<HANDLE> {
    let path_wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let access = if for_write {
        GENERIC_WRITE.0 | GENERIC_READ.0
    } else {
        GENERIC_READ.0
    };
    unsafe {
        match CreateFileW(
            PCWSTR(path_wide.as_ptr()),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_ALWAYS,
            FILE_FLAGS_AND_ATTRIBUTES(FILE_FLAG_NO_BUFFERING.0 | FILE_FLAG_WRITE_THROUGH.0),
            HANDLE::default(),
        ) {
            Ok(h) if !h.is_invalid() => Some(h),
            _ => None,
        }
    }
}

fn format_gb(gb: f64) -> String {
    if gb >= 1024.0 {
        format!("{:.1} TB", gb / 1024.0)
    } else {
        format!("{:.0} GB", gb)
    }
}

fn format_duration(d: Duration) -> String {
    if d.as_millis() < 1 {
        format!("{} us", d.as_micros())
    } else if d.as_secs_f64() < 1.0 {
        format!("{:.1} ms", d.as_secs_f64() * 1000.0)
    } else {
        format!("{:.2} s", d.as_secs_f64())
    }
}

fn query_cache_stats() {
    println!("\n--- Cache Stats ---");
    use std::io::{BufRead, BufReader, Write};
    match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(r"\\.\pipe\NovaCacheIpc")
    {
        Ok(pipe) => {
            let mut pipe = pipe;
            let req = serde_json::json!({ "type": "get_stats" });
            let mut req_str = serde_json::to_string(&req).unwrap();
            req_str.push('\n');
            if pipe.write_all(req_str.as_bytes()).is_ok() {
                let mut reader = BufReader::new(pipe);
                let mut line = String::new();
                if reader.read_line(&mut line).is_ok() {
                    if let Ok(val) = serde_json::from_str::<serde_json::Value>(&line) {
                        let data = val.get("data").cloned().unwrap_or_default();
                        let hits_t1 = data["hits_t1"].as_u64().unwrap_or(0);
                        let hits_t2 = data["hits_t2"].as_u64().unwrap_or(0);
                        let misses = data["misses"].as_u64().unwrap_or(0);
                        let evictions = data["evictions"].as_u64().unwrap_or(0);
                        let total = hits_t1 + hits_t2 + misses;
                        let rate = if total > 0 {
                            (hits_t1 + hits_t2) as f64 / total as f64 * 100.0
                        } else {
                            0.0
                        };
                        println!(
                            "  L1 hits: {} (T1) + {} (T2) = {}",
                            hits_t1,
                            hits_t2,
                            hits_t1 + hits_t2
                        );
                        println!("  Misses: {}, Evictions: {}", misses, evictions);
                        println!("  Hit rate: {:.2}% ({}/{})", rate, hits_t1 + hits_t2, total);
                    }
                }
            }
        }
        Err(e) => println!("  Could not connect to service: {:?}", e),
    }
}

fn query_l2_backends() {
    println!("\n--- L2 Backends ---");
    use std::io::{BufRead, BufReader, Write};
    match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(r"\\.\pipe\NovaCacheIpc")
    {
        Ok(pipe) => {
            let mut pipe = pipe;
            let req = serde_json::json!({ "type": "get_l2_backends" });
            let mut req_str = serde_json::to_string(&req).unwrap();
            req_str.push('\n');
            if pipe.write_all(req_str.as_bytes()).is_ok() {
                let mut reader = BufReader::new(pipe);
                let mut line = String::new();
                if reader.read_line(&mut line).is_ok() {
                    if let Ok(val) = serde_json::from_str::<serde_json::Value>(&line) {
                        let data = val.get("data").cloned().unwrap_or_default();
                        let backends = data["backends"].as_array().cloned().unwrap_or_default();
                        let total_free = data["total_free"].as_u64().unwrap_or(0);
                        let total_slots = data["total_slots"].as_u64().unwrap_or(0);
                        println!(
                            "  {} backend(s), {}/{} slots free",
                            backends.len(),
                            total_free,
                            total_slots
                        );
                        for (i, b) in backends.iter().enumerate() {
                            let path = b["path"].as_str().unwrap_or("?");
                            let speed = b["speed_mbps"].as_f64().unwrap_or(0.0);
                            let free = b["free_slots"].as_u64().unwrap_or(0);
                            let total = b["total_slots"].as_u64().unwrap_or(0);
                            println!("  [{}] {} {:.1} MB/s ({}/{})", i, path, speed, free, total);
                        }
                    }
                }
            }
        }
        Err(_) => println!("  Could not connect to service."),
    }
}

fn test_sequential_read(handle: HANDLE, file_size: u64) -> (Duration, f64) {
    let block_size = 65536u64;
    let num_blocks = (file_size / block_size) as usize;
    let mut buf = vec![0u8; block_size as usize];
    let mut count = 0usize;

    let start = Instant::now();
    for i in 0..num_blocks {
        let offset = (i as u64) * block_size;
        if seek_and_read(handle, offset, &mut buf) {
            count += 1;
        } else {
            break;
        }
    }
    let elapsed = start.elapsed();
    let mb = (count as f64 * block_size as f64) / (1024.0 * 1024.0);
    let speed = if elapsed.as_secs_f64() > 0.0 {
        mb / elapsed.as_secs_f64()
    } else {
        0.0
    };
    (elapsed, speed)
}

fn test_random_read(handle: HANDLE, file_size: u64) -> (Duration, f64) {
    let block_size = 65536u64;
    let num_blocks = (file_size / block_size) as usize;
    if num_blocks == 0 {
        return (Duration::ZERO, 0.0);
    }
    let mut buf = vec![0u8; block_size as usize];
    let mut indices: Vec<usize> = (0..num_blocks).collect();
    let mut seed: u32 = 0xDEADBEEF;
    for i in (1..num_blocks).rev() {
        seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
        let j = (seed as usize) % (i + 1);
        indices.swap(i, j);
    }

    let mut count = 0usize;
    let start = Instant::now();
    for &idx in &indices {
        let offset = (idx as u64) * block_size;
        if seek_and_read(handle, offset, &mut buf) {
            count += 1;
        } else {
            break;
        }
    }
    let elapsed = start.elapsed();
    let mb = (count as f64 * block_size as f64) / (1024.0 * 1024.0);
    let speed = if elapsed.as_secs_f64() > 0.0 {
        mb / elapsed.as_secs_f64()
    } else {
        0.0
    };
    (elapsed, speed)
}

fn test_write_verify(handle: HANDLE, file_size: u64) -> (Duration, bool) {
    let block_size = 65536u64;
    let num_blocks = (file_size / block_size) as usize;
    let mut write_buf = vec![0u8; block_size as usize];
    let mut read_buf = vec![0u8; block_size as usize];
    let mut verified = true;

    let start = Instant::now();
    for i in 0..num_blocks {
        for (j, b) in write_buf.iter_mut().enumerate() {
            *b = ((i * 7 + j * 13 + 42) % 256) as u8;
        }
        let offset = (i as u64) * block_size;
        if !seek_and_write(handle, offset, &write_buf) {
            verified = false;
            break;
        }
    }

    for i in 0..num_blocks {
        let offset = (i as u64) * block_size;
        if !seek_and_read(handle, offset, &mut read_buf) {
            verified = false;
            break;
        }
        for (j, b) in write_buf.iter_mut().enumerate() {
            *b = ((i * 7 + j * 13 + 42) % 256) as u8;
        }
        if read_buf != write_buf {
            verified = false;
            break;
        }
    }
    let elapsed = start.elapsed();
    (elapsed, verified)
}

fn test_mixed_io(handle: HANDLE, file_size: u64) -> Duration {
    let block_size = 65536u64;
    let num_blocks = (file_size / block_size) as usize;
    if num_blocks == 0 {
        return Duration::ZERO;
    }
    let mut buf = vec![0u8; block_size as usize];
    let mut seed: u32 = 0xCAFEBABE;

    let start = Instant::now();
    for _ in 0..num_blocks {
        seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
        let block_idx = (seed % num_blocks as u32) as usize;
        let offset = (block_idx as u64) * block_size;
        if seed % 3 == 0 {
            for b in buf.iter_mut() {
                *b = (seed % 256) as u8;
            }
            seek_and_write(handle, offset, &buf);
        } else {
            seek_and_read(handle, offset, &mut buf);
        }
    }
    start.elapsed()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("============================================");
    println!("  Nova Cache - Extended Multi-Drive Test");
    println!("============================================\n");

    let drives = enumerate_drives();
    println!("Found {} drives:\n", drives.len());
    for d in &drives {
        println!(
            "  {}:\\ [{}] {} {} ({})",
            d.letter,
            &d.drive_type,
            if d.label.is_empty() {
                "(no label)"
            } else {
                &d.label
            },
            format_gb(d.total_gb),
            &d.fs_type,
        );
    }
    println!();

    let test_size: u64 = 8 * 1024 * 1024;
    let mut results: Vec<(char, String, bool, String)> = Vec::new();

    for d in &drives {
        let test_path = format!("{}:\\nova_cache_test.bin", d.letter);
        println!("=== Drive {}:\\ [{}] ===", d.letter, d.drive_type);

        let handle_w = match open_raw(&test_path, true) {
            Some(h) => h,
            None => {
                println!("  SKIP: Cannot open for write\n");
                results.push((
                    d.letter,
                    d.drive_type.clone(),
                    false,
                    "SKIP: cannot open".into(),
                ));
                continue;
            }
        };

        print!("  Writing test file ({} MB)... ", test_size / (1024 * 1024));
        std::io::Write::flush(&mut std::io::stdout()).ok();

        let mut written_total: u64 = 0;
        let pattern_block = vec![0xABu8; 4096];
        while written_total < test_size {
            let chunk = (test_size - written_total).min(4096) as usize;
            if !seek_and_write(handle_w, written_total, &pattern_block[..chunk]) {
                break;
            }
            written_total += chunk as u64;
        }
        unsafe {
            let _ = CloseHandle(handle_w);
        }
        if written_total < test_size {
            println!("PARTIAL (wrote {}/{} bytes)\n", written_total, test_size);
            results.push((
                d.letter,
                d.drive_type.clone(),
                false,
                "PARTIAL write".into(),
            ));
            continue;
        }
        println!("OK");

        let handle_r = match open_raw(&test_path, false) {
            Some(h) => h,
            None => {
                println!("  Cannot open for read\n");
                results.push((
                    d.letter,
                    d.drive_type.clone(),
                    false,
                    "SKIP: cannot read".into(),
                ));
                continue;
            }
        };

        print!("  [1/5] Sequential read: ");
        std::io::Write::flush(&mut std::io::stdout()).ok();
        let (t1, s1) = test_sequential_read(handle_r, test_size);
        println!("{} ({:.1} MB/s)", format_duration(t1), s1);

        print!("  [2/5] Wait 2s (cache populates)...");
        std::io::Write::flush(&mut std::io::stdout()).ok();
        std::thread::sleep(Duration::from_secs(2));
        println!(" done");

        print!("  [3/5] Cached sequential read: ");
        std::io::Write::flush(&mut std::io::stdout()).ok();
        let (t3, s3) = test_sequential_read(handle_r, test_size);
        let speedup = if t3.as_nanos() > 0 {
            t1.as_secs_f64() / t3.as_secs_f64()
        } else {
            0.0
        };
        println!("{} ({:.1} MB/s, {:.1}x)", format_duration(t3), s3, speedup);

        print!("  [4/5] Random read ({} blocks): ", test_size / 65536);
        std::io::Write::flush(&mut std::io::stdout()).ok();
        let (t4, s4) = test_random_read(handle_r, test_size);
        println!("{} ({:.1} MB/s)", format_duration(t4), s4);

        print!("  [5/5] Mixed I/O (read+write): ");
        std::io::Write::flush(&mut std::io::stdout()).ok();
        let t5 = test_mixed_io(handle_r, test_size);
        println!("{}", format_duration(t5));

        unsafe {
            let _ = CloseHandle(handle_r);
        }

        print!("  Write+verify test: ");
        std::io::Write::flush(&mut std::io::stdout()).ok();
        let handle_v = match open_raw(&test_path, true) {
            Some(h) => h,
            None => {
                println!("SKIP (cannot reopen)");
                results.push((
                    d.letter,
                    d.drive_type.clone(),
                    false,
                    "SKIP: cannot reopen".into(),
                ));
                continue;
            }
        };
        let (t6, ok) = test_write_verify(handle_v, test_size);
        unsafe {
            let _ = CloseHandle(handle_v);
        }
        println!(
            "{} - {}",
            format_duration(t6),
            if ok { "DATA OK" } else { "DATA CORRUPTED!" }
        );

        let status = if ok {
            "PASS".into()
        } else {
            "FAIL: DATA CORRUPTED".into()
        };
        results.push((d.letter, d.drive_type.clone(), ok, status));

        println!();
    }

    println!("Cleaning up test files...");
    for d in &drives {
        let path = format!("{}:\\nova_cache_test.bin", d.letter);
        let _ = std::fs::remove_file(&path);
    }

    println!("\n============================================");
    println!("  TEST SUMMARY");
    println!("============================================");
    let mut all_ok = true;
    for (letter, dtype, ok, status) in &results {
        let _icon = if *ok { "PASS" } else { "FAIL" };
        println!("  {}:\\ [{}] {}", letter, dtype, status);
        if !ok {
            all_ok = false;
        }
    }
    if all_ok {
        println!("\n  All drives: PASSED");
    } else {
        println!("\n  WARNING: Some drives have data corruption!");
        println!("  Run: chkdsk X:\\ /f /r to repair");
    }
    println!("============================================\n");

    query_cache_stats();
    query_l2_backends();

    Ok(())
}
