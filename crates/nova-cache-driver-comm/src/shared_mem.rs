use anyhow::{anyhow, Result};
use std::ptr;
use std::sync::atomic::Ordering;
use std::sync::Mutex;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Memory::{
    CreateFileMappingW, FlushViewOfFile, MapViewOfFile, UnmapViewOfFile, FILE_MAP_ALL_ACCESS,
    MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
};
use windows::Win32::System::Threading::CreateEventW;

#[repr(C)]
#[derive(Debug)]
pub struct SharedMemHeader {
    pub head: u64,
    pub tail: u64,
    pub capacity: u32,
    pub block_size: u32,
    pub volume_bitmap: u32,
    pub ring_capacity: u32,
    pub perf_counter_freq: u64,
    pub cached_hits: u64,
    pub cached_reads_total: u64,
    pub cached_writes_total: u64,
    pub write_back_enabled: u32,
    pub dirty_count: u32,
    pub l2_capacity: u32,
    pub reserved: [u32; 1],
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Default)]
pub struct SharedMemBlockDesc {
    pub sequence_num: u64,
    pub volume_id: u32,
    pub flags: u32,
    pub offset: u64,
    pub length: u32,
    pub status: u32,
    pub pre_op_tick: u64,
    pub post_op_tick: u64,
    pub file_object: u64,
    pub crc32: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Default)]
pub struct CacheDirectoryEntry {
    pub offset: u64,
    pub volume_id: u32,
    pub slot_index: u32,
    pub valid: u32,
    pub length: u32,
    pub sequence_num: u64,
    pub file_object: usize,
}

pub struct SharedMemoryRing {
    section_handle: HANDLE,
    view_ptr: *mut std::ffi::c_void,
    ring_capacity: usize,
    cache_capacity: usize,
    l2_capacity: usize,
    block_size: usize,
    data_event: HANDLE,
    event_name: String,
    pop_lock: Mutex<()>,
}

unsafe impl Send for SharedMemoryRing {}
unsafe impl Sync for SharedMemoryRing {}

impl SharedMemoryRing {
    pub fn create(
        name: &str,
        ring_capacity: usize,
        cache_capacity: usize,
        l2_capacity: usize,
        block_size: usize,
    ) -> Result<Self> {
        let header_size = std::mem::size_of::<SharedMemHeader>();
        let desc_size = std::mem::size_of::<SharedMemBlockDesc>();
        let entry_size = std::mem::size_of::<CacheDirectoryEntry>();

        // Extended size layout:
        // Header + Ring Descs + Ring Data + Cache Directory + L2 Cache Directory + Cache Data
        // Ring and cache have independent capacities
        let total_size = header_size
            + (ring_capacity * desc_size)
            + (ring_capacity * block_size)
            + (cache_capacity * entry_size)
            + (l2_capacity * entry_size)
            + (cache_capacity * block_size);

        // Use a unique name per process to prevent stale section reuse.
        // When CreateFileMappingW finds an existing section with the same name,
        // it opens the old one — causing stale Valid=1 entries in the cache directory.
        // A per-PID name guarantees a fresh section every time.
        let session_name = format!("{}_{}", name, std::process::id());
        let name_wide: Vec<u16> = session_name.encode_utf16().chain(Some(0)).collect();

        // Create file mapping
        let section_handle = unsafe {
            CreateFileMappingW(
                HANDLE::default(),
                None,
                PAGE_READWRITE,
                (total_size >> 32) as u32,
                (total_size & 0xFFFFFFFF) as u32,
                PCWSTR(name_wide.as_ptr()),
            )?
        };

        // Map view
        let view_ptr =
            unsafe { MapViewOfFile(section_handle, FILE_MAP_ALL_ACCESS, 0, 0, total_size) };

        if view_ptr.Value.is_null() {
            unsafe {
                let _ = CloseHandle(section_handle);
            }
            return Err(anyhow!("Failed to map view of file mapping"));
        }

        // Initialize header
        let header = view_ptr.Value as *mut SharedMemHeader;
        unsafe {
            ptr::write(
                header,
                SharedMemHeader {
                    head: 0,
                    tail: 0,
                    capacity: cache_capacity as u32,
                    block_size: block_size as u32,
                    volume_bitmap: 0,
                    ring_capacity: ring_capacity as u32,
                    perf_counter_freq: 0,
                    cached_hits: 0,
                    cached_reads_total: 0,
                    cached_writes_total: 0,
                    write_back_enabled: 0,
                    dirty_count: 0,
                    l2_capacity: l2_capacity as u32,
                    reserved: [0; 1],
                },
            );
        }

        // Initialize Cache Directory to zeros (valid = 0)
        let ring_data_offset = header_size + (ring_capacity * desc_size);
        let cache_directory_offset = ring_data_offset + (ring_capacity * block_size);
        let directory_ptr =
            (view_ptr.Value as usize + cache_directory_offset) as *mut CacheDirectoryEntry;
        
        let l2_directory_offset = cache_directory_offset + (cache_capacity * entry_size);
        let l2_directory_ptr = 
            (view_ptr.Value as usize + l2_directory_offset) as *mut CacheDirectoryEntry;
            
        unsafe {
            ptr::write_bytes(directory_ptr, 0, cache_capacity);
            ptr::write_bytes(l2_directory_ptr, 0, l2_capacity);
        }

        // Also zero all ring descriptors and ring data to prevent stale entries
        let descs_ptr = (view_ptr.Value as usize + header_size) as *mut u8;
        let descs_size = ring_capacity * desc_size;
        let ring_data_size = ring_capacity * block_size;
        unsafe {
            ptr::write_bytes(descs_ptr, 0, descs_size + ring_data_size);
        }

        // Flush the entire view to ensure all zeros are committed to the section.
        // Without this, CreateFileMappingW may return cached pages from a previous session.
        unsafe {
            let _ = FlushViewOfFile(view_ptr.Value as *const std::ffi::c_void, total_size);
        }

        // Create data ready notification event (per-session name)
        let event_name = format!("{}_Event", session_name);
        let event_name_wide: Vec<u16> = event_name.encode_utf16().chain(Some(0)).collect();
        let data_event = unsafe {
            CreateEventW(
                None,
                false, // auto reset
                false, // initial state non-signaled
                PCWSTR(event_name_wide.as_ptr()),
            )?
        };

        Ok(Self {
            section_handle,
            view_ptr: view_ptr.Value,
            ring_capacity,
            cache_capacity,
            l2_capacity,
            block_size,
            data_event,
            event_name,
            pop_lock: Mutex::new(()),
        })
    }

    pub fn get_event_name(&self) -> &str {
        &self.event_name
    }

    pub fn get_data_event(&self) -> HANDLE {
        self.data_event
    }

    pub fn get_section_handle(&self) -> HANDLE {
        self.section_handle
    }

    pub fn set_volume_bitmap(&self, bitmap: u32) {
        let header = self.view_ptr as *mut SharedMemHeader;
        unsafe {
            ptr::write_volatile(&mut (*header).volume_bitmap, bitmap);
        }
    }

    pub fn get_volume_bitmap(&self) -> u32 {
        let header = self.view_ptr as *const SharedMemHeader;
        unsafe { ptr::read_volatile(&(*header).volume_bitmap) }
    }

    pub fn get_perf_counter_freq(&self) -> u64 {
        let header = self.view_ptr as *const SharedMemHeader;
        unsafe { ptr::read_volatile(&(*header).perf_counter_freq) }
    }

    pub fn get_driver_counters(&self) -> (u64, u64, u64) {
        let header = self.view_ptr as *const SharedMemHeader;
        unsafe {
            let hits = ptr::read_volatile(&(*header).cached_hits);
            let reads = ptr::read_volatile(&(*header).cached_reads_total);
            let writes = ptr::read_volatile(&(*header).cached_writes_total);
            (hits, reads, writes)
        }
    }

    pub fn get_debug_counter(&self) -> u32 {
        let header = self.view_ptr as *const SharedMemHeader;
        unsafe { ptr::read_volatile(&(*header).reserved[0]) }
    }

    pub fn set_write_back_enabled(&self, enabled: bool) {
        let header = self.view_ptr as *mut SharedMemHeader;
        unsafe {
            ptr::write_volatile(
                &mut (*header).write_back_enabled,
                if enabled { 1 } else { 0 },
            );
        }
    }

    pub fn get_write_back_enabled(&self) -> bool {
        let header = self.view_ptr as *const SharedMemHeader;
        unsafe { ptr::read_volatile(&(*header).write_back_enabled) != 0 }
    }

    pub fn set_dirty_count(&self, count: u32) {
        let header = self.view_ptr as *mut SharedMemHeader;
        unsafe {
            ptr::write_volatile(&mut (*header).dirty_count, count);
        }
    }

    pub fn get_dirty_count(&self) -> u32 {
        let header = self.view_ptr as *const SharedMemHeader;
        unsafe { ptr::read_volatile(&(*header).dirty_count) }
    }

    pub fn pop(&self) -> Option<(SharedMemBlockDesc, Vec<u8>)> {
        let _guard = self.pop_lock.lock().unwrap_or_else(|e| e.into_inner());

        let header = self.view_ptr as *mut SharedMemHeader;

        // Read tail and head
        let head = unsafe { ptr::read_volatile(&((*header).head)) };
        let tail = unsafe { ptr::read_volatile(&((*header).tail)) };

        if tail >= head {
            return None;
        }

        let idx = (tail % self.ring_capacity as u64) as usize;

        // Get descriptor
        let header_size = std::mem::size_of::<SharedMemHeader>();
        let desc_size = std::mem::size_of::<SharedMemBlockDesc>();
        let descs_ptr = (self.view_ptr as usize + header_size) as *const SharedMemBlockDesc;
        let desc = unsafe { ptr::read(descs_ptr.add(idx)) };

        // Get data
        let data_start_ptr =
            (self.view_ptr as usize + header_size + self.ring_capacity * desc_size) as *const u8;
        let block_ptr = unsafe { data_start_ptr.add(idx * self.block_size) };

        // Validate length against block size to prevent out-of-bounds read
        let len = (desc.length as usize).min(self.block_size);
        let mut data = vec![0u8; len];
        unsafe {
            ptr::copy_nonoverlapping(block_ptr, data.as_mut_ptr(), len);
        }

        // Increment tail
        unsafe {
            ptr::write_volatile(&mut ((*header).tail), tail + 1);
        }

        Some((desc, data))
    }

    pub fn insert_l1_cache(
        &self,
        volume_id: u32,
        offset: u64,
        data: &[u8],
        crc32: u32, // unused, kept for API compatibility for now
        file_object: usize,
    ) {
        if data.len() > self.block_size || file_object == 0 {
            return;
        }

        let header_size = std::mem::size_of::<SharedMemHeader>();
        let desc_size = std::mem::size_of::<SharedMemBlockDesc>();
        let entry_size = std::mem::size_of::<CacheDirectoryEntry>();

        let ring_data_offset = header_size + (self.ring_capacity * desc_size);
        let cache_directory_offset = ring_data_offset + (self.ring_capacity * self.block_size);
        let l2_directory_offset = cache_directory_offset + (self.cache_capacity * entry_size);
        let cache_data_offset = l2_directory_offset + (self.l2_capacity * entry_size);

        let directory_ptr =
            (self.view_ptr as usize + cache_directory_offset) as *mut CacheDirectoryEntry;
        let data_start_ptr = (self.view_ptr as usize + cache_data_offset) as *mut u8;

        // 2-way set associative: capacity/2 buckets, 2 slots each
        let num_buckets = self.cache_capacity / 2;
        if num_buckets == 0 {
            return;
        }

        // Bucket is based on the 64KB block boundary
        let bucket = ((offset / self.block_size as u64) % num_buckets as u64) as usize;
        let chunk_data_offset = (offset % self.block_size as u64) as usize;
        let slot_a = bucket * 2;
        let slot_b = bucket * 2 + 1;
        let file_object = file_object as u64;

        unsafe {
            // Check if either slot already holds THIS 64KB block for this file
            let entry_a = &mut *directory_ptr.add(slot_a);
            let entry_b = &mut *directory_ptr.add(slot_b);

            let target_slot = if entry_a.valid == 1
                && entry_a.volume_id == volume_id
                && (entry_a.offset / self.block_size as u64) == (offset / self.block_size as u64)
                && entry_a.file_object as u64 == file_object
            {
                Some(slot_a)
            } else if entry_b.valid == 1
                && entry_b.volume_id == volume_id
                && (entry_b.offset / self.block_size as u64) == (offset / self.block_size as u64)
                && entry_b.file_object as u64 == file_object
            {
                Some(slot_b)
            } else {
                None
            };

            let slot = match target_slot {
                Some(s) => s,
                None => {
                    if entry_a.valid == 0 {
                        slot_a
                    } else if entry_b.valid == 0 {
                        slot_b
                    } else {
                        slot_a // evict oldest (simple)
                    }
                }
            };

            // 1. Invalidate slot first (prevents driver from reading partial data)
            let entry_ptr = directory_ptr.add(slot);
            ptr::write_volatile(&mut (*entry_ptr).valid, 0);
            std::sync::atomic::fence(Ordering::Release);

            // 2. Write metadata (store EXACT offset and EXACT length)
            ptr::write_volatile(&mut (*entry_ptr).offset, offset);
            ptr::write_volatile(&mut (*entry_ptr).volume_id, volume_id);
            ptr::write_volatile(&mut (*entry_ptr).slot_index, slot as u32);
            ptr::write_volatile(&mut (*entry_ptr).length, data.len() as u32);
            ptr::write_volatile(&mut (*entry_ptr).file_object, file_object as usize);

            // 3. Write data to cache data slot AT THE EXACT CHUNK OFFSET
            let slot_ptr = data_start_ptr.add(slot * self.block_size);
            ptr::copy_nonoverlapping(data.as_ptr(), slot_ptr.add(chunk_data_offset), data.len());
            // No zero-filling because we are only setting Valid=1 for the EXACT range!

            // 4. Increment generation counter, then mark valid
            let new_seq = (*entry_ptr).sequence_num.wrapping_add(1);
            ptr::write_volatile(&mut (*entry_ptr).sequence_num, new_seq);
            std::sync::atomic::fence(Ordering::Release);
            ptr::write_volatile(&mut (*entry_ptr).valid, 1);
        }
    }

    pub fn invalidate_l1_cache(&self, volume_id: u32, offset: u64, file_object: usize) {
        let header_size = std::mem::size_of::<SharedMemHeader>();
        let desc_size = std::mem::size_of::<SharedMemBlockDesc>();
        let entry_size = std::mem::size_of::<CacheDirectoryEntry>();

        let ring_data_offset = header_size + (self.ring_capacity * desc_size);
        let cache_directory_offset = ring_data_offset + (self.ring_capacity * self.block_size);
        let directory_ptr =
            (self.view_ptr as usize + cache_directory_offset) as *mut CacheDirectoryEntry;

        let num_buckets = self.cache_capacity / 2;
        if num_buckets == 0 {
            return;
        }

        let bucket = ((offset / self.block_size as u64) % num_buckets as u64) as usize;
        let slot_a = bucket * 2;
        let slot_b = bucket * 2 + 1;
        let file_object = file_object as u64;

        unsafe {
            for &slot in &[slot_a, slot_b] {
                let entry_ptr = directory_ptr.add(slot);
                if (*entry_ptr).valid == 1
                    && (*entry_ptr).volume_id == volume_id
                    && (*entry_ptr).offset == offset
                    && (*entry_ptr).file_object as u64 == file_object
                {
                    ptr::write_volatile(&mut (*entry_ptr).valid, 0);
                    std::sync::atomic::fence(Ordering::Release);
                }
            }
        }
    }

    pub fn insert_l2_cache(
        &self,
        volume_id: u32,
        offset: u64,
        crc32: u32,
        file_object: usize,
        l2_slot: u32,
        valid: bool,
    ) {
        if self.l2_capacity == 0 {
            return;
        }

        let header_size = std::mem::size_of::<SharedMemHeader>();
        let desc_size = std::mem::size_of::<SharedMemBlockDesc>();
        let entry_size = std::mem::size_of::<CacheDirectoryEntry>();

        // Safe checked offsets calculation
        let ring_data_offset = match self.ring_capacity.checked_mul(desc_size) {
            Some(val) => header_size + val,
            None => {
                tracing::error!("Overflow calculating ring_data_offset");
                return;
            }
        };
        let cache_directory_offset = match self.ring_capacity.checked_mul(self.block_size) {
            Some(val) => ring_data_offset + val,
            None => {
                tracing::error!("Overflow calculating cache_directory_offset");
                return;
            }
        };
        let l2_directory_offset = match self.cache_capacity.checked_mul(entry_size) {
            Some(val) => cache_directory_offset + val,
            None => {
                tracing::error!("Overflow calculating l2_directory_offset");
                return;
            }
        };

        let l2_directory_ptr =
            (self.view_ptr as usize + l2_directory_offset) as *mut CacheDirectoryEntry;

        let num_buckets = self.l2_capacity / 4;
        if num_buckets == 0 {
            return;
        }

        if self.block_size == 0 {
            tracing::error!("block_size is zero in insert_l2_cache");
            return;
        }

        let bucket = ((offset / self.block_size as u64) % num_buckets as u64) as usize;
        let base_slot = match bucket.checked_mul(4) {
            Some(val) => val,
            None => {
                tracing::error!("Overflow calculating base_slot: bucket={}", bucket);
                return;
            }
        };

        unsafe {
            // If invalidating, we must find the entry that has this l2_slot and invalidate it
            if !valid {
                for i in 0..4 {
                    let idx = match base_slot.checked_add(i) {
                        Some(val) => val,
                        None => continue,
                    };
                    if idx >= self.l2_capacity {
                        tracing::error!("idx {} out of bounds for l2_capacity {}", idx, self.l2_capacity);
                        continue;
                    }
                    let entry = &mut *l2_directory_ptr.add(idx);
                    if entry.valid == 1
                        && entry.volume_id == volume_id
                        && entry.offset == offset
                        && entry.file_object as usize == file_object
                    {
                        ptr::write_volatile(&mut (*entry).valid, 0);
                        std::sync::atomic::fence(Ordering::Release);
                        break;
                    }
                }
                return;
            }

            let mut target_idx = base_slot;
            let mut found_empty = false;

            // Try to find exact match or empty slot
            for i in 0..4 {
                let idx = match base_slot.checked_add(i) {
                    Some(val) => val,
                    None => continue,
                };
                if idx >= self.l2_capacity {
                    tracing::error!("idx {} out of bounds for l2_capacity {}", idx, self.l2_capacity);
                    continue;
                }
                let entry = &mut *l2_directory_ptr.add(idx);
                if entry.valid == 1
                    && entry.volume_id == volume_id
                    && entry.offset == offset
                    && entry.file_object as usize == file_object
                {
                    target_idx = idx;
                    found_empty = true;
                    break;
                } else if entry.valid == 0 {
                    target_idx = idx;
                    found_empty = true;
                }
            }

            // If no empty slot and no match, evict the oldest (LRU approximation by sequence_num)
            if !found_empty {
                let mut min_seq = u64::MAX;
                for i in 0..4 {
                    let idx = match base_slot.checked_add(i) {
                        Some(val) => val,
                        None => continue,
                    };
                    if idx >= self.l2_capacity {
                        tracing::error!("idx {} out of bounds for l2_capacity {}", idx, self.l2_capacity);
                        continue;
                    }
                    let entry = &*l2_directory_ptr.add(idx);
                    if entry.sequence_num < min_seq {
                        min_seq = entry.sequence_num;
                        target_idx = idx;
                    }
                }
            }

            if target_idx >= self.l2_capacity {
                tracing::error!("target_idx {} out of bounds for l2_capacity {}", target_idx, self.l2_capacity);
                return;
            }
            let entry_ptr = l2_directory_ptr.add(target_idx);

            // Invalidate first
            ptr::write_volatile(&mut (*entry_ptr).valid, 0);
            std::sync::atomic::fence(Ordering::Release);

            // Write metadata
            ptr::write_volatile(&mut (*entry_ptr).offset, offset);
            ptr::write_volatile(&mut (*entry_ptr).volume_id, volume_id);
            ptr::write_volatile(&mut (*entry_ptr).slot_index, l2_slot);
            ptr::write_volatile(&mut (*entry_ptr).length, 65536);
            ptr::write_volatile(&mut (*entry_ptr).file_object, file_object as usize);

            // Increment generation counter, then mark valid
            let new_seq = (*entry_ptr).sequence_num.wrapping_add(1);
            ptr::write_volatile(&mut (*entry_ptr).sequence_num, new_seq);
            std::sync::atomic::fence(Ordering::Release);
            ptr::write_volatile(&mut (*entry_ptr).valid, 1);
        }
    }
}

impl Drop for SharedMemoryRing {
    fn drop(&mut self) {
        unsafe {
            let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.view_ptr,
            });
            let _ = CloseHandle(self.section_handle);
            let _ = CloseHandle(self.data_event);
        }
    }
}
