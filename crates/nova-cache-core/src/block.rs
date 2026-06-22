//! # Block Descriptor Module
//!
//! Defines the fundamental unit of caching in Nova Cache — the **Block**.
//!
//! Each block represents a fixed-size region of a disk volume, identified by
//! a `BlockId` consisting of the volume identifier and the byte offset.
//!
//! Block sizes are configurable (4KB–1MB) with a default of 64KB, which provides
//! a good balance between:
//! - Small random I/O (game asset loading, database pages)
//! - Large sequential I/O (video playback, file copying)

use std::fmt;
use std::hash::Hash;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

/// Default block size: 64 KB
pub const DEFAULT_BLOCK_SIZE: usize = 64 * 1024;

/// Minimum block size: 4 KB (one memory page)
pub const MIN_BLOCK_SIZE: usize = 4 * 1024;

/// Maximum block size: 1 MB
pub const MAX_BLOCK_SIZE: usize = 1024 * 1024;

/// Unique identifier for a volume being cached.
///
/// Stores the volume GUID path (e.g., `\\?\Volume{GUID}`).
#[derive(Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct VolumeId {
    /// Volume GUID path stored as a compact string.
    guid: String,
}

impl VolumeId {
    /// Create a new VolumeId from a GUID string.
    pub fn new(guid: &str) -> Self {
        Self {
            guid: guid.to_string(),
        }
    }

    /// Returns the GUID as a string slice.
    #[inline]
    pub fn as_str(&self) -> &str {
        &self.guid
    }
}

impl fmt::Debug for VolumeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "VolumeId({})", self.guid)
    }
}

impl fmt::Display for VolumeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.guid)
    }
}

/// Unique identifier for a cached block.
///
/// A block is uniquely identified by the volume it belongs to and the
/// byte offset within that volume. The offset is always aligned to the
/// configured block size.
#[derive(Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct BlockId {
    /// The volume this block belongs to.
    pub volume: VolumeId,
    /// Byte offset within the volume (block-aligned).
    pub offset: u64,
}

impl BlockId {
    /// Create a new BlockId.
    ///
    /// The offset is automatically aligned down to the given block size.
    pub fn new(volume: VolumeId, offset: u64, block_size: usize) -> Self {
        let aligned_offset = offset & !(block_size as u64 - 1);
        Self {
            volume,
            offset: aligned_offset,
        }
    }

    /// Create a BlockId from a raw (already aligned) offset.
    pub fn from_aligned(volume: VolumeId, aligned_offset: u64) -> Self {
        Self {
            volume,
            offset: aligned_offset,
        }
    }
}

impl fmt::Debug for BlockId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Block({}, offset=0x{:X})",
            self.volume.as_str(),
            self.offset
        )
    }
}

impl fmt::Display for BlockId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@0x{:X}", self.volume.as_str(), self.offset)
    }
}

/// Indicates where the block data currently resides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CacheTier {
    /// Block is in L1 RAM cache (fastest).
    L1Ram,
    /// Block is in L2 SSD cache (fast).
    L2Ssd,
    /// Block is not cached (must read from HDD).
    None,
}

/// Flags describing the state of a cached block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockFlags {
    bits: u8,
}

impl BlockFlags {
    /// No flags set.
    pub const NONE: Self = Self { bits: 0 };
    /// Block data is valid and readable.
    pub const VALID: Self = Self { bits: 1 << 0 };
    /// Block has been modified (dirty) and needs to be flushed.
    pub const DIRTY: Self = Self { bits: 1 << 1 };
    /// Block is currently being read from disk.
    pub const READING: Self = Self { bits: 1 << 2 };
    /// Block is currently being written to disk.
    pub const WRITING: Self = Self { bits: 1 << 3 };
    /// Block is pinned (should not be evicted).
    pub const PINNED: Self = Self { bits: 1 << 4 };
    /// Block belongs to a game process (higher priority).
    pub const GAME_PRIORITY: Self = Self { bits: 1 << 5 };

    /// Check if a specific flag is set.
    #[inline]
    pub fn contains(self, other: Self) -> bool {
        (self.bits & other.bits) == other.bits
    }

    /// Set a flag.
    #[inline]
    pub fn set(&mut self, other: Self) {
        self.bits |= other.bits;
    }

    /// Clear a flag.
    #[inline]
    pub fn clear(&mut self, other: Self) {
        self.bits &= !other.bits;
    }
}

/// Metadata for a cached block.
///
/// This structure is stored alongside the block data and tracks
/// access patterns, state, and location information.
#[derive(Debug)]
pub struct BlockMeta {
    /// Unique identifier for this block.
    pub id: BlockId,
    /// Which cache tier this block currently resides in.
    pub tier: CacheTier,
    /// State flags for this block.
    pub flags: BlockFlags,
    /// Size of the actual data in this block (may be < block_size for partial blocks).
    pub data_size: u32,
    /// Number of times this block has been accessed.
    pub access_count: AtomicU64,
    /// Timestamp of last access (for statistics).
    pub last_access: RwLock<Instant>,
    /// Process ID that last accessed this block (for game mode priority).
    pub last_pid: AtomicU64,
}

impl BlockMeta {
    /// Create new block metadata.
    pub fn new(id: BlockId, tier: CacheTier, data_size: u32) -> Self {
        Self {
            id,
            tier,
            flags: BlockFlags::VALID,
            data_size,
            access_count: AtomicU64::new(1),
            last_access: RwLock::new(Instant::now()),
            last_pid: AtomicU64::new(0),
        }
    }

    /// Record an access to this block.
    #[inline]
    pub fn record_access(&self, pid: u64) {
        self.access_count.fetch_add(1, Ordering::Relaxed);
        *self.last_access.write() = Instant::now();
        self.last_pid.store(pid, Ordering::Relaxed);
    }

    /// Get the total number of accesses.
    #[inline]
    pub fn accesses(&self) -> u64 {
        self.access_count.load(Ordering::Relaxed)
    }

    /// Check if this block is dirty (needs flushing).
    #[inline]
    pub fn is_dirty(&self) -> bool {
        self.flags.contains(BlockFlags::DIRTY)
    }

    /// Check if this block is pinned.
    #[inline]
    pub fn is_pinned(&self) -> bool {
        self.flags.contains(BlockFlags::PINNED)
    }

    /// Check if this block has game priority.
    #[inline]
    pub fn is_game_priority(&self) -> bool {
        self.flags.contains(BlockFlags::GAME_PRIORITY)
    }
}

/// Calculates how many blocks are needed to cover a given byte range.
///
/// # Arguments
/// * `offset` - Starting byte offset
/// * `length` - Length of the range in bytes
/// * `block_size` - Size of each block in bytes
///
/// # Returns
/// A vector of aligned offsets that cover the range.
pub fn blocks_for_range(offset: u64, length: u64, block_size: usize) -> Vec<u64> {
    let bs = block_size as u64;
    let start_block = offset / bs;
    let end_block = (offset + length + bs - 1) / bs;

    (start_block..end_block).map(|b| b * bs).collect()
}

/// Aligns an offset down to the nearest block boundary.
#[inline]
pub fn align_down(offset: u64, block_size: usize) -> u64 {
    offset & !(block_size as u64 - 1)
}

/// Aligns an offset up to the nearest block boundary.
#[inline]
pub fn align_up(offset: u64, block_size: usize) -> u64 {
    let bs = block_size as u64;
    (offset + bs - 1) & !(bs - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_volume_id_creation() {
        let vol = VolumeId::new("\\\\?\\Volume{abc-123}");
        assert_eq!(vol.as_str(), "\\\\?\\Volume{abc-123}");
    }

    #[test]
    fn test_block_id_alignment() {
        let vol = VolumeId::new("vol1");
        let block = BlockId::new(vol.clone(), 100_000, DEFAULT_BLOCK_SIZE);
        // 100_000 / 65536 = 1, so aligned offset = 65536
        assert_eq!(block.offset, 65536);

        let block2 = BlockId::new(vol, 65536, DEFAULT_BLOCK_SIZE);
        assert_eq!(block2.offset, 65536);
    }

    #[test]
    fn test_block_id_equality() {
        let vol = VolumeId::new("vol1");
        let b1 = BlockId::new(vol.clone(), 65536, DEFAULT_BLOCK_SIZE);
        let b2 = BlockId::new(vol, 70000, DEFAULT_BLOCK_SIZE); // Same block
        assert_eq!(b1, b2);
    }

    #[test]
    fn test_blocks_for_range() {
        let blocks = blocks_for_range(0, 200_000, DEFAULT_BLOCK_SIZE);
        // 200_000 / 65536 = 3.05, so 4 blocks needed: 0, 65536, 131072, 196608
        assert_eq!(blocks.len(), 4);
        assert_eq!(blocks[0], 0);
        assert_eq!(blocks[1], 65536);
        assert_eq!(blocks[2], 131072);
        assert_eq!(blocks[3], 196608);
    }

    #[test]
    fn test_align_functions() {
        assert_eq!(align_down(100_000, DEFAULT_BLOCK_SIZE), 65536);
        assert_eq!(align_up(100_000, DEFAULT_BLOCK_SIZE), 131072);
        assert_eq!(align_down(65536, DEFAULT_BLOCK_SIZE), 65536);
        assert_eq!(align_up(65536, DEFAULT_BLOCK_SIZE), 65536);
        assert_eq!(align_down(0, DEFAULT_BLOCK_SIZE), 0);
        assert_eq!(align_up(0, DEFAULT_BLOCK_SIZE), 0);
    }

    #[test]
    fn test_block_flags() {
        let mut flags = BlockFlags::NONE;
        assert!(!flags.contains(BlockFlags::VALID));

        flags.set(BlockFlags::VALID);
        assert!(flags.contains(BlockFlags::VALID));
        assert!(!flags.contains(BlockFlags::DIRTY));

        flags.set(BlockFlags::DIRTY);
        assert!(flags.contains(BlockFlags::VALID));
        assert!(flags.contains(BlockFlags::DIRTY));

        flags.clear(BlockFlags::VALID);
        assert!(!flags.contains(BlockFlags::VALID));
        assert!(flags.contains(BlockFlags::DIRTY));
    }

    #[test]
    fn test_block_meta_access() {
        let vol = VolumeId::new("vol1");
        let id = BlockId::from_aligned(vol, 0);
        let meta = BlockMeta::new(id, CacheTier::L1Ram, DEFAULT_BLOCK_SIZE as u32);

        assert_eq!(meta.accesses(), 1);
        meta.record_access(1234);
        assert_eq!(meta.accesses(), 2);
        assert_eq!(meta.last_pid.load(Ordering::Relaxed), 1234);
    }

    #[test]
    fn test_block_meta_game_priority() {
        let vol = VolumeId::new("vol1");
        let id = BlockId::from_aligned(vol, 0);
        let mut meta = BlockMeta::new(id, CacheTier::L1Ram, DEFAULT_BLOCK_SIZE as u32);

        assert!(!meta.is_game_priority());
        meta.flags.set(BlockFlags::GAME_PRIORITY);
        assert!(meta.is_game_priority());
    }
}
