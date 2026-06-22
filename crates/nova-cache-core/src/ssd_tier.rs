use anyhow::{Context, Result};
use crossbeam::queue::ArrayQueue;
use memmap2::MmapMut;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tracing::warn;

pub struct SsdTier {
    /// Memory-mapped file for zero-copy reads/writes
    mmap: Option<Mutex<MmapMut>>,
    /// Fallback file I/O when memory-mapping is unavailable
    file: Option<Mutex<File>>,
    free_slots: Arc<ArrayQueue<usize>>,
    block_size: usize,
    total_slots: usize,
    healthy: AtomicBool,
}

impl SsdTier {
    pub fn new(path: &Path, size_bytes: u64, block_size: usize) -> Result<Self> {
        let total_slots = size_bytes as usize / block_size;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path)
            .context("Failed to open SSD cache file")?;

        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows::Win32::Foundation::HANDLE;
            use windows::Win32::System::Ioctl::FSCTL_SET_SPARSE;
            use windows::Win32::System::IO::DeviceIoControl;

            let handle = HANDLE(file.as_raw_handle() as _);
            unsafe {
                if let Err(e) =
                    DeviceIoControl(handle, FSCTL_SET_SPARSE, None, 0, None, 0, None, None)
                {
                    warn!(
                        "Could not set sparse flag on {}: {:?}. File will use real disk space.",
                        path.display(),
                        e
                    );
                }
            }
        }

        file.set_len(size_bytes)
            .context("Failed to pre-allocate SSD cache file")?;

        // SAFETY: The file will not be truncated while the mapping exists
        let (mmap, file_fallback) = match unsafe { MmapMut::map_mut(&file) } {
            Ok(m) => {
                drop(file);
                (Some(Mutex::new(m)), None)
            }
            Err(e) => {
                warn!(
                    "Failed to memory-map L2 cache file, falling back to file I/O: {:?}",
                    e
                );
                (None, Some(Mutex::new(file)))
            }
        };

        let free_slots = Arc::new(ArrayQueue::new(total_slots));
        for i in 0..total_slots {
            free_slots
                .push(i)
                .map_err(|_| anyhow::anyhow!("Queue full during initialization"))?;
        }

        Ok(Self {
            mmap,
            file: file_fallback,
            free_slots,
            block_size,
            total_slots,
            healthy: AtomicBool::new(true),
        })
    }

    #[inline]
    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Acquire)
    }

    pub fn mark_unhealthy(&self) {
        self.healthy.store(false, Ordering::Release);
        warn!("SSD cache tier marked as unhealthy — all L2 operations will be skipped");
    }

    pub fn allocate(&self) -> Option<usize> {
        if !self.is_healthy() {
            return None;
        }
        self.free_slots.pop()
    }

    pub fn free(&self, slot_id: usize) {
        if slot_id < self.total_slots {
            if self.free_slots.push(slot_id).is_err() {
                tracing::warn!(
                    "Failed to free SSD slot {} (queue full or double-free)",
                    slot_id
                );
            }
        }
    }

    pub fn read(&self, slot_id: usize, dst: &mut [u8]) -> Result<(), String> {
        if !self.is_healthy() {
            return Err("SSD cache tier is unhealthy (device removed or I/O error)".to_string());
        }
        if slot_id >= self.total_slots {
            return Err("Invalid slot ID".to_string());
        }
        if dst.len() > self.block_size {
            return Err("Destination buffer too large".to_string());
        }

        let offset = (slot_id * self.block_size) as usize;
        let len = dst.len();

        if let Some(mmap) = &self.mmap {
            let guard = mmap.lock().unwrap();
            dst.copy_from_slice(&guard[offset..offset + len]);
            Ok(())
        } else if let Some(file) = &self.file {
            let mut guard = file.lock().unwrap();
            match guard
                .seek(SeekFrom::Start(offset as u64))
                .and_then(|_| guard.read_exact(dst))
            {
                Ok(()) => Ok(()),
                Err(e) => {
                    self.mark_unhealthy();
                    Err(format!("SSD read failed: {:?}", e))
                }
            }
        } else {
            Err("No backing store available".to_string())
        }
    }

    pub fn write(&self, slot_id: usize, src: &[u8]) -> Result<(), String> {
        if !self.is_healthy() {
            return Err("SSD cache tier is unhealthy (device removed or I/O error)".to_string());
        }
        if slot_id >= self.total_slots {
            return Err("Invalid slot ID".to_string());
        }
        if src.len() > self.block_size {
            return Err("Source buffer too large".to_string());
        }

        let offset = (slot_id * self.block_size) as usize;
        let len = src.len();

        if let Some(mmap) = &self.mmap {
            let mut guard = mmap.lock().unwrap();
            guard[offset..offset + len].copy_from_slice(src);
            Ok(())
        } else if let Some(file) = &self.file {
            let mut guard = file.lock().unwrap();
            match guard
                .seek(SeekFrom::Start(offset as u64))
                .and_then(|_| guard.write_all(src))
            {
                Ok(()) => Ok(()),
                Err(e) => {
                    self.mark_unhealthy();
                    Err(format!("SSD write failed: {:?}", e))
                }
            }
        } else {
            Err("No backing store available".to_string())
        }
    }

    pub fn reserve_slots(&self, slots_to_reserve: &[usize]) {
        let reserve_set: std::collections::HashSet<usize> =
            slots_to_reserve.iter().copied().collect();
        let mut kept = Vec::new();
        let initial = self.free_slots.len();
        while let Some(s) = self.free_slots.pop() {
            if !reserve_set.contains(&s) {
                kept.push(s);
            }
        }
        let reserved_count = initial - kept.len();
        for s in kept {
            let _ = self.free_slots.push(s);
        }
        if reserved_count > 0 {
            tracing::info!("SsdTier: reserved {} slots in allocator", reserved_count);
        }
    }

    pub fn free_slots(&self) -> usize {
        self.free_slots.len()
    }

    pub fn block_size(&self) -> usize {
        self.block_size
    }

    pub fn total_slots(&self) -> usize {
        self.total_slots
    }

    pub fn flush(&self) -> std::io::Result<()> {
        if !self.is_healthy() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "SSD cache tier is unhealthy",
            ));
        }
        if let Some(mmap) = &self.mmap {
            mmap.lock().unwrap().flush()
        } else if let Some(file) = &self.file {
            file.lock().unwrap().sync_data()
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn test_ssd_tier_basic() -> Result<()> {
        let temp_dir = std::env::temp_dir();
        let path = temp_dir.join("ssd_test_basic.bin");
        let size_bytes = 1024 * 64; // 64KB
        let block_size = 1024; // 1KB per slot
        let ssd = SsdTier::new(&path, size_bytes, block_size)?;

        assert!(ssd.is_healthy());

        let slot = ssd.allocate().expect("Should allocate");
        let data = vec![0x42u8; block_size];
        ssd.write(slot, &data).map_err(|e| anyhow::anyhow!(e))?;

        let mut read_data = vec![0u8; block_size];
        ssd.read(slot, &mut read_data)
            .map_err(|e| anyhow::anyhow!(e))?;

        assert_eq!(data, read_data);
        ssd.free(slot);
        assert_eq!(ssd.free_slots(), 64);
        let _ = std::fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn test_ssd_tier_concurrent() -> Result<()> {
        let temp_dir = std::env::temp_dir();
        let path = temp_dir.join("ssd_test_concurrent.bin");
        let size_bytes = 1024 * 1024; // 1MB
        let block_size = 1024; // 1KB
        let ssd = Arc::new(SsdTier::new(&path, size_bytes, block_size)?);

        let mut handles = Vec::new();
        for _ in 0..10 {
            let ssd_clone = ssd.clone();
            handles.push(thread::spawn(move || {
                let slot = ssd_clone.allocate().expect("Failed to allocate");
                let data = vec![0xafu8; block_size];
                ssd_clone.write(slot, &data).expect("Write failed");

                let mut read_data = vec![0u8; block_size];
                ssd_clone.read(slot, &mut read_data).expect("Read failed");
                assert_eq!(data, read_data);

                ssd_clone.free(slot);
            }));
        }

        for handle in handles {
            handle.join().expect("Thread panicked");
        }

        assert_eq!(ssd.free_slots(), 1024);
        let _ = std::fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn test_unhealthy_tier_returns_errors() -> Result<()> {
        let temp_dir = std::env::temp_dir();
        let path = temp_dir.join("ssd_test_unhealthy.bin");
        let ssd = SsdTier::new(&path, 1024, 256)?;

        assert!(ssd.is_healthy());

        ssd.mark_unhealthy();
        assert!(!ssd.is_healthy());

        assert!(ssd.allocate().is_none());
        assert!(ssd.read(0, &mut vec![0u8; 256]).is_err());
        assert!(ssd.write(0, &vec![0u8; 256]).is_err());
        assert!(ssd.flush().is_err());

        let _ = std::fs::remove_file(path);
        Ok(())
    }
}
