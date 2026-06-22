use anyhow::{Context, Result};
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;

const MAGIC: &[u8; 8] = b"NOVACACH";
const VERSION: u32 = 3;

#[derive(Debug, Clone)]
pub struct CachedEntry {
    pub block_id: u64,
    pub backend_index: u32,
    pub slot_id: u32,
    pub in_t2: bool,
}

fn write_u32(writer: &mut impl Write, v: u32) -> std::io::Result<()> {
    writer.write_all(&v.to_le_bytes())
}

fn write_u64(writer: &mut impl Write, v: u64) -> std::io::Result<()> {
    writer.write_all(&v.to_le_bytes())
}

fn write_u8(writer: &mut impl Write, v: u8) -> std::io::Result<()> {
    writer.write_all(&[v])
}

fn read_u32(reader: &mut impl Read) -> std::io::Result<u32> {
    let mut buf = [0u8; 4];
    reader.read_exact(&mut buf)?;
    Ok(u32::from_le_bytes(buf))
}

fn read_u64(reader: &mut impl Read) -> std::io::Result<u64> {
    let mut buf = [0u8; 8];
    reader.read_exact(&mut buf)?;
    Ok(u64::from_le_bytes(buf))
}

fn read_u8(reader: &mut impl Read) -> std::io::Result<u8> {
    let mut buf = [0u8; 1];
    reader.read_exact(&mut buf)?;
    Ok(buf[0])
}

pub fn save_cache_index(
    path: &Path,
    cache_generation: u64,
    block_size: u32,
    entries: &[CachedEntry],
) -> Result<()> {
    let tmp_path = path.with_extension("tmp");

    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp_path)
        .context("Failed to create cache index file")?;

    let mut writer = BufWriter::new(file);

    writer.write_all(MAGIC)?;
    write_u32(&mut writer, VERSION)?;
    write_u64(&mut writer, cache_generation)?;
    write_u32(&mut writer, block_size)?;
    write_u32(&mut writer, entries.len() as u32)?;

    for entry in entries {
        write_u64(&mut writer, entry.block_id)?;
        write_u32(&mut writer, entry.backend_index)?;
        write_u32(&mut writer, entry.slot_id)?;
        write_u8(&mut writer, if entry.in_t2 { 1 } else { 0 })?;
    }

    writer.flush()?;

    drop(writer);

    std::fs::rename(&tmp_path, path).context("Failed to atomically rename cache index file")?;

    Ok(())
}

pub fn rebuild_cache_index(
    path: &Path,
    cache_generation: u64,
    block_size: u32,
    _l2_file_size: u64,
    journal_entries: &[(u64, u32, u64, u32, u32)],
) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    let mut entries = Vec::new();

    for &(block_id, _volume_id, _offset, _length, slot_id) in journal_entries {
        if !seen.insert(block_id) {
            continue;
        }
        entries.push(CachedEntry {
            block_id,
            backend_index: 0,
            slot_id,
            in_t2: false,
        });
    }

    save_cache_index(path, cache_generation, block_size, &entries)?;
    tracing::info!(
        "Rebuilt cache index from journal: {} entries from {} committed blocks",
        entries.len(),
        journal_entries.len()
    );
    Ok(())
}

pub fn load_cache_index(path: &Path) -> Result<Option<(u32, u64, Vec<CachedEntry>)>> {
    if !path.exists() {
        return Ok(None);
    }

    let file = File::open(path).context("Failed to open cache index file")?;
    let mut reader = BufReader::new(file);

    let mut magic = [0u8; 8];
    reader.read_exact(&mut magic)?;
    if &magic != MAGIC {
        tracing::warn!("Cache index file has invalid magic, ignoring");
        return Ok(None);
    }

    let version = read_u32(&mut reader)?;
    if version < 1 || version > 3 {
        tracing::warn!(
            "Cache index file version mismatch: expected 1-3, got {}",
            version
        );
        return Ok(None);
    }

    let cache_generation = if version >= 3 {
        read_u64(&mut reader)?
    } else {
        0 // version 1/2: unknown generation → forces discard on generation-aware restore
    };

    let block_size = read_u32(&mut reader)?;
    let num_entries = read_u32(&mut reader)?;

    let mut entries = Vec::with_capacity(num_entries as usize);
    for _ in 0..num_entries {
        let block_id = read_u64(&mut reader)?;
        let backend_index = if version >= 2 {
            read_u32(&mut reader)?
        } else {
            0
        };
        let slot_id = read_u32(&mut reader)?;
        let in_t2 = read_u8(&mut reader)? != 0;
        entries.push(CachedEntry {
            block_id,
            backend_index,
            slot_id,
            in_t2,
        });
    }

    tracing::info!(
        "Loaded {} cached entries from index (block_size={}, generation=0x{:016X})",
        entries.len(),
        block_size,
        cache_generation
    );
    Ok(Some((block_size, cache_generation, entries)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_save_and_load_empty() {
        let dir = std::env::temp_dir().join("novacache_persistence_test_empty");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cache_index.bin");

        save_cache_index(&path, 0xDEAD_BEEF_CAFE, 65536, &[]).unwrap();
        let result = load_cache_index(&path).unwrap();
        assert!(result.is_some());
        let (bs, gen, entries) = result.unwrap();
        assert_eq!(bs, 65536);
        assert_eq!(gen, 0xDEAD_BEEF_CAFE);
        assert!(entries.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_save_and_load_entries() {
        let dir = std::env::temp_dir().join("novacache_persistence_test_entries");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cache_index.bin");

        let entries = vec![
            CachedEntry {
                block_id: 0x0001_0000_0000,
                backend_index: 0,
                slot_id: 0,
                in_t2: true,
            },
            CachedEntry {
                block_id: 0x0002_0000_1000,
                backend_index: 1,
                slot_id: 1,
                in_t2: false,
            },
            CachedEntry {
                block_id: 0x0003_FFFF_F000,
                backend_index: 2,
                slot_id: 42,
                in_t2: true,
            },
        ];

        save_cache_index(&path, 0x1234_5678_9ABC_DEF0, 65536, &entries).unwrap();
        let result = load_cache_index(&path).unwrap();
        let (bs, gen, loaded) = result.unwrap();
        assert_eq!(bs, 65536);
        assert_eq!(gen, 0x1234_5678_9ABC_DEF0);
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded[0].block_id, 0x0001_0000_0000);
        assert_eq!(loaded[0].backend_index, 0);
        assert_eq!(loaded[0].slot_id, 0);
        assert!(loaded[0].in_t2);
        assert_eq!(loaded[1].block_id, 0x0002_0000_1000);
        assert_eq!(loaded[1].backend_index, 1);
        assert!(!loaded[1].in_t2);
        assert_eq!(loaded[2].block_id, 0x0003_FFFF_F000);
        assert_eq!(loaded[2].backend_index, 2);
        assert_eq!(loaded[2].slot_id, 42);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_nonexistent() {
        let result = load_cache_index(Path::new("/nonexistent/path/cache_index.bin")).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_atomic_write() {
        let dir = std::env::temp_dir().join("novacache_persistence_test_atomic");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cache_index.bin");

        let entries = vec![CachedEntry {
            block_id: 123,
            backend_index: 0,
            slot_id: 5,
            in_t2: false,
        }];
        save_cache_index(&path, 0x4242, 4096, &entries).unwrap();

        let tmp_path = path.with_extension("tmp");
        assert!(
            !tmp_path.exists(),
            "tmp file should not remain after successful rename"
        );

        let (bs, gen, loaded) = load_cache_index(&path).unwrap().unwrap();
        assert_eq!(bs, 4096);
        assert_eq!(gen, 0x4242);
        assert_eq!(loaded.len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
