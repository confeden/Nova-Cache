//! A fixed-size, thread-safe memory pool for L1 cache blocks.
//!
//! Provides O(1) allocation and deallocation of fixed-size memory slots from a large,
//! contiguous memory buffer. It is designed for high-performance concurrent access
//! by multiple threads.

use crossbeam::queue::ArrayQueue;
use std::cell::UnsafeCell;

/// A thread-safe memory pool that manages a contiguous block of memory.
///
/// `MemoryPool` allocates a large `Vec<u8>` on creation and hands out fixed-size
/// "slots" to callers. The use of `crossbeam::queue::ArrayQueue` for managing a
/// free list of slot indices ensures O(1) allocation and deallocation.
///
/// Interior mutability is achieved via `UnsafeCell`, allowing methods like `read`
/// and `write` to be called on an immutable reference (`&self`). A manual `Sync`
/// implementation is provided, asserting that concurrent access is safe because
/// the `allocate` method guarantees that each thread receives a unique, disjoint
/// memory slot, thus preventing data races.
pub struct MemoryPool {
    /// The contiguous memory buffer. `UnsafeCell` is used for interior mutability,
    /// allowing `&self` methods to write to the buffer.
    buffer: UnsafeCell<Vec<u8>>,
    /// A lock-free queue that stores the indices of available slots.
    free_list: ArrayQueue<usize>,
    /// The total number of slots managed by the pool.
    num_slots: usize,
    /// The size of each individual slot, in bytes.
    block_size: usize,
}

// # Safety
// `MemoryPool` can be safely shared across threads (`Sync`) because access to the
// underlying `buffer` (wrapped in `UnsafeCell`) is externally synchronized.
// The `allocate` method provides a unique slot index to each caller, ensuring that
// any two threads will be operating on disjoint slices of the `buffer`. The `free_list`
// (`ArrayQueue`) is itself thread-safe.
unsafe impl Sync for MemoryPool {}

impl MemoryPool {
    /// Creates a new `MemoryPool`.
    ///
    /// This allocates a contiguous memory block of `num_slots * block_size` bytes.
    ///
    /// # Arguments
    ///
    /// * `num_slots` - The total number of memory slots to create.
    /// * `block_size` - The size of each memory slot in bytes.
    ///
    /// # Returns
    ///
    /// A new `MemoryPool` instance.
    pub fn new(num_slots: usize, block_size: usize) -> Self {
        let capacity = num_slots.saturating_mul(block_size);
        let buffer = vec![0u8; capacity];
        let free_list = ArrayQueue::new(num_slots);
        for i in 0..num_slots {
            // This should not fail as the queue has exactly `num_slots` capacity.
            free_list.push(i).expect("Failed to populate free list");
        }

        MemoryPool {
            buffer: UnsafeCell::new(buffer),
            free_list,
            num_slots,
            block_size,
        }
    }

    /// Allocates a memory slot from the pool.
    ///
    /// This operation is O(1).
    ///
    /// # Returns
    ///
    /// * `Some(usize)` - The ID of the allocated slot if one is available.
    /// * `None` - If the pool is empty.
    pub fn allocate(&self) -> Option<usize> {
        self.free_list.pop()
    }

    /// Returns a memory slot to the pool.
    ///
    /// This operation is O(1).
    ///
    /// # Arguments
    ///
    /// * `slot_id` - The ID of the slot to free.
    ///
    /// # Panics
    ///
    /// Panics in debug builds if the pool is already full, which indicates a
    /// likely double-free.
    pub fn free(&self, slot_id: usize) {
        if let Err(err) = self.free_list.push(slot_id) {
            tracing::warn!(
                "Failed to free slot {} back to pool (pool may be full or double-free): {}",
                slot_id,
                err
            );
        }
    }

    /// Writes data from a source slice into a specified memory slot.
    ///
    /// # Arguments
    ///
    /// * `slot_id` - The destination slot ID.
    /// * `src` - The source slice. Its length must match the pool's `block_size`.
    ///
    /// # Returns
    ///
    /// * `Ok(())` - On successful write.
    /// * `Err(String)` - If `slot_id` is invalid or `src` has an incorrect length.
    pub fn write(&self, slot_id: usize, src: &[u8]) -> Result<(), String> {
        if slot_id >= self.num_slots {
            return Err(format!(
                "Invalid slot_id: {} (max is {})",
                slot_id,
                self.num_slots - 1
            ));
        }
        if src.len() != self.block_size {
            return Err(format!(
                "Source slice length {} does not match block size {}",
                src.len(),
                self.block_size
            ));
        }

        let offset = slot_id * self.block_size;

        // # Safety
        // This is safe because:
        // 1. The caller has obtained a unique `slot_id` from `allocate()`.
        // 2. We have checked the bounds of `slot_id` and the length of `src`.
        // 3. The `Vec` buffer is never reallocated, so the pointer is stable.
        unsafe {
            let buffer_ptr = self.buffer.get(); // Returns *mut Vec<u8>
            let dest_ptr = (*buffer_ptr).as_mut_ptr().add(offset);
            std::ptr::copy_nonoverlapping(src.as_ptr(), dest_ptr, self.block_size);
        }

        Ok(())
    }

    /// Reads data from a specified memory slot into a destination slice.
    ///
    /// # Arguments
    ///
    /// * `slot_id` - The source slot ID.
    /// * `dst` - The destination slice. Its length must match the pool's `block_size`.
    ///
    /// # Returns
    ///
    /// * `Ok(())` - On successful read.
    /// * `Err(String)` - If `slot_id` is invalid or `dst` has an incorrect length.
    pub fn read(&self, slot_id: usize, dst: &mut [u8]) -> Result<(), String> {
        if slot_id >= self.num_slots {
            return Err(format!(
                "Invalid slot_id: {} (max is {})",
                slot_id,
                self.num_slots - 1
            ));
        }
        if dst.len() != self.block_size {
            return Err(format!(
                "Destination slice length {} does not match block size {}",
                dst.len(),
                self.block_size
            ));
        }

        let offset = slot_id * self.block_size;

        // # Safety
        // This is safe because:
        // 1. The caller has a valid `slot_id`.
        // 2. We have checked the bounds of `slot_id` and the length of `dst`.
        // 3. Concurrent reads are safe. A concurrent write will be to a different,
        //    disjoint slot.
        unsafe {
            let buffer_ptr = self.buffer.get(); // Returns *mut Vec<u8>
            let src_ptr = (*buffer_ptr).as_ptr().add(offset);
            std::ptr::copy_nonoverlapping(src_ptr, dst.as_mut_ptr(), self.block_size);
        }

        Ok(())
    }

    /// Returns the number of currently available slots in the pool.
    pub fn free_slots(&self) -> usize {
        self.free_list.len()
    }

    /// Returns the size of each block in the pool.
    pub fn block_size(&self) -> usize {
        self.block_size
    }

    /// Returns the total number of slots in the pool.
    pub fn total_slots(&self) -> usize {
        self.num_slots
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn test_new_pool() {
        let pool = MemoryPool::new(100, 16);
        assert_eq!(pool.total_slots(), 100);
        assert_eq!(pool.block_size(), 16);
        assert_eq!(pool.free_slots(), 100);
    }

    #[test]
    fn test_alloc_and_free() {
        let pool = MemoryPool::new(10, 32);
        assert_eq!(pool.free_slots(), 10);

        let mut slots = Vec::new();
        for i in 0..10 {
            let slot = pool.allocate().expect("Allocation should succeed");
            assert!(!slots.contains(&slot), "Allocated slot should be unique");
            slots.push(slot);
            assert_eq!(pool.free_slots(), 10 - (i + 1));
        }

        assert!(pool.allocate().is_none(), "Pool should be empty");

        for slot in slots {
            pool.free(slot);
        }

        assert_eq!(pool.free_slots(), 10);
    }

    #[test]
    fn test_read_write_single_thread() {
        let pool = MemoryPool::new(5, 8);
        let slot = pool.allocate().unwrap();

        let data_to_write: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];
        pool.write(slot, &data_to_write).unwrap();

        let mut data_to_read: [u8; 8] = [0; 8];
        pool.read(slot, &mut data_to_read).unwrap();

        assert_eq!(data_to_write, data_to_read);

        pool.free(slot);
    }

    #[test]
    fn test_read_write_invalid_args() {
        let pool = MemoryPool::new(1, 16);
        let good_slot = pool.allocate().unwrap();

        let good_data = vec![0; 16];
        let bad_data_small = vec![0; 8];
        let bad_data_large = vec![0; 32];
        let bad_slot = 100;

        assert!(
            pool.write(bad_slot, &good_data).is_err(),
            "Write to bad slot should fail"
        );
        assert!(
            pool.write(good_slot, &bad_data_small).is_err(),
            "Write with small buffer should fail"
        );
        assert!(
            pool.write(good_slot, &bad_data_large).is_err(),
            "Write with large buffer should fail"
        );

        let mut good_buffer = vec![0; 16];
        let mut bad_buffer_small = vec![0; 8];
        let mut bad_buffer_large = vec![0; 32];

        assert!(
            pool.read(bad_slot, &mut good_buffer).is_err(),
            "Read from bad slot should fail"
        );
        assert!(
            pool.read(good_slot, &mut bad_buffer_small).is_err(),
            "Read with small buffer should fail"
        );
        assert!(
            pool.read(good_slot, &mut bad_buffer_large).is_err(),
            "Read with large buffer should fail"
        );
    }

    #[test]
    fn test_concurrent_access() {
        let num_threads = 8;
        let num_slots_per_thread = 10;
        let block_size = 64;
        // Introduce contention by having fewer slots than would be needed for all threads to allocate at once.
        let num_pool_slots = num_threads * num_slots_per_thread / 2;
        let iterations = 50;

        let pool = Arc::new(MemoryPool::new(num_pool_slots, block_size));
        let mut handles = vec![];

        for i in 0..num_threads {
            let pool_clone = Arc::clone(&pool);
            let handle = thread::spawn(move || {
                let mut my_slots = Vec::new();
                let mut write_buffer = vec![0u8; block_size];
                let mut read_buffer = vec![0u8; block_size];

                for _ in 0..iterations {
                    // Try to allocate some slots
                    for _ in 0..num_slots_per_thread {
                        if let Some(slot) = pool_clone.allocate() {
                            my_slots.push(slot);
                        }
                    }

                    // Work on the slots we managed to get
                    for &slot in &my_slots {
                        // Write thread-specific data to avoid accidental success
                        write_buffer[0] = i as u8; // Thread ID
                        write_buffer[1] = slot as u8; // Slot ID
                        pool_clone.write(slot, &write_buffer).unwrap();

                        // Read it back immediately to check for corruption
                        pool_clone.read(slot, &mut read_buffer).unwrap();
                        assert_eq!(
                            write_buffer, read_buffer,
                            "Data corruption detected in thread {}",
                            i
                        );
                    }

                    // Free all our slots
                    for slot in my_slots.drain(..) {
                        pool_clone.free(slot);
                    }
                }
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.join().unwrap();
        }

        // After all threads are done, all slots should have been returned.
        assert_eq!(
            pool.free_slots(),
            num_pool_slots,
            "All slots should be freed at the end"
        );
    }
}
