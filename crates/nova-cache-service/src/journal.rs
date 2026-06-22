use anyhow::{anyhow, Result};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use tracing::{info, warn};

const JOURNAL_MAGIC: u32 = 0x4E4F5641; // "NOVA"
const JOURNAL_VERSION: u32 = 1;
const JOURNAL_HEADER_SIZE: u64 = 16;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct JournalEntryHeader {
    magic: u32,
    sequence: u64,
    block_id: u64,
    volume_id: u32,
    offset: u64,
    length: u32,
    checksum: u32,
    committed: u32,
    slot_id: u32,
}

impl JournalEntryHeader {
    fn size() -> usize {
        std::mem::size_of::<Self>()
    }
}

#[derive(Debug, Clone)]
pub struct JournalEntryMeta {
    pub block_id: u64,
    pub volume_id: u32,
    pub offset: u64,
    pub length: u32,
    pub slot_id: u32,
}

pub struct Journal {
    file: Mutex<File>,
    path: PathBuf,
    l2_pool: Arc<parking_lot::RwLock<nova_cache_core::l2_pool::L2Pool>>,
    block_size: usize,
    next_sequence: AtomicU64,
    uncommitted_count: AtomicU64,
}

impl Journal {
    pub fn open(
        path: PathBuf,
        l2_pool: Arc<parking_lot::RwLock<nova_cache_core::l2_pool::L2Pool>>,
        block_size: usize,
    ) -> Result<Self> {
        let file = if path.exists() {
            OpenOptions::new().read(true).write(true).open(&path)?
        } else {
            let mut f = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .open(&path)?;

            let mut hdr = [0u8; 16];
            hdr[0..4].copy_from_slice(&JOURNAL_MAGIC.to_le_bytes());
            hdr[4..8].copy_from_slice(&JOURNAL_VERSION.to_le_bytes());
            // hdr[8..16] stays zero (reserved)
            f.write_all(&hdr)?;
            f.sync_all()?;
            info!("Created new journal at {}", path.display());
            f
        };

        let journal = Self {
            file: Mutex::new(file),
            path,
            l2_pool,
            block_size,
            next_sequence: AtomicU64::new(1),
            uncommitted_count: AtomicU64::new(0),
        };

        journal.scan_and_replay()?;
        Ok(journal)
    }

    fn scan_and_replay(&self) -> Result<()> {
        let mut file = self
            .file
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;

        file.seek(SeekFrom::Start(0))?;
        let mut hdr_buf = [0u8; 16];
        if file.read(&mut hdr_buf)? < 16 {
            info!("Journal empty or corrupt header, starting fresh");
            return Ok(());
        }

        let magic = u32::from_le_bytes([hdr_buf[0], hdr_buf[1], hdr_buf[2], hdr_buf[3]]);
        if magic != JOURNAL_MAGIC {
            warn!(
                "Journal magic mismatch: 0x{:08X} != 0x{:08X}, reinitializing",
                magic, JOURNAL_MAGIC
            );
            file.seek(SeekFrom::Start(0))?;
            file.write_all(&JOURNAL_MAGIC.to_le_bytes())?;
            file.write_all(&JOURNAL_VERSION.to_le_bytes())?;
            file.write_all(&[0u8; 8])?;
            file.sync_all()?;
            return Ok(());
        }

        let mut max_seq: u64 = 0;
        let mut uncommitted: Vec<(JournalEntryHeader, Vec<u8>)> = Vec::new();

        loop {
            let mut entry_hdr_buf = [0u8; std::mem::size_of::<JournalEntryHeader>()];
            match file.read_exact(&mut entry_hdr_buf) {
                Ok(_) => {}
                Err(_) => break,
            }

            let entry_hdr =
                unsafe { std::ptr::read(entry_hdr_buf.as_ptr() as *const JournalEntryHeader) };

            if entry_hdr.magic != JOURNAL_MAGIC {
                break;
            }

            if entry_hdr.sequence > max_seq {
                max_seq = entry_hdr.sequence;
            }

            if entry_hdr.length > 0 && entry_hdr.length <= 65536 {
                let mut data = vec![0u8; entry_hdr.length as usize];
                if file.read_exact(&mut data).is_err() {
                    break;
                }
                if entry_hdr.committed == 0 {
                    uncommitted.push((entry_hdr, data));
                }
            } else {
                break;
            }
        }

        self.next_sequence.store(max_seq + 1, Ordering::Relaxed);

        if !uncommitted.is_empty() {
            info!(
                "Journal: {} uncommitted entries found, replaying...",
                uncommitted.len()
            );
            for (hdr, _data) in &uncommitted {
                info!(
                    "Journal replay: block_id=0x{:016X} vol={} offset=0x{:X} len={}",
                    hdr.block_id, hdr.volume_id, hdr.offset, hdr.length
                );
            }

            for (hdr, data) in &uncommitted {
                let actual_crc = crc32c_simple(data);
                if actual_crc != hdr.checksum {
                    warn!("Journal replay: CRC32 mismatch for block_id=0x{:016X} (expected 0x{:08X}, got 0x{:08X}). Skipping.", hdr.block_id, hdr.checksum, actual_crc);
                    self.mark_committed_raw(&mut file, hdr.sequence)?;
                    continue;
                }
                if let Err(e) = self.replay_entry_to_disk(&hdr, &data) {
                    warn!("Journal replay failed for block_id=0x{:016X}: {:?}. Marking committed anyway.", hdr.block_id, e);
                } else {
                    info!(
                        "Journal replay: block_id=0x{:016X} written to disk",
                        hdr.block_id
                    );
                }
                self.mark_committed_raw(&mut file, hdr.sequence)?;
            }
            info!("Journal replay complete");
            self.uncommitted_count.store(0, Ordering::Relaxed);
        } else {
            info!("Journal: all entries committed");
            self.uncommitted_count.store(0, Ordering::Relaxed);
        }

        Ok(())
    }

    fn mark_committed_raw(&self, file: &mut File, sequence: u64) -> Result<()> {
        file.seek(SeekFrom::Start(JOURNAL_HEADER_SIZE))?;
        loop {
            let pos = file.stream_position()?;
            let mut entry_hdr_buf = [0u8; std::mem::size_of::<JournalEntryHeader>()];
            match file.read_exact(&mut entry_hdr_buf) {
                Ok(_) => {}
                Err(_) => break,
            }

            let mut entry_hdr =
                unsafe { std::ptr::read(entry_hdr_buf.as_ptr() as *const JournalEntryHeader) };

            if entry_hdr.magic != JOURNAL_MAGIC {
                break;
            }

            if entry_hdr.sequence == sequence {
                entry_hdr.committed = 1;
                file.seek(SeekFrom::Start(pos))?;
                let updated_buf = unsafe {
                    std::slice::from_raw_parts(
                        &entry_hdr as *const JournalEntryHeader as *const u8,
                        std::mem::size_of::<JournalEntryHeader>(),
                    )
                };
                file.write_all(updated_buf)?;
                return Ok(());
            }

            if entry_hdr.length > 0 && entry_hdr.length <= 65536 {
                let mut skip = vec![0u8; entry_hdr.length as usize];
                if file.read_exact(&mut skip).is_err() {
                    break;
                }
            } else {
                break;
            }
        }
        Ok(())
    }

    fn replay_entry_to_disk(&self, hdr: &JournalEntryHeader, data: &[u8]) -> Result<()> {
        let pool = self.l2_pool.read();
        let slot = nova_cache_core::l2_pool::L2Slot {
            backend: 0,
            slot: hdr.slot_id,
        };
        pool.write(&slot, data).map_err(|e| anyhow!(e))
    }

    pub fn append(
        &self,
        block_id: u64,
        volume_id: u32,
        offset: u64,
        slot_id: u32,
        data: &[u8],
    ) -> Result<u64> {
        let mut file = self
            .file
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;

        let sequence = self.next_sequence.fetch_add(1, Ordering::Relaxed);

        let checksum = crc32c_simple(data);

        let entry = JournalEntryHeader {
            magic: JOURNAL_MAGIC,
            sequence,
            block_id,
            volume_id,
            offset,
            length: data.len() as u32,
            checksum,
            committed: 0,
            slot_id,
        };

        file.seek(SeekFrom::End(0))?;

        let hdr_bytes = unsafe {
            std::slice::from_raw_parts(
                &entry as *const JournalEntryHeader as *const u8,
                JournalEntryHeader::size(),
            )
        };
        file.write_all(hdr_bytes)?;
        file.write_all(data)?;

        self.uncommitted_count.fetch_add(1, Ordering::Relaxed);

        Ok(sequence)
    }

    pub fn commit_batch(&self, sequences: &[u64]) -> Result<()> {
        if sequences.is_empty() {
            return Ok(());
        }

        let mut file = self
            .file
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;

        for &seq in sequences {
            self.mark_committed_raw(&mut file, seq)?;
        }

        file.sync_all()?;
        self.uncommitted_count
            .fetch_sub(sequences.len() as u64, Ordering::Relaxed);

        Ok(())
    }

    pub fn commit(&self, sequence: u64) -> Result<()> {
        let mut file = self
            .file
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        self.mark_committed_raw(&mut file, sequence)?;
        file.sync_all()?;
        self.uncommitted_count.fetch_sub(1, Ordering::Relaxed);
        Ok(())
    }

    pub fn uncommitted_count(&self) -> u64 {
        self.uncommitted_count.load(Ordering::Relaxed)
    }

    pub fn is_empty(&self) -> bool {
        self.uncommitted_count.load(Ordering::Relaxed) == 0
    }

    pub fn file_size(&self) -> u64 {
        std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0)
    }

    pub fn scan_committed_entries(&self) -> Result<Vec<JournalEntryMeta>> {
        let mut file = self
            .file
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;

        file.seek(SeekFrom::Start(0))?;
        let mut hdr_buf = [0u8; 16];
        if file.read(&mut hdr_buf)? < 16 {
            return Ok(Vec::new());
        }

        let magic = u32::from_le_bytes([hdr_buf[0], hdr_buf[1], hdr_buf[2], hdr_buf[3]]);
        if magic != JOURNAL_MAGIC {
            return Ok(Vec::new());
        }

        let mut entries = Vec::new();

        loop {
            let mut entry_hdr_buf = [0u8; std::mem::size_of::<JournalEntryHeader>()];
            match file.read_exact(&mut entry_hdr_buf) {
                Ok(_) => {}
                Err(_) => break,
            }

            let entry_hdr =
                unsafe { std::ptr::read(entry_hdr_buf.as_ptr() as *const JournalEntryHeader) };

            if entry_hdr.magic != JOURNAL_MAGIC {
                break;
            }

            if entry_hdr.length > 0 && entry_hdr.length <= 65536 {
                let mut skip = vec![0u8; entry_hdr.length as usize];
                if file.read_exact(&mut skip).is_err() {
                    break;
                }
                if entry_hdr.committed != 0 {
                    entries.push(JournalEntryMeta {
                        block_id: entry_hdr.block_id,
                        volume_id: entry_hdr.volume_id,
                        offset: entry_hdr.offset,
                        length: entry_hdr.length,
                        slot_id: entry_hdr.slot_id,
                    });
                }
            } else {
                break;
            }
        }

        Ok(entries)
    }

    pub fn truncate(&self) -> Result<()> {
        let mut file = self
            .file
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;

        file.seek(SeekFrom::Start(JOURNAL_HEADER_SIZE))?;
        let mut entries: Vec<(JournalEntryHeader, Vec<u8>)> = Vec::new();

        loop {
            let mut entry_hdr_buf = [0u8; std::mem::size_of::<JournalEntryHeader>()];
            match file.read_exact(&mut entry_hdr_buf) {
                Ok(_) => {}
                Err(_) => break,
            }

            let entry_hdr =
                unsafe { std::ptr::read(entry_hdr_buf.as_ptr() as *const JournalEntryHeader) };

            if entry_hdr.magic != JOURNAL_MAGIC {
                break;
            }

            let mut data = vec![0u8; entry_hdr.length as usize];
            if file.read_exact(&mut data).is_err() {
                break;
            }

            if entry_hdr.committed == 0 {
                entries.push((entry_hdr, data));
            }
        }

        // Rewrite in-place and truncate
        file.seek(SeekFrom::Start(0))?;
        file.set_len(0)?;

        file.write_all(&JOURNAL_MAGIC.to_le_bytes())?;
        file.write_all(&JOURNAL_VERSION.to_le_bytes())?;
        file.write_all(&[0u8; 8])?;

        for (hdr, data) in &entries {
            let hdr_bytes = unsafe {
                std::slice::from_raw_parts(
                    hdr as *const JournalEntryHeader as *const u8,
                    JournalEntryHeader::size(),
                )
            };
            file.write_all(hdr_bytes)?;
            file.write_all(data)?;
        }

        file.sync_all()?;

        self.uncommitted_count
            .store(entries.len() as u64, Ordering::Relaxed);
        info!(
            "Journal truncated: {} uncommitted entries retained",
            entries.len()
        );
        Ok(())
    }
}

fn crc32c_simple(data: &[u8]) -> u32 {
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
