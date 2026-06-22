use crate::ssd_tier::SsdTier;
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::time::Instant;
use tracing::{info, warn};

#[derive(Debug, Clone)]
pub struct L2Slot {
    pub backend: u32,
    pub slot: u32,
}

pub struct L2Backend {
    pub path: PathBuf,
    pub tier: SsdTier,
    pub speed_mbps: f64,
}

pub struct L2Pool {
    backends: Vec<L2Backend>,
    total_slots: usize,
}

fn measure_read_speed(path: &Path) -> f64 {
    let test_path = path.join(".nova_speed_test.bin");
    let data: Vec<u8> = (0..1024 * 1024).map(|i| (i % 256) as u8).collect();

    if std::fs::write(&test_path, &data).is_err() {
        return 0.0;
    }

    let start = Instant::now();
    let result = std::fs::read(&test_path);
    let elapsed = start.elapsed();

    let _ = std::fs::remove_file(&test_path);

    match result {
        Ok(_) if !elapsed.is_zero() => 1024.0 / elapsed.as_secs_f64(),
        _ => 0.0,
    }
}

impl L2Pool {
    pub fn new(paths: &[PathBuf], size_per_backend: u64, block_size: usize) -> Result<Self> {
        let mut backends: Vec<L2Backend> = Vec::new();

        for path in paths {
            let dir = path.parent().unwrap_or(Path::new("."));
            if !dir.exists() {
                warn!(
                    "L2 backend directory {} does not exist, skipping",
                    dir.display()
                );
                continue;
            }

            let speed = measure_read_speed(dir);
            info!("L2 backend {}: speed={:.1} MB/s", path.display(), speed);

            match SsdTier::new(path, size_per_backend, block_size) {
                Ok(tier) => {
                    info!(
                        "L2 backend {} initialized: {} slots, {:.1} MB/s",
                        path.display(),
                        tier.total_slots(),
                        speed
                    );
                    backends.push(L2Backend {
                        path: path.clone(),
                        tier,
                        speed_mbps: speed,
                    });
                }
                Err(e) => {
                    warn!("Failed to create L2 backend {}: {:?}", path.display(), e);
                }
            }
        }

        if backends.is_empty() {
            return Err(anyhow::anyhow!("No valid L2 backends could be created"));
        }

        backends.sort_by(|a, b| {
            b.speed_mbps
                .partial_cmp(&a.speed_mbps)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let total_slots = backends.iter().map(|b| b.tier.total_slots()).sum();

        info!(
            "L2 pool initialized: {} backends, {} total slots",
            backends.len(),
            total_slots
        );

        Ok(Self {
            backends,
            total_slots,
        })
    }

    pub fn allocate(&self) -> Option<L2Slot> {
        for (idx, backend) in self.backends.iter().enumerate() {
            if let Some(slot) = backend.tier.allocate() {
                return Some(L2Slot {
                    backend: idx as u32,
                    slot: slot as u32,
                });
            }
        }
        None
    }

    pub fn read(&self, slot: &L2Slot, dst: &mut [u8]) -> Result<(), String> {
        let backend = self
            .backends
            .get(slot.backend as usize)
            .ok_or_else(|| format!("Invalid backend index {}", slot.backend))?;
        backend.tier.read(slot.slot as usize, dst)
    }

    pub fn write(&self, slot: &L2Slot, src: &[u8]) -> Result<(), String> {
        let backend = self
            .backends
            .get(slot.backend as usize)
            .ok_or_else(|| format!("Invalid backend index {}", slot.backend))?;
        backend.tier.write(slot.slot as usize, src)
    }

    pub fn free(&self, slot: &L2Slot) {
        if let Some(backend) = self.backends.get(slot.backend as usize) {
            backend.tier.free(slot.slot as usize);
        }
    }

    pub fn is_healthy(&self) -> bool {
        self.backends.iter().any(|b| b.tier.is_healthy())
    }

    pub fn total_free_slots(&self) -> usize {
        self.backends.iter().map(|b| b.tier.free_slots()).sum()
    }

    pub fn total_slots(&self) -> usize {
        self.total_slots
    }

    pub fn backend_count(&self) -> usize {
        self.backends.len()
    }

    pub fn backend_info(&self) -> Vec<(PathBuf, f64, usize, usize)> {
        self.backends
            .iter()
            .map(|b| {
                (
                    b.path.clone(),
                    b.speed_mbps,
                    b.tier.free_slots(),
                    b.tier.total_slots(),
                )
            })
            .collect()
    }

    pub fn reserve_slots_for_backend(&self, backend: u32, slot_ids: &[usize]) {
        if let Some(b) = self.backends.get(backend as usize) {
            b.tier.reserve_slots(slot_ids);
        }
    }

    pub fn is_slot_valid(&self, slot: &L2Slot) -> bool {
        self.backends
            .get(slot.backend as usize)
            .map(|b| (slot.slot as usize) < b.tier.total_slots())
            .unwrap_or(false)
    }

    pub fn flush(&self) {
        for backend in &self.backends {
            if let Err(e) = backend.tier.flush() {
                tracing::warn!("L2 flush failed for backend: {:?}", e);
            }
        }
    }

    pub fn mark_unhealthy(&self) {
        for backend in &self.backends {
            backend.tier.mark_unhealthy();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("l2_pool_test_{}", name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_l2_pool_basic() {
        let dir1 = temp_dir("basic1");
        let dir2 = temp_dir("basic2");
        let path1 = dir1.join("l2.dat");
        let path2 = dir2.join("l2.dat");

        let pool = L2Pool::new(&[path1.clone(), path2.clone()], 1024 * 64, 1024).unwrap();
        assert_eq!(pool.backend_count(), 2);
        assert!(pool.total_free_slots() > 0);

        let slot = pool.allocate().unwrap();
        let data = vec![0xABu8; 1024];
        pool.write(&slot, &data).unwrap();

        let mut read_data = vec![0u8; 1024];
        pool.read(&slot, &mut read_data).unwrap();
        assert_eq!(data, read_data);

        pool.free(&slot);

        let _ = std::fs::remove_dir_all(&dir1);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    #[test]
    fn test_l2_pool_allocation_order() {
        let dir1 = temp_dir("order1");
        let dir2 = temp_dir("order2");
        let path1 = dir1.join("l2.dat");
        let path2 = dir2.join("l2.dat");

        let pool = L2Pool::new(&[path1, path2], 1024 * 64, 1024).unwrap();
        assert!(pool.backend_count() >= 1);

        let info = pool.backend_info();
        for i in 1..info.len() {
            assert!(
                info[i - 1].1 >= info[i].1,
                "backends should be sorted by speed descending"
            );
        }

        let _ = std::fs::remove_dir_all(&dir1);
        let _ = std::fs::remove_dir_all(&dir2);
    }
}
