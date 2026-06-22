//! # Adaptive Replacement Cache (ARC)
//!
//! A high-performance, thread-safe implementation of the ARC algorithm
//! by Megiddo & Modha (2003). ARC dynamically balances between recency
//! and frequency to achieve superior hit rates and scan resistance
//! compared to plain LRU.
//!
//! ## Design
//!
//! ARC maintains four lists:
//!
//! | List | Contents | Purpose |
//! |------|----------|---------|
//! | **T1** | Pages seen exactly once recently | Captures recency |
//! | **T2** | Pages seen at least twice recently | Captures frequency |
//! | **B1** | Ghost entries evicted from T1 | Guides adaptation toward recency |
//! | **B2** | Ghost entries evicted from T2 | Guides adaptation toward frequency |
//!
//! An adaptive parameter **p** controls the target size of T1 vs T2.
//! Ghost hits in B1 increase p (favor recency); ghost hits in B2
//! decrease p (favor frequency).
//!
//! ## Complexity
//!
//! All operations (get, insert, remove) are **O(1)** amortised, achieved
//! via a `HashMap` for lookup and intrusive doubly-linked lists for
//! ordering.
//!
//! ## Thread Safety
//!
//! The public API uses `&self` throughout, with a [`parking_lot::Mutex`]
//! guarding the interior mutable state. This allows the cache to be
//! shared behind an `Arc` without external synchronisation.
//!
//! # Examples
//!
//! ```
//! use nova_cache_core::ArcCache;
//!
//! let cache = ArcCache::new(100);
//! cache.insert("key", "value");
//! assert_eq!(cache.get(&"key"), Some("value"));
//! ```

use std::collections::HashMap;
use std::hash::Hash;

use parking_lot::Mutex;

// ---------------------------------------------------------------------------
// Intrusive doubly-linked list
// ---------------------------------------------------------------------------

/// Index into the node arena.
type Idx = usize;

/// Sentinel value meaning "no link".
const NONE: Idx = usize::MAX;

/// A node in the arena-backed doubly-linked list.
#[derive(Debug, Clone)]
struct Node<K> {
    key: K,
    prev: Idx,
    next: Idx,
}

/// Arena-allocated doubly-linked list providing O(1) push/remove/pop.
///
/// Nodes are allocated in a contiguous `Vec` and linked via indices.
/// Removed slots are pushed onto a free-list for reuse, keeping the
/// arena compact in the steady state.
#[derive(Debug, Clone)]
struct LinkedList<K> {
    nodes: Vec<Node<K>>,
    head: Idx, // MRU end
    tail: Idx, // LRU end
    len: usize,
    free: Vec<Idx>,
}

impl<K: Clone> LinkedList<K> {
    fn new() -> Self {
        Self {
            nodes: Vec::new(),
            head: NONE,
            tail: NONE,
            len: 0,
            free: Vec::new(),
        }
    }

    #[inline]
    fn len(&self) -> usize {
        self.len
    }

    #[inline]
    fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Push a key to the MRU (head) position. Returns its arena index.
    fn push_front(&mut self, key: K) -> Idx {
        let idx = self.alloc(key);
        self.link_front(idx);
        idx
    }

    /// Remove the LRU (tail) entry and return its key.
    fn pop_back(&mut self) -> Option<K> {
        if self.tail == NONE {
            return None;
        }
        let idx = self.tail;
        self.unlink(idx);
        Some(self.free_node(idx))
    }

    /// Remove the node at `idx` and return its key.
    fn remove(&mut self, idx: Idx) -> K {
        self.unlink(idx);
        self.free_node(idx)
    }

    /// Move an existing node to the MRU (head) position.
    fn move_to_front(&mut self, idx: Idx) {
        if self.head == idx {
            return; // already at front
        }
        self.unlink(idx);
        self.link_front(idx);
    }

    /// Clear the list, recycling all storage.
    fn clear(&mut self) {
        self.head = NONE;
        self.tail = NONE;
        self.len = 0;
        self.nodes.clear();
        self.free.clear();
    }

    /// Iterate over all keys in LRU order (MRU to LRU).
    fn keys(&self) -> impl Iterator<Item = &K> {
        let mut keys = Vec::new();
        let mut idx = self.head;
        while idx != NONE {
            keys.push(&self.nodes[idx].key);
            idx = self.nodes[idx].next;
        }
        keys.into_iter()
    }

    // -- internal helpers --

    /// Allocate a node (reuse from free-list or push new).
    fn alloc(&mut self, key: K) -> Idx {
        if let Some(idx) = self.free.pop() {
            self.nodes[idx] = Node {
                key,
                prev: NONE,
                next: NONE,
            };
            idx
        } else {
            let idx = self.nodes.len();
            self.nodes.push(Node {
                key,
                prev: NONE,
                next: NONE,
            });
            idx
        }
    }

    /// Return a node to the free-list and yield its key.
    fn free_node(&mut self, idx: Idx) -> K {
        self.free.push(idx);
        self.nodes[idx].key.clone()
    }

    /// Insert `idx` at the head (MRU) position.
    fn link_front(&mut self, idx: Idx) {
        self.nodes[idx].prev = NONE;
        self.nodes[idx].next = self.head;
        if self.head != NONE {
            self.nodes[self.head].prev = idx;
        }
        self.head = idx;
        if self.tail == NONE {
            self.tail = idx;
        }
        self.len += 1;
    }

    /// Remove `idx` from whatever position it currently occupies.
    fn unlink(&mut self, idx: Idx) {
        let prev = self.nodes[idx].prev;
        let next = self.nodes[idx].next;

        if prev != NONE {
            self.nodes[prev].next = next;
        } else {
            self.head = next;
        }

        if next != NONE {
            self.nodes[next].prev = prev;
        } else {
            self.tail = prev;
        }

        self.nodes[idx].prev = NONE;
        self.nodes[idx].next = NONE;
        self.len -= 1;
    }
}

// ---------------------------------------------------------------------------
// ARC statistics
// ---------------------------------------------------------------------------

/// Runtime statistics for an [`ArcCache`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ArcStats {
    /// Number of hits served from T1 (recency list).
    pub hits_t1: u64,
    /// Number of hits served from T2 (frequency list).
    pub hits_t2: u64,
    /// Number of cache misses.
    pub misses: u64,
    /// Number of ghost hits in B1 (recently evicted recency entries).
    pub ghost_hits_b1: u64,
    /// Number of ghost hits in B2 (recently evicted frequency entries).
    pub ghost_hits_b2: u64,
    /// Total number of evictions performed.
    pub evictions: u64,
}

impl ArcStats {
    /// Total number of requests (hits + misses).
    #[inline]
    pub fn total_requests(&self) -> u64 {
        self.hits_t1 + self.hits_t2 + self.misses
    }

    /// Hit rate as a fraction in `[0.0, 1.0]`.
    #[inline]
    pub fn hit_rate(&self) -> f64 {
        let total = self.total_requests();
        if total == 0 {
            return 0.0;
        }
        (self.hits_t1 + self.hits_t2) as f64 / total as f64
    }
}

// ---------------------------------------------------------------------------
// Which list a key lives in
// ---------------------------------------------------------------------------

/// Tracks which list a key currently resides in and its index in that
/// list's linked-list arena.
#[derive(Debug, Clone, Copy)]
enum Location {
    T1(Idx),
    T2(Idx),
    B1(Idx),
    B2(Idx),
}

// ---------------------------------------------------------------------------
// Inner mutable state
// ---------------------------------------------------------------------------

/// The mutable interior of an [`ArcCache`], protected by a [`Mutex`].
#[derive(Debug)]
struct Inner<K: Clone, V> {
    /// Maximum number of *real* cache entries (|T1| + |T2| <= capacity).
    capacity: usize,

    /// Adaptive target size for T1.  0 <= p <= capacity.
    p: usize,

    /// Monotonic tick counter for ghost entry timestamps.
    tick: u64,

    // -- data lists (hold actual values) --
    t1_list: LinkedList<K>,
    t2_list: LinkedList<K>,

    // -- ghost lists (metadata only, no values) --
    b1_list: LinkedList<K>,
    b2_list: LinkedList<K>,

    /// Maps every key present in any of the four lists to its
    /// [`Location`].
    directory: HashMap<K, Location>,

    /// Values for keys in T1 and T2.  Ghost entries (B1/B2) have no
    /// value stored here.
    values: HashMap<K, V>,

    /// Timestamp (monotonic tick) when each ghost entry was inserted.
    /// Used for TTL-based eviction of stale ghost entries.
    ghost_inserted_at: HashMap<K, u64>,

    /// Cumulative statistics.
    stats: ArcStats,
}

impl<K: Hash + Eq + Clone, V> Inner<K, V> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            p: 0,
            tick: 0,
            t1_list: LinkedList::new(),
            t2_list: LinkedList::new(),
            b1_list: LinkedList::new(),
            b2_list: LinkedList::new(),
            directory: HashMap::with_capacity(capacity.saturating_mul(2)),
            values: HashMap::with_capacity(capacity),
            ghost_inserted_at: HashMap::new(),
            stats: ArcStats::default(),
        }
    }

    /// Total number of cached entries with values (|T1| + |T2|).
    #[inline]
    fn len(&self) -> usize {
        self.t1_list.len() + self.t2_list.len()
    }

    // -----------------------------------------------------------------------
    // REPLACE subroutine  (Section III.B of the original paper)
    // -----------------------------------------------------------------------

    /// Evict one entry from T1 or T2 to make room, returning the
    /// evicted (key, value) pair.  The ghost of the evicted key is
    /// placed onto the appropriate ghost list.
    ///
    /// `in_b2` indicates whether the key being inserted was found in B2
    /// (used to break ties when |T1| == p).
    fn replace(&mut self, in_b2: bool) -> Option<(K, V)> {
        let t1_len = self.t1_list.len();
        if t1_len == 0 && self.t2_list.is_empty() {
            return None;
        }

        let evict_from_t1 = if t1_len > 0 {
            t1_len > self.p || (in_b2 && t1_len == self.p)
        } else {
            false
        };

        self.tick += 1;

        if evict_from_t1 {
            // Evict LRU of T1 -> ghost to MRU of B1
            if let Some(key) = self.t1_list.pop_back() {
                let value = self
                    .values
                    .remove(&key)
                    .expect("T1 entry must have a value");
                let ghost_idx = self.b1_list.push_front(key.clone());
                self.directory.insert(key.clone(), Location::B1(ghost_idx));
                self.ghost_inserted_at.insert(key.clone(), self.tick);
                self.stats.evictions += 1;
                Some((key, value))
            } else {
                None
            }
        } else {
            // Evict LRU of T2 -> ghost to MRU of B2
            if let Some(key) = self.t2_list.pop_back() {
                let value = self
                    .values
                    .remove(&key)
                    .expect("T2 entry must have a value");
                let ghost_idx = self.b2_list.push_front(key.clone());
                self.directory.insert(key.clone(), Location::B2(ghost_idx));
                self.ghost_inserted_at.insert(key.clone(), self.tick);
                self.stats.evictions += 1;
                Some((key, value))
            } else {
                None
            }
        }
    }

    // -----------------------------------------------------------------------
    // GET
    // -----------------------------------------------------------------------

    /// Look up `key`.  Returns a clone of the value on hit (after
    /// promoting to T2), or `None` on miss.
    fn get(&mut self, key: &K) -> Option<V>
    where
        V: Clone,
    {
        let location = self.directory.get(key).copied();
        match location {
            Some(Location::T1(idx)) => {
                // Hit in T1 -> move to MRU of T2
                self.t1_list.remove(idx);
                let t2_idx = self.t2_list.push_front(key.clone());
                self.directory.insert(key.clone(), Location::T2(t2_idx));
                self.stats.hits_t1 += 1;
                self.values.get(key).cloned()
            }
            Some(Location::T2(idx)) => {
                // Hit in T2 -> move to MRU of T2
                self.t2_list.move_to_front(idx);
                self.stats.hits_t2 += 1;
                self.values.get(key).cloned()
            }
            _ => {
                // Miss (not in T1 or T2; B1/B2 are ghosts with no value)
                self.stats.misses += 1;
                None
            }
        }
    }

    // -----------------------------------------------------------------------
    // INSERT
    // -----------------------------------------------------------------------

    /// Insert `key`/`value`.  Returns the evicted `(K, V)` if an
    /// eviction was necessary.
    fn insert(&mut self, key: K, value: V) -> Option<(K, V)> {
        if self.capacity == 0 {
            return None;
        }

        // -- Case 1: key already in T1 or T2 (update in place) --
        if let Some(loc) = self.directory.get(&key).copied() {
            match loc {
                Location::T1(idx) => {
                    // Promote to MRU of T2 and update value.
                    self.t1_list.remove(idx);
                    let t2_idx = self.t2_list.push_front(key.clone());
                    self.directory.insert(key.clone(), Location::T2(t2_idx));
                    self.values.insert(key, value);
                    return None;
                }
                Location::T2(idx) => {
                    // Refresh in T2 and update value.
                    self.t2_list.move_to_front(idx);
                    self.values.insert(key, value);
                    return None;
                }
                Location::B1(idx) => {
                    // -- Case 2: ghost hit in B1 --
                    self.stats.ghost_hits_b1 += 1;

                    // Adapt p upward (favor recency).
                    let b1_len = self.b1_list.len();
                    let b2_len = self.b2_list.len();
                    let delta = std::cmp::max(1, b2_len.checked_div(b1_len).unwrap_or(1));
                    self.p = std::cmp::min(self.p.saturating_add(delta), self.capacity);

                    let evicted = self.replace(false);

                    // Remove ghost from B1.
                    self.b1_list.remove(idx);

                    // Insert at MRU of T2 with new value.
                    let t2_idx = self.t2_list.push_front(key.clone());
                    self.directory.insert(key.clone(), Location::T2(t2_idx));
                    self.values.insert(key, value);

                    return evicted;
                }
                Location::B2(idx) => {
                    // -- Case 3: ghost hit in B2 --
                    self.stats.ghost_hits_b2 += 1;

                    // Adapt p downward (favor frequency).
                    let b1_len = self.b1_list.len();
                    let b2_len = self.b2_list.len();
                    let delta = std::cmp::max(1, b1_len.checked_div(b2_len).unwrap_or(1));
                    self.p = self.p.saturating_sub(delta);

                    let evicted = self.replace(true);

                    // Remove ghost from B2.
                    self.b2_list.remove(idx);

                    // Insert at MRU of T2 with new value.
                    let t2_idx = self.t2_list.push_front(key.clone());
                    self.directory.insert(key.clone(), Location::T2(t2_idx));
                    self.values.insert(key, value);

                    return evicted;
                }
            }
        }

        // -- Case 4: complete miss --
        self.stats.misses += 1;

        let t1b1 = self.t1_list.len() + self.b1_list.len();
        let evicted;

        if t1b1 == self.capacity {
            // Case 4a
            if self.t1_list.len() < self.capacity {
                // Delete LRU of B1, then REPLACE.
                if let Some(ghost_key) = self.b1_list.pop_back() {
                    self.directory.remove(&ghost_key);
                    self.ghost_inserted_at.remove(&ghost_key);
                }
                evicted = self.replace(false);
            } else {
                // |T1| == c  ->  delete LRU of T1 entirely (no ghost).
                evicted = if let Some(lru_key) = self.t1_list.pop_back() {
                    let lru_val = self
                        .values
                        .remove(&lru_key)
                        .expect("T1 entry must have value");
                    self.directory.remove(&lru_key);
                    self.stats.evictions += 1;
                    Some((lru_key, lru_val))
                } else {
                    None
                };
            }
        } else {
            // Case 4b: t1b1 < capacity
            let total =
                self.t1_list.len() + self.t2_list.len() + self.b1_list.len() + self.b2_list.len();
            if total >= self.capacity {
                // If total == 2c, delete LRU of B2.
                if total >= 2 * self.capacity {
                    if let Some(ghost_key) = self.b2_list.pop_back() {
                        self.directory.remove(&ghost_key);
                        self.ghost_inserted_at.remove(&ghost_key);
                    }
                }
                evicted = self.replace(false);
            } else {
                evicted = None;
            }
        }

        // Insert x at MRU of T1.
        let t1_idx = self.t1_list.push_front(key.clone());
        self.directory.insert(key.clone(), Location::T1(t1_idx));
        self.values.insert(key, value);

        evicted
    }

    /// Insert `key`/`value` directly at MRU of T2 (hot-file priority boost).
    /// Used for blocks belonging to frequently-accessed files.
    /// Returns the evicted `(K, V)` if an eviction was necessary.
    fn insert_t2(&mut self, key: K, value: V) -> Option<(K, V)> {
        if self.capacity == 0 {
            return None;
        }

        // If already cached, just update the value.
        if let Some(loc) = self.directory.get(&key).cloned() {
            match loc {
                Location::T1(idx) => {
                    self.t1_list.remove(idx);
                    let t2_idx = self.t2_list.push_front(key.clone());
                    self.directory.insert(key.clone(), Location::T2(t2_idx));
                    self.values.insert(key, value);
                    return None;
                }
                Location::T2(idx) => {
                    self.t2_list.move_to_front(idx);
                    self.values.insert(key, value);
                    return None;
                }
                _ => {}
            }
        }

        // Ghost hit in B2 → adapt p upward.
        if let Some(Location::B2(idx)) = self.directory.get(&key).cloned() {
            self.b2_list.remove(idx);
            self.directory.remove(&key);
            self.ghost_inserted_at.remove(&key);

            let b1_len = self.b1_list.len();
            let b2_len = self.b2_list.len();
            let delta = std::cmp::max(1, b2_len.checked_div(b1_len).unwrap_or(1));
            self.p = std::cmp::min(self.capacity, self.p + delta);

            let evicted = self.replace(true);
            let t2_idx = self.t2_list.push_front(key.clone());
            self.directory.insert(key.clone(), Location::T2(t2_idx));
            self.values.insert(key, value);
            return evicted;
        }

        // Ghost hit in B1 → adapt p upward.
        if let Some(Location::B1(idx)) = self.directory.get(&key).cloned() {
            self.b1_list.remove(idx);
            self.directory.remove(&key);
            self.ghost_inserted_at.remove(&key);

            let b1_len = self.b1_list.len();
            let b2_len = self.b2_list.len();
            let delta = std::cmp::max(1, b1_len.checked_div(b2_len).unwrap_or(1));
            self.p = std::cmp::min(self.capacity, self.p + delta);

            let evicted = self.replace(true);
            let t2_idx = self.t2_list.push_front(key.clone());
            self.directory.insert(key.clone(), Location::T2(t2_idx));
            self.values.insert(key, value);
            return evicted;
        }

        // Complete miss — insert directly into T2 (priority boost).
        self.stats.misses += 1;

        // Ensure |T2| <= capacity.
        let total = self.t1_list.len() + self.t2_list.len();
        let evicted = if total >= self.capacity {
            self.replace(false)
        } else {
            None
        };

        let t2_idx = self.t2_list.push_front(key.clone());
        self.directory.insert(key.clone(), Location::T2(t2_idx));
        self.values.insert(key, value);

        evicted
    }

    // -----------------------------------------------------------------------
    // REMOVE
    // -----------------------------------------------------------------------

    /// Remove `key` from the cache entirely.  Returns the value if it
    /// was cached (T1 or T2).  Ghost entries are also removed.
    fn remove(&mut self, key: &K) -> Option<V> {
        if let Some(loc) = self.directory.remove(key) {
            match loc {
                Location::T1(idx) => {
                    self.t1_list.remove(idx);
                    self.values.remove(key)
                }
                Location::T2(idx) => {
                    self.t2_list.remove(idx);
                    self.values.remove(key)
                }
                Location::B1(idx) => {
                    self.b1_list.remove(idx);
                    self.ghost_inserted_at.remove(key);
                    None // ghosts hold no value
                }
                Location::B2(idx) => {
                    self.b2_list.remove(idx);
                    self.ghost_inserted_at.remove(key);
                    None
                }
            }
        } else {
            None
        }
    }

    // -----------------------------------------------------------------------
    // CLEAR / RESIZE
    // -----------------------------------------------------------------------

    fn clear(&mut self) {
        self.t1_list.clear();
        self.t2_list.clear();
        self.b1_list.clear();
        self.b2_list.clear();
        self.directory.clear();
        self.values.clear();
        self.ghost_inserted_at.clear();
        self.p = 0;
    }

    fn resize(&mut self, new_capacity: usize) {
        // Evict excess entries.
        while self.len() > new_capacity {
            self.replace(false);
        }
        // Trim ghost lists so |B1|+|T1| <= c and total <= 2c.
        while self.t1_list.len() + self.b1_list.len() > new_capacity {
            if let Some(k) = self.b1_list.pop_back() {
                self.directory.remove(&k);
                self.ghost_inserted_at.remove(&k);
            } else {
                break;
            }
        }
        while self.t1_list.len() + self.t2_list.len() + self.b1_list.len() + self.b2_list.len()
            > 2 * new_capacity
        {
            if let Some(k) = self.b2_list.pop_back() {
                self.directory.remove(&k);
                self.ghost_inserted_at.remove(&k);
            } else {
                break;
            }
        }
        self.capacity = new_capacity;
        self.p = std::cmp::min(self.p, new_capacity);
    }
}

// ---------------------------------------------------------------------------
// Ghost TTL eviction
// ---------------------------------------------------------------------------

impl<K: Hash + Eq + Clone, V> Inner<K, V> {
    /// Purge ghost entries older than `ttl_ticks` ticks from B1 and B2.
    /// Returns the number of entries purged.
    fn purge_expired_ghosts(&mut self, ttl_ticks: u64) -> u64 {
        if ttl_ticks == 0 {
            return 0;
        }
        let cutoff = self.tick.saturating_sub(ttl_ticks);
        let mut purged = 0u64;

        // Collect expired B1 keys (iterate LRU to MRU)
        let expired_b1: Vec<K> = self
            .b1_list
            .keys()
            .filter(|k| self.ghost_inserted_at.get(*k).copied().unwrap_or(0) < cutoff)
            .cloned()
            .collect();
        for key in expired_b1 {
            if let Some(idx) = self.directory.remove(&key) {
                match idx {
                    Location::B1(node_idx) => {
                        self.b1_list.remove(node_idx);
                        self.ghost_inserted_at.remove(&key);
                        purged += 1;
                    }
                    _ => {}
                }
            }
        }

        // Collect expired B2 keys
        let expired_b2: Vec<K> = self
            .b2_list
            .keys()
            .filter(|k| self.ghost_inserted_at.get(*k).copied().unwrap_or(0) < cutoff)
            .cloned()
            .collect();
        for key in expired_b2 {
            if let Some(idx) = self.directory.remove(&key) {
                match idx {
                    Location::B2(node_idx) => {
                        self.b2_list.remove(node_idx);
                        self.ghost_inserted_at.remove(&key);
                        purged += 1;
                    }
                    _ => {}
                }
            }
        }

        purged
    }
}

// ---------------------------------------------------------------------------
// Public API  --  ArcCache<K, V>
// ---------------------------------------------------------------------------

/// A thread-safe Adaptive Replacement Cache.
///
/// All public methods take `&self` and use interior mutability
/// ([`parking_lot::Mutex`]) so the cache can be shared behind an
/// `Arc<ArcCache<K, V>>` without further synchronisation.
///
/// # Type Parameters
///
/// * `K` -- Key type.  Must be `Hash + Eq + Clone + Send`.
/// * `V` -- Value type.  Must be `Clone + Send`.
///
/// Values are cloned on [`get`](Self::get) because the internal mutex
/// cannot hand out references that outlive the lock guard.  If cloning
/// is expensive, wrap the value in `Arc<V>`.
///
/// # Examples
///
/// ```
/// use nova_cache_core::ArcCache;
///
/// let cache = ArcCache::new(2);
/// cache.insert(1, "one");
/// cache.insert(2, "two");
///
/// assert_eq!(cache.get(&1), Some("one"));
/// assert_eq!(cache.len(), 2);
///
/// // Third insert evicts one entry.
/// cache.insert(3, "three");
/// assert_eq!(cache.len(), 2);
/// ```
pub struct ArcCache<K: Clone, V> {
    inner: Mutex<Inner<K, V>>,
}

// Manual Debug because we don't want to require V: Debug.
impl<K: Hash + Eq + Clone + std::fmt::Debug, V> std::fmt::Debug for ArcCache<K, V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.inner.lock();
        f.debug_struct("ArcCache")
            .field("capacity", &inner.capacity)
            .field("len", &inner.len())
            .field("p", &inner.p)
            .field("|T1|", &inner.t1_list.len())
            .field("|T2|", &inner.t2_list.len())
            .field("|B1|", &inner.b1_list.len())
            .field("|B2|", &inner.b2_list.len())
            .finish()
    }
}

impl<K, V> ArcCache<K, V>
where
    K: Hash + Eq + Clone + Send,
    V: Clone + Send,
{
    /// Create a new ARC cache with the given capacity.
    ///
    /// `capacity` is the maximum number of entries that hold actual
    /// values (i.e. |T1| + |T2| <= `capacity`).  Ghost entries (B1, B2)
    /// may use additional memory for metadata.
    ///
    /// A capacity of 0 is allowed but the cache will never store
    /// anything.
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(Inner::new(capacity)),
        }
    }

    /// Look up `key` in the cache.
    ///
    /// On a hit the entry is promoted according to ARC rules (T1->T2 on
    /// second access, or refreshed within T2), and a **clone** of the
    /// value is returned.  Returns `None` on a miss.
    ///
    /// # Performance Note
    ///
    /// The value is cloned while holding the internal lock.  Wrap
    /// expensive values in `Arc<V>` to keep the clone cheap.
    pub fn get(&self, key: &K) -> Option<V> {
        self.inner.lock().get(key)
    }

    /// Insert a key-value pair into the cache.
    ///
    /// If the key already exists its value is updated and the entry is
    /// promoted.  If the cache is full an entry is evicted according to
    /// the ARC policy; the evicted `(key, value)` is returned.
    pub fn insert(&self, key: K, value: V) -> Option<(K, V)> {
        self.inner.lock().insert(key, value)
    }

    /// Insert `key`/`value` directly at MRU of T2 (hot-file priority boost).
    /// Blocks from frequently-accessed files are placed in T2 directly,
    /// giving them longer cache residency and higher eviction priority.
    pub fn insert_t2(&self, key: K, value: V) -> Option<(K, V)> {
        self.inner.lock().insert_t2(key, value)
    }

    /// Remove `key` from the cache (including ghost entries).
    ///
    /// Returns the value if the key was present in T1 or T2.  Ghost
    /// entries are removed silently.
    pub fn remove(&self, key: &K) -> Option<V> {
        self.inner.lock().remove(key)
    }

    /// Return a snapshot of the current cache statistics.
    pub fn stats(&self) -> ArcStats {
        self.inner.lock().stats
    }

    /// Number of entries currently holding values (|T1| + |T2|).
    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }

    /// Whether the cache holds zero value-bearing entries.
    pub fn is_empty(&self) -> bool {
        self.inner.lock().len() == 0
    }

    /// Remove all entries (values and ghosts) and reset p to 0.
    ///
    /// Statistics are **not** reset.
    pub fn clear(&self) {
        self.inner.lock().clear();
    }

    /// Purge ghost list entries older than `ttl_ticks`.
    /// Returns the number of entries purged.
    pub fn purge_ghosts(&self, ttl_ticks: u64) -> u64 {
        self.inner.lock().purge_expired_ghosts(ttl_ticks)
    }

    /// Change the cache capacity.
    ///
    /// If `new_capacity` is smaller than the current number of cached
    /// entries, excess entries are evicted according to normal ARC
    /// policy.  Ghost lists are also trimmed.
    pub fn resize(&self, new_capacity: usize) {
        self.inner.lock().resize(new_capacity);
    }

    /// Return the current cache capacity.
    pub fn capacity(&self) -> usize {
        self.inner.lock().capacity
    }

    /// Return the current adaptive parameter **p** (target size of T1).
    ///
    /// Useful for diagnostics and tests.
    pub fn p(&self) -> usize {
        self.inner.lock().p
    }

    /// Return a snapshot of all cached entries as `(key, value)` pairs.
    ///
    /// Useful for persisting cache state to disk.
    pub fn entries(&self) -> Vec<(K, V)> {
        let inner = self.inner.lock();
        inner
            .values
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Restore the cache from a set of pre-existing entries.
    ///
    /// All entries are inserted as T1 (recency). The cache must be empty
    /// or have been cleared before calling this. Ghost lists and p are reset.
    pub fn restore_entries(&self, entries: Vec<(K, V)>) {
        let mut inner = self.inner.lock();
        inner.clear();
        for (key, value) in entries {
            if inner.capacity == 0 {
                break;
            }
            // Insert at MRU of T1
            let t1_idx = inner.t1_list.push_front(key.clone());
            inner.directory.insert(key.clone(), Location::T1(t1_idx));
            inner.values.insert(key, value);
        }
    }
}

// SAFETY: The inner state is protected by a Mutex; K and V are Send.
unsafe impl<K: Clone + Send, V: Send> Send for ArcCache<K, V> {}
unsafe impl<K: Clone + Send, V: Send> Sync for ArcCache<K, V> {}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // 1. Basic insert and get
    // -----------------------------------------------------------------------

    #[test]
    fn basic_insert_and_get() {
        let cache = ArcCache::new(4);
        cache.insert("a", 1);
        cache.insert("b", 2);
        cache.insert("c", 3);
        cache.insert("d", 4);

        assert_eq!(cache.get(&"a"), Some(1));
        assert_eq!(cache.get(&"b"), Some(2));
        assert_eq!(cache.get(&"c"), Some(3));
        assert_eq!(cache.get(&"d"), Some(4));
        assert_eq!(cache.get(&"z"), None);
        assert_eq!(cache.len(), 4);
    }

    // -----------------------------------------------------------------------
    // 2. Eviction when cache is full
    // -----------------------------------------------------------------------

    #[test]
    fn eviction_when_full() {
        let cache = ArcCache::new(3);
        cache.insert(1, "a");
        cache.insert(2, "b");
        cache.insert(3, "c");
        assert_eq!(cache.len(), 3);

        // This should evict one entry.
        let evicted = cache.insert(4, "d");
        assert!(evicted.is_some());
        assert_eq!(cache.len(), 3);

        // The LRU of T1 (key=1) should have been evicted.
        assert_eq!(cache.get(&1), None);
        assert_eq!(cache.get(&4), Some("d"));
    }

    // -----------------------------------------------------------------------
    // 3. T1 -> T2 promotion on second access
    // -----------------------------------------------------------------------

    #[test]
    fn promotion_t1_to_t2() {
        let cache = ArcCache::new(4);
        cache.insert("x", 10);

        // First get promotes from T1 to T2.
        assert_eq!(cache.get(&"x"), Some(10));

        let stats = cache.stats();
        assert_eq!(stats.hits_t1, 1);
        assert_eq!(stats.hits_t2, 0);

        // Second get is a T2 hit.
        assert_eq!(cache.get(&"x"), Some(10));
        let stats = cache.stats();
        assert_eq!(stats.hits_t1, 1);
        assert_eq!(stats.hits_t2, 1);
    }

    // -----------------------------------------------------------------------
    // 4. B1 ghost hit -> p increases (favors recency)
    // -----------------------------------------------------------------------

    #[test]
    fn b1_ghost_hit_increases_p() {
        // Use capacity 3 so that when |T1| < c, evicted entries go
        // to B1 as ghosts (via REPLACE).
        let cache = ArcCache::new(3);

        // Fill T1: T1 = [3, 2, 1]
        cache.insert(1, "a");
        cache.insert(2, "b");
        cache.insert(3, "c");

        // Promote key 1 to T2 so |T1| < c.
        cache.get(&1); // T1=[3,2], T2=[1]

        // Insert 4: t1b1=2 < c=3, total=3>=c -> REPLACE.
        //   REPLACE: |T1|=2 > p=0 -> evict T1 LRU (key 2) to B1.
        //   T1=[4,3], T2=[1], B1=[2]
        cache.insert(4, "d");

        // Insert 5: t1b1 = |T1|=2 + |B1|=1 = 3 = c, |T1|=2 < c=3
        //   -> delete LRU of B1 (key 2), then REPLACE.
        //   REPLACE: |T1|=2 > p=0 -> evict T1 LRU (key 3) to B1.
        //   T1=[5,4], T2=[1], B1=[3]
        cache.insert(5, "e");

        let p_before = cache.p();

        // Re-insert key 3 -> B1 ghost hit -> p increases.
        cache.insert(3, "c_new");
        let p_after = cache.p();
        assert!(
            p_after > p_before,
            "p should increase on B1 ghost hit: before={p_before}, after={p_after}"
        );

        let stats = cache.stats();
        assert!(stats.ghost_hits_b1 > 0, "should record B1 ghost hit");
    }

    // -----------------------------------------------------------------------
    // 5. B2 ghost hit -> p decreases (favors frequency)
    // -----------------------------------------------------------------------

    #[test]
    fn b2_ghost_hit_decreases_p() {
        let cache = ArcCache::new(2);

        // Insert and promote key 1 to T2 by accessing it twice.
        cache.insert(1, "a");
        cache.get(&1); // T1 -> T2

        // Insert key 2 into T1.
        cache.insert(2, "b");
        // State: T2=[1], T1=[2]

        // Insert 3: |T1|=1 > p=0 -> evict T1(2) to B1.
        cache.insert(3, "c");
        // T1=[3], T2=[1], B1=[2]

        // Insert 4: |T1|=1 > p=0 -> evict T1(3) to B1.
        cache.insert(4, "d");
        // T1=[4], T2=[1], B1=[3,2]

        // Access 4 to promote to T2.
        cache.get(&4);
        // T1=[], T2=[4,1], B1=[3,2]

        // Insert 5: evicts from T2 (since T1 empty). Key 1 -> B2.
        cache.insert(5, "e");
        // T1=[5], T2=[4], B2=[1], B1=[3,2]

        // Insert 6: |T1|=1 > p (still 0) -> evict T1(5) to B1.
        cache.insert(6, "f");
        // T1=[6], T2=[4], B1=[5,3,2], B2=[1]

        let p_before = cache.p();

        // Re-insert key 1 -> B2 ghost hit -> p should decrease (or
        // stay 0).
        cache.insert(1, "a_again");
        let p_after = cache.p();

        assert!(
            p_after <= p_before,
            "p should not increase on B2 ghost hit: before={p_before}, after={p_after}"
        );

        let stats = cache.stats();
        assert!(stats.ghost_hits_b2 > 0, "should record B2 ghost hit");
    }

    // -----------------------------------------------------------------------
    // 6. Scan resistance
    // -----------------------------------------------------------------------

    #[test]
    fn scan_resistance() {
        // ARC should protect frequently-used entries from being evicted
        // by a one-pass scan of many keys.
        let capacity = 100;
        let cache = ArcCache::new(capacity);

        // Populate "hot" keys and access each twice to promote to T2.
        for i in 0..50 {
            cache.insert(i, i * 10);
            cache.get(&i); // promote to T2
        }

        // Fill remaining T1 slots.
        for i in 50..100 {
            cache.insert(i, i * 10);
        }

        // "Scan" 500 cold keys -- these should NOT evict all hot keys.
        for i in 1000..1500 {
            cache.insert(i, i);
        }

        // At least some of the hot keys (0..50, in T2) should survive.
        let hot_surviving: usize = (0..50).filter(|k| cache.get(k).is_some()).count();
        assert!(
            hot_surviving > 20,
            "ARC should protect T2 entries from scans; only {hot_surviving}/50 survived"
        );
    }

    // -----------------------------------------------------------------------
    // 7. Statistics accuracy
    // -----------------------------------------------------------------------

    #[test]
    fn statistics_accuracy() {
        let cache = ArcCache::new(2);

        // Two explicit get-misses.
        assert_eq!(cache.get(&"x"), None);
        assert_eq!(cache.get(&"y"), None);

        cache.insert("a", 1);
        cache.insert("b", 2);

        // Hit T1.
        cache.get(&"a");
        // Hit T2 (a was promoted).
        cache.get(&"a");

        let s = cache.stats();
        assert_eq!(s.hits_t1, 1, "expected 1 T1 hit");
        assert_eq!(s.hits_t2, 1, "expected 1 T2 hit");
        // 2 explicit get-misses + miss counters from inserts.
        assert!(
            s.misses >= 2,
            "expected at least 2 misses, got {}",
            s.misses
        );
        assert!(s.hit_rate() > 0.0);
    }

    // -----------------------------------------------------------------------
    // 8. Thread safety
    // -----------------------------------------------------------------------

    #[test]
    fn thread_safety() {
        use std::sync::Arc;
        use std::thread;

        let cache = Arc::new(ArcCache::new(256));
        let mut handles = Vec::new();

        for t in 0..8 {
            let cache = Arc::clone(&cache);
            handles.push(thread::spawn(move || {
                let base = t * 1000;
                for i in base..base + 500 {
                    cache.insert(i, i);
                }
                for i in base..base + 500 {
                    let _ = cache.get(&i);
                }
            }));
        }

        for h in handles {
            h.join().expect("thread panicked");
        }

        // Cache should be in a consistent state.
        assert!(cache.len() <= 256);
        let stats = cache.stats();
        assert!(stats.total_requests() > 0);
    }

    // -----------------------------------------------------------------------
    // 9. Edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn capacity_zero() {
        let cache = ArcCache::<i32, i32>::new(0);
        assert!(cache.insert(1, 10).is_none());
        assert_eq!(cache.get(&1), None);
        assert!(cache.is_empty());
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn capacity_one() {
        let cache = ArcCache::new(1);
        cache.insert("a", 1);
        assert_eq!(cache.get(&"a"), Some(1));
        assert_eq!(cache.len(), 1);

        // Inserting a second key evicts the first.
        cache.insert("b", 2);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.get(&"a"), None);
        assert_eq!(cache.get(&"b"), Some(2));
    }

    #[test]
    fn duplicate_inserts_update_value() {
        let cache = ArcCache::new(4);
        cache.insert("k", 1);
        assert_eq!(cache.get(&"k"), Some(1));

        cache.insert("k", 42);
        assert_eq!(cache.get(&"k"), Some(42));
        // No extra entry created.
        assert_eq!(cache.len(), 1);
    }

    // -----------------------------------------------------------------------
    // 10. Resize behaviour
    // -----------------------------------------------------------------------

    #[test]
    fn resize_shrink() {
        let cache = ArcCache::new(10);
        for i in 0..10 {
            cache.insert(i, i * 100);
        }
        assert_eq!(cache.len(), 10);

        cache.resize(5);
        assert!(cache.len() <= 5);
        assert_eq!(cache.capacity(), 5);
    }

    #[test]
    fn resize_grow() {
        let cache = ArcCache::new(3);
        for i in 0..3 {
            cache.insert(i, i);
        }
        cache.resize(10);
        assert_eq!(cache.capacity(), 10);
        assert_eq!(cache.len(), 3);

        // Now we can add more without eviction.
        for i in 3..10 {
            cache.insert(i, i);
        }
        assert_eq!(cache.len(), 10);
    }

    // -----------------------------------------------------------------------
    // Additional: remove
    // -----------------------------------------------------------------------

    #[test]
    fn remove_existing_key() {
        let cache = ArcCache::new(4);
        cache.insert(1, "one");
        cache.insert(2, "two");

        assert_eq!(cache.remove(&1), Some("one"));
        assert_eq!(cache.get(&1), None);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn remove_nonexistent_key() {
        let cache = ArcCache::new(4);
        cache.insert(1, "one");
        assert_eq!(cache.remove(&99), None);
        assert_eq!(cache.len(), 1);
    }

    // -----------------------------------------------------------------------
    // Additional: clear
    // -----------------------------------------------------------------------

    #[test]
    fn clear_resets_cache() {
        let cache = ArcCache::new(8);
        for i in 0..8 {
            cache.insert(i, i);
        }
        assert_eq!(cache.len(), 8);

        cache.clear();
        assert!(cache.is_empty());
        assert_eq!(cache.len(), 0);

        // Should still work after clear.
        cache.insert(100, 100);
        assert_eq!(cache.get(&100), Some(100));
    }

    // -----------------------------------------------------------------------
    // Additional: ArcStats helper methods
    // -----------------------------------------------------------------------

    #[test]
    fn stats_hit_rate() {
        let s = ArcStats {
            hits_t1: 30,
            hits_t2: 20,
            misses: 50,
            ghost_hits_b1: 0,
            ghost_hits_b2: 0,
            evictions: 0,
        };
        assert_eq!(s.total_requests(), 100);
        assert!((s.hit_rate() - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn stats_hit_rate_zero_requests() {
        let s = ArcStats::default();
        assert_eq!(s.hit_rate(), 0.0);
    }

    // -----------------------------------------------------------------------
    // Linked list unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn linked_list_push_pop() {
        let mut list = LinkedList::new();
        list.push_front(1);
        list.push_front(2);
        list.push_front(3);
        assert_eq!(list.len(), 3);

        assert_eq!(list.pop_back(), Some(1));
        assert_eq!(list.pop_back(), Some(2));
        assert_eq!(list.pop_back(), Some(3));
        assert_eq!(list.pop_back(), None);
        assert!(list.is_empty());
    }

    #[test]
    fn linked_list_remove_middle() {
        let mut list = LinkedList::new();
        list.push_front(1);
        let mid = list.push_front(2);
        list.push_front(3);
        // Order: 3 -> 2 -> 1

        let key = list.remove(mid);
        assert_eq!(key, 2);
        assert_eq!(list.len(), 2);

        // Remaining: 3 -> 1
        assert_eq!(list.pop_back(), Some(1));
        assert_eq!(list.pop_back(), Some(3));
    }

    #[test]
    fn linked_list_move_to_front() {
        let mut list = LinkedList::new();
        let a = list.push_front(1);
        list.push_front(2);
        list.push_front(3);
        // Order: 3 -> 2 -> 1

        list.move_to_front(a);
        // Order: 1 -> 3 -> 2

        assert_eq!(list.pop_back(), Some(2));
        assert_eq!(list.pop_back(), Some(3));
        assert_eq!(list.pop_back(), Some(1));
    }

    // -----------------------------------------------------------------------
    // Stress test with pseudo-random access pattern
    // -----------------------------------------------------------------------

    #[test]
    fn stress_random_access() {
        use rand::Rng;

        let cache = ArcCache::new(64);
        let mut rng = rand::rng();

        for _ in 0..10_000 {
            let key: u32 = rng.random_range(0..256);
            if rng.random_bool(0.5) {
                cache.insert(key, key as u64);
            } else {
                let _ = cache.get(&key);
            }
        }

        assert!(cache.len() <= 64);
        let stats = cache.stats();
        assert!(stats.total_requests() > 0);
    }
}
