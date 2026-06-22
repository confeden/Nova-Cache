//! # Nova Cache Monitor
//!
//! System I/O monitoring subsystem using Event Tracing for Windows (ETW).
//! Captures disk I/O events in real-time to build access heatmaps and
//! detect sequential read patterns for intelligent prefetching.
//!
//! ## Modules
//!
//! - [`etw`]: ETW session management via `ferrisetw`.
//! - [`heatmap`]: Per-file and per-region access frequency tracking.
//! - [`prefetch`]: Sequential I/O detection and prefetch scheduling.

pub mod etw;
pub mod heatmap;
pub mod prefetch;
