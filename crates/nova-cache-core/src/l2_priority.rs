use parking_lot::RwLock;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU32, Ordering};

const SCORE_MAX: u32 = 100_000;

#[derive(Clone, Copy, Default)]
struct SlotEntry {
    block_id: u64,
    score: u32,
}

struct Inner {
    slots: Vec<SlotEntry>,
    used: BTreeSet<(u32, u32, u64)>,
}

pub struct L2PriorityHeap {
    inner: RwLock<Inner>,
    max_slots: usize,
    pub hit_bonus: AtomicU32,
    pub sequential_penalty: AtomicU32,
    pub initial_score: AtomicU32,
}

impl L2PriorityHeap {
    pub fn new(max_slots: usize) -> Self {
        Self {
            inner: RwLock::new(Inner {
                slots: vec![SlotEntry::default(); max_slots],
                used: BTreeSet::new(),
            }),
            max_slots,
            hit_bonus: AtomicU32::new(50),
            sequential_penalty: AtomicU32::new(5),
            initial_score: AtomicU32::new(10),
        }
    }

    pub fn set_hit_bonus(&self, val: u32) {
        self.hit_bonus.store(val, Ordering::Relaxed);
    }

    pub fn set_sequential_penalty(&self, val: u32) {
        self.sequential_penalty.store(val, Ordering::Relaxed);
    }

    pub fn set_initial_score(&self, val: u32) {
        self.initial_score.store(val, Ordering::Relaxed);
    }

    pub fn insert(&self, l2_slot: u32, block_id: u64, is_sequential: bool) {
        if (l2_slot as usize) >= self.max_slots {
            return;
        }
        let score = if is_sequential {
            self.sequential_penalty.load(Ordering::Relaxed)
        } else {
            self.initial_score.load(Ordering::Relaxed)
        };
        let mut inner = self.inner.write();
        let old = inner.slots[l2_slot as usize];
        if old.block_id != u64::MAX {
            inner.used.remove(&(old.score, l2_slot, old.block_id));
        }
        inner.slots[l2_slot as usize] = SlotEntry { block_id, score };
        inner.used.insert((score, l2_slot, block_id));
    }

    pub fn record_hit(&self, l2_slot: u32) {
        if (l2_slot as usize) >= self.max_slots {
            return;
        }
        let mut inner = self.inner.write();
        let old = inner.slots[l2_slot as usize];
        if old.block_id == u64::MAX {
            return;
        }
        let bonus = self.hit_bonus.load(Ordering::Relaxed);
        let new_score = old.score.saturating_add(bonus).min(SCORE_MAX);
        inner.used.remove(&(old.score, l2_slot, old.block_id));
        inner.slots[l2_slot as usize].score = new_score;
        inner.used.insert((new_score, l2_slot, old.block_id));
    }

    pub fn age_all(&self) {
        let mut inner = self.inner.write();
        let entries: Vec<(u32, u32, u64)> = inner.used.iter().copied().collect();
        inner.used.clear();
        for (score, slot_id, block_id) in entries {
            let new_score = score.saturating_sub(1);
            inner.slots[slot_id as usize].score = new_score;
            inner.used.insert((new_score, slot_id, block_id));
        }
    }

    pub fn evict_worst(&self) -> Option<(u32, u64)> {
        let mut inner = self.inner.write();
        let first = match inner.used.iter().next().copied() {
            Some(e) => e,
            None => return None,
        };
        inner.used.remove(&first);
        inner.slots[first.1 as usize] = SlotEntry::default();
        Some((first.1, first.2))
    }

    pub fn clear_slot(&self, l2_slot: u32) {
        if (l2_slot as usize) >= self.max_slots {
            return;
        }
        let mut inner = self.inner.write();
        let entry = inner.slots[l2_slot as usize];
        if entry.block_id != u64::MAX {
            inner.used.remove(&(entry.score, l2_slot, entry.block_id));
            inner.slots[l2_slot as usize] = SlotEntry::default();
        }
    }

    pub fn find_and_clear_block(&self, block_id: u64) -> Option<u32> {
        let mut inner = self.inner.write();
        for &(score, slot_id, bid) in inner.used.iter() {
            if bid == block_id {
                inner.used.remove(&(score, slot_id, block_id));
                inner.slots[slot_id as usize] = SlotEntry::default();
                return Some(slot_id);
            }
        }
        None
    }

    pub fn len(&self) -> usize {
        self.inner.read().used.len()
    }
}
