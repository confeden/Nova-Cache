//! # Configuration Module
//!
//! Handles reading, writing, and validating Nova Cache configuration.
//!
//! Configuration is stored in TOML format and supports:
//! - Cache sizing (L1 RAM and L2 SSD)
//! - Block size selection
//! - Write policy (write-through or write-back)
//! - Game Mode settings (process priority, auto-detection)
//! - Prefetch settings (sequential detection, read-ahead)
//! - KDU settings (provider selection, paths)

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Configuration errors.
#[derive(Error, Debug)]
pub enum ConfigError {
    #[error("Failed to read config file: {0}")]
    ReadError(#[from] std::io::Error),

    #[error("Failed to parse config: {0}")]
    ParseError(#[from] toml::de::Error),

    #[error("Failed to serialize config: {0}")]
    SerializeError(#[from] toml::ser::Error),

    #[error("Validation error: {0}")]
    ValidationError(String),
}

/// Top-level Nova Cache configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NovaCacheConfig {
    /// General settings.
    #[serde(default)]
    pub general: GeneralConfig,

    /// Cache settings.
    #[serde(default)]
    pub cache: CacheConfig,

    /// Game Mode settings.
    #[serde(default)]
    pub game_mode: GameModeConfig,

    /// Prefetch settings.
    #[serde(default)]
    pub prefetch: PrefetchConfig,

    /// KDU (driver loader) settings.
    #[serde(default)]
    pub kdu: KduConfig,

    /// Volumes to cache.
    #[serde(default)]
    pub volumes: Vec<VolumeConfig>,
}

/// General settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneralConfig {
    /// Log level: trace, debug, info, warn, error.
    #[serde(default = "default_log_level")]
    pub log_level: String,

    /// Automatically start caching on service startup.
    #[serde(default = "default_true")]
    pub auto_start: bool,

    /// Path to the log file.
    #[serde(default = "default_log_path")]
    pub log_path: PathBuf,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            log_level: default_log_level(),
            auto_start: true,
            log_path: default_log_path(),
        }
    }
}

/// Cache settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheConfig {
    /// L1 (RAM) cache size in megabytes.
    #[serde(default = "default_l1_size_mb")]
    pub l1_size_mb: u32,

    /// Block size in kilobytes. Must be a power of 2, between 4 and 1024.
    #[serde(default = "default_block_size_kb")]
    pub block_size_kb: u32,

    /// Cache replacement algorithm.
    #[serde(default = "default_algorithm")]
    pub algorithm: CacheAlgorithm,

    /// L2 (SSD) cache settings.
    #[serde(default)]
    pub l2: L2Config,

    /// Base flush interval in milliseconds (minimum).
    /// Actual interval adapts based on dirty block count.
    #[serde(default = "default_flush_interval_ms")]
    pub flush_interval_ms: u64,

    /// Maximum flush interval in milliseconds (when cache is idle).
    #[serde(default = "default_flush_interval_max_ms")]
    pub flush_interval_max_ms: u64,

    /// Dirty block count threshold to trigger aggressive flushing.
    #[serde(default = "default_flush_dirty_threshold")]
    pub flush_dirty_threshold: u32,

    /// Maximum number of dirty blocks allowed before backpressure kicks in.
    /// When exceeded, the flush thread flushes all dirty blocks immediately.
    #[serde(default = "default_max_dirty_blocks")]
    pub max_dirty_blocks: usize,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            l1_size_mb: default_l1_size_mb(),
            block_size_kb: default_block_size_kb(),
            algorithm: CacheAlgorithm::Arc,
            l2: L2Config::default(),
            flush_interval_ms: default_flush_interval_ms(),
            flush_interval_max_ms: default_flush_interval_max_ms(),
            flush_dirty_threshold: default_flush_dirty_threshold(),
            max_dirty_blocks: default_max_dirty_blocks(),
        }
    }
}

/// L2 (SSD) cache settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct L2Config {
    /// Whether L2 cache is enabled.
    #[serde(default = "default_true")]
    pub enable: bool,

    /// Path to the SSD cache file (primary backend).
    #[serde(default = "default_l2_path")]
    pub path: PathBuf,

    /// L2 cache size in gigabytes (total across all backends).
    #[serde(default = "default_l2_size_gb")]
    pub size_gb: u32,

    /// Additional L2 backend paths. Each gets size_gb/len(backends+1) space.
    /// The primary `path` is always included as the first backend.
    #[serde(default)]
    pub backends: Vec<PathBuf>,

    /// Whether to cache sequential reads in L2.
    #[serde(default = "default_true")]
    pub cache_sequential: bool,
}

impl Default for L2Config {
    fn default() -> Self {
        Self {
            enable: true,
            path: default_l2_path(),
            size_gb: default_l2_size_gb(),
            backends: Vec::new(),
            cache_sequential: true,
        }
    }
}

/// Cache replacement algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheAlgorithm {
    /// Adaptive Replacement Cache — best general-purpose algorithm.
    Arc,
    /// Least Recently Used — simpler but less scan-resistant.
    Lru,
}

/// Game Mode configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameModeConfig {
    /// Enable Game Mode.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Priority boost multiplier for game process I/O in cache.
    /// Values > 1.0 make game blocks harder to evict.
    #[serde(default = "default_priority_boost")]
    pub priority_boost: f64,

    /// Auto-detect game processes (by executable path patterns).
    #[serde(default = "default_true")]
    pub detect_games: bool,

    /// Custom game executable names to detect.
    #[serde(default)]
    pub custom_games: Vec<String>,

    /// Directories to watch for game I/O (e.g., Steam library paths).
    #[serde(default)]
    pub game_directories: Vec<PathBuf>,
}

impl Default for GameModeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            priority_boost: default_priority_boost(),
            detect_games: true,
            custom_games: Vec::new(),
            game_directories: Vec::new(),
        }
    }
}

/// Prefetch configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrefetchConfig {
    /// Enable smart prefetching.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Threshold in KB to detect a sequential read stream.
    #[serde(default = "default_sequential_threshold_kb")]
    pub sequential_threshold_kb: u32,

    /// How many MB to prefetch ahead for sequential streams.
    #[serde(default = "default_prefetch_ahead_mb")]
    pub prefetch_ahead_mb: u32,

    /// Enable fragmentation-aware prefetching.
    /// Reads the file's fragment map and prefetches in optimal order.
    #[serde(default = "default_true")]
    pub fragment_aware: bool,

    /// Number of parallel prefetch worker threads.
    /// Each worker independently fetches predicted blocks from L2/HDD into L1.
    #[serde(default = "default_prefetch_workers")]
    pub worker_threads: usize,

    /// Minimum prefetch window in MB (adaptive scaling).
    #[serde(default = "default_prefetch_min_window_mb")]
    pub prefetch_min_window_mb: u32,

    /// Maximum prefetch window in MB (adaptive scaling).
    #[serde(default = "default_prefetch_max_window_mb")]
    pub prefetch_max_window_mb: u32,

    /// Maximum entries in the block metadata map before cleanup.
    #[serde(default = "default_max_block_metadata")]
    pub max_block_metadata: usize,
}

fn default_prefetch_workers() -> usize {
    4
}

fn default_prefetch_min_window_mb() -> u32 {
    1
}

fn default_prefetch_max_window_mb() -> u32 {
    64
}

fn default_max_block_metadata() -> usize {
    100_000
}

impl Default for PrefetchConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sequential_threshold_kb: default_sequential_threshold_kb(),
            prefetch_ahead_mb: default_prefetch_ahead_mb(),
            fragment_aware: true,
            worker_threads: default_prefetch_workers(),
            prefetch_min_window_mb: default_prefetch_min_window_mb(),
            prefetch_max_window_mb: default_prefetch_max_window_mb(),
            max_block_metadata: default_max_block_metadata(),
        }
    }
}

/// KDU (Kernel Driver Utility) configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KduConfig {
    /// Provider ID to use (see KDU documentation).
    #[serde(default = "default_provider_id")]
    pub provider_id: u32,

    /// Path to kdu.exe.
    #[serde(default = "default_kdu_path")]
    pub kdu_path: PathBuf,

    /// Automatically load the kernel driver on service start.
    #[serde(default = "default_true")]
    pub auto_load_driver: bool,

    /// Fallback provider IDs to try if the primary fails.
    #[serde(default = "default_fallback_providers")]
    pub fallback_providers: Vec<u32>,
}

impl Default for KduConfig {
    fn default() -> Self {
        Self {
            provider_id: default_provider_id(),
            kdu_path: default_kdu_path(),
            auto_load_driver: true,
            fallback_providers: default_fallback_providers(),
        }
    }
}

/// Per-volume caching configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeConfig {
    /// Drive letter (e.g., "D") or GUID path.
    pub volume: String,

    /// Whether caching is enabled for this volume.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Optional custom L1 size override in MB. None = use global default.
    pub l1_size_mb_override: Option<u32>,

    /// Optional custom block size override in KB. None = use global default.
    pub block_size_kb_override: Option<u32>,

    /// Optional custom L2 size override in GB. None = use global default.
    pub l2_size_gb_override: Option<u32>,
}

// === Default value functions ===

fn default_log_level() -> String {
    "info".to_string()
}

fn default_log_path() -> PathBuf {
    PathBuf::from(r"C:\ProgramData\NovaCache\nova_cache.log")
}

fn default_l1_size_mb() -> u32 {
    2048 // 2 GB
}

fn default_block_size_kb() -> u32 {
    64 // 64 KB
}

fn default_algorithm() -> CacheAlgorithm {
    CacheAlgorithm::Arc
}

fn default_l2_path() -> PathBuf {
    PathBuf::from("l2_cache.dat")
}

fn default_l2_size_gb() -> u32 {
    64 // 64 GB
}

fn default_flush_interval_ms() -> u64 {
    500 // 500ms base interval
}

fn default_flush_interval_max_ms() -> u64 {
    5000 // 5s max when idle
}

fn default_flush_dirty_threshold() -> u32 {
    128 // trigger aggressive flush at 128 dirty blocks
}

fn default_max_dirty_blocks() -> usize {
    4096 // hard cap: flush all if 4096 dirty blocks accumulate
}

fn default_priority_boost() -> f64 {
    2.0
}

fn default_sequential_threshold_kb() -> u32 {
    512 // 512 KB
}

fn default_prefetch_ahead_mb() -> u32 {
    16 // 16 MB
}

fn default_provider_id() -> u32 {
    11 // MSI EneTechIo64 — not in MSFT blocklist
}

fn default_kdu_path() -> PathBuf {
    PathBuf::from(r"kdu\kdu.exe")
}

fn default_fallback_providers() -> Vec<u32> {
    vec![13, 22, 25, 27, 28, 30, 34] // Providers not in MSFT blocklist
}

fn default_true() -> bool {
    true
}

// === Config loading/saving ===

impl NovaCacheConfig {
    /// Load configuration from a TOML file.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path)?;
        let config: Self = toml::from_str(&content)?;
        config.validate()?;
        Ok(config)
    }

    /// Save configuration to a TOML file.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        let content = toml::to_string_pretty(self)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, content)?;
        Ok(())
    }

    /// Load from default config path, or create default if not exists.
    pub fn load_or_default(path: &Path) -> Result<Self, ConfigError> {
        if path.exists() {
            Self::load(path)
        } else {
            let config = Self::default();
            config.save(path)?;
            Ok(config)
        }
    }

    /// Validate configuration values.
    pub fn validate(&self) -> Result<(), ConfigError> {
        // Block size must be 64 KB (matches driver's hardcoded CACHE_BLOCK_SIZE)
        let bs = self.cache.block_size_kb;
        if bs != 64 {
            return Err(ConfigError::ValidationError(format!(
                "block_size_kb must be 64 (matches driver CACHE_BLOCK_SIZE), got {}",
                bs
            )));
        }

        // L1 size must be reasonable
        if self.cache.l1_size_mb < 64 {
            return Err(ConfigError::ValidationError(
                "l1_size_mb must be at least 64 MB".to_string(),
            ));
        }

        // Adaptive flush: max must be >= base
        if self.cache.flush_interval_max_ms < self.cache.flush_interval_ms {
            return Err(ConfigError::ValidationError(format!(
                "flush_interval_max_ms ({}) must be >= flush_interval_ms ({})",
                self.cache.flush_interval_max_ms, self.cache.flush_interval_ms
            )));
        }

        // Priority boost must be positive
        if self.game_mode.priority_boost <= 0.0 {
            return Err(ConfigError::ValidationError(
                "priority_boost must be positive".to_string(),
            ));
        }

        Ok(())
    }

    /// Calculate the number of blocks that fit in L1 cache.
    pub fn l1_block_count(&self) -> usize {
        let l1_bytes = self.cache.l1_size_mb as usize * 1024 * 1024;
        let block_bytes = self.cache.block_size_kb as usize * 1024;
        l1_bytes / block_bytes
    }

    /// Calculate the number of blocks that fit in L2 cache.
    pub fn l2_block_count(&self) -> usize {
        if !self.cache.l2.enable {
            return 0;
        }
        let l2_bytes = self.cache.l2.size_gb as usize * 1024 * 1024 * 1024;
        let block_bytes = self.cache.block_size_kb as usize * 1024;
        l2_bytes / block_bytes
    }

    /// Block size in bytes.
    pub fn block_size_bytes(&self) -> usize {
        self.cache.block_size_kb as usize * 1024
    }
}

impl Default for NovaCacheConfig {
    fn default() -> Self {
        Self {
            general: GeneralConfig::default(),
            cache: CacheConfig::default(),
            game_mode: GameModeConfig::default(),
            prefetch: PrefetchConfig::default(),
            kdu: KduConfig::default(),
            volumes: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = NovaCacheConfig::default();
        assert_eq!(config.cache.l1_size_mb, 2048);
        assert_eq!(config.cache.block_size_kb, 64);
        assert_eq!(config.cache.algorithm, CacheAlgorithm::Arc);
        assert_eq!(config.cache.flush_interval_ms, 500);
        assert_eq!(config.cache.flush_interval_max_ms, 5000);
        assert_eq!(config.cache.flush_dirty_threshold, 128);
        assert!(config.game_mode.enabled);
        assert!(config.prefetch.enabled);
    }

    #[test]
    fn test_block_count_calculation() {
        let config = NovaCacheConfig::default();
        // 2048 MB / 64 KB = 32,768 blocks
        assert_eq!(config.l1_block_count(), 32768);
        // 64 GB / 64 KB = 1,048,576 blocks
        assert_eq!(config.l2_block_count(), 1048576);
    }

    #[test]
    fn test_validation_valid() {
        let config = NovaCacheConfig::default();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validation_bad_block_size() {
        let mut config = NovaCacheConfig::default();
        config.cache.block_size_kb = 128; // Must be 64
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validation_block_size_too_small() {
        let mut config = NovaCacheConfig::default();
        config.cache.block_size_kb = 32; // Must be 64
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validation_flush_interval_max_too_small() {
        let mut config = NovaCacheConfig::default();
        config.cache.flush_interval_max_ms = 100; // < flush_interval_ms (500)
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validation_l1_too_small() {
        let mut config = NovaCacheConfig::default();
        config.cache.l1_size_mb = 32; // < 64 MB minimum
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_serialization_roundtrip() {
        let config = NovaCacheConfig::default();
        let toml_str = toml::to_string_pretty(&config).unwrap();
        let parsed: NovaCacheConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.cache.l1_size_mb, config.cache.l1_size_mb);
        assert_eq!(parsed.cache.block_size_kb, config.cache.block_size_kb);
        assert_eq!(parsed.cache.algorithm, config.cache.algorithm);
    }

    #[test]
    fn test_parse_minimal_toml() {
        let toml_str = r#"
[general]
log_level = "debug"

[cache]
l1_size_mb = 4096
"#;
        let config: NovaCacheConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.general.log_level, "debug");
        assert_eq!(config.cache.l1_size_mb, 4096);
        // Defaults should fill in
        assert_eq!(config.cache.block_size_kb, 64);
        assert!(config.game_mode.enabled);
    }
}
