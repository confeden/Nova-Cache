//! # Nova Cache Core
//!
//! The core caching engine for Nova Cache. Provides the foundational data structures
//! and algorithms for a two-tier disk cache (L1 RAM + L2 SSD) using the Adaptive
//! Replacement Cache (ARC) algorithm.
//!
//! ## Architecture
//!
//! - **ARC algorithm** ([`arc`]): Adaptive cache eviction balancing recency and frequency.
//! - **Block management** ([`block`]): Fixed-size block abstraction over cached data.
//! - **Memory pool** ([`pool`]): Pre-allocated, lock-free memory pool for L1 cache blocks.
//! - **SSD tier** ([`ssd_tier`]): Memory-mapped file-backed L2 cache on SSD.
//! - **Statistics** ([`stats`]): Real-time cache hit/miss/eviction counters.
//! - **Configuration** ([`config`]): Deserialized config from `nova_cache.toml`.

pub mod arc;
pub mod block;
pub mod config;
pub mod l2_pool;
pub mod l2_priority;
pub mod persistence;
pub mod pool;
pub mod predictor;
pub mod ssd_tier;
pub mod stats;
pub mod tinylfu;

pub use arc::ArcCache;
