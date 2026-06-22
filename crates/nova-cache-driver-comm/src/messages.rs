use bytemuck::{Pod, Zeroable};

/// Driver command codes sent from user-mode to kernel-mode.
#[repr(u32)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DriverCommand {
    StartCaching = 1,
    StopCaching = 2,
    GetStats = 3,
    FlushCache = 4,
    ResizeCache = 5,
}

/// A flat command request structure sent from user-mode to kernel-mode.
///
/// Uses `#[repr(C)]` and derives `Pod` and `Zeroable` for safe, zero-copy serialization.
#[repr(C)]
#[derive(Copy, Clone, Debug, Zeroable, Pod)]
pub struct DriverRequest {
    /// Command code (corresponds to `DriverCommand` values).
    pub command: u32,
    /// Cache size in MB (used by StartCaching and ResizeCache).
    pub cache_size_mb: u32,
    /// Block size in KB (used by StartCaching).
    pub block_size_kb: u32,
    /// UTF-16 representation of the volume GUID (e.g. `\\?\Volume{GUID}`).
    pub volume_guid: [u16; 128],
}

/// Cache hit/miss and throughput statistics.
#[repr(C)]
#[derive(Copy, Clone, Debug, Zeroable, Pod)]
pub struct CacheStats {
    pub total_reads: u64,
    pub cache_hits_l1: u64,
    pub cache_hits_l2: u64,
    pub cache_misses: u64,
    pub bytes_read_cached: u64,
    pub bytes_read_disk: u64,
    pub bytes_written: u64,
    pub cache_used_mb: u32,
    pub cache_total_mb: u32,
    pub hit_rate_percent: f32,
    pub avg_latency_us: f32,
}

/// Response returned from the kernel-mode driver.
#[repr(C)]
#[derive(Copy, Clone, Debug, Zeroable, Pod)]
pub struct DriverResponse {
    /// NTSTATUS code (0 for success).
    pub status: i32,
    /// Explicit padding to align `stats` to 8 bytes.
    pub _padding: u32,
    /// Cache statistics (populated on GetStats or success responses).
    pub stats: CacheStats,
}

impl DriverRequest {
    /// Helper to create a request with a volume GUID string.
    pub fn new_start_caching(
        volume_guid_str: &str,
        cache_size_mb: u32,
        block_size_kb: u32,
    ) -> Self {
        let mut volume_guid = [0u16; 128];
        let utf16: Vec<u16> = volume_guid_str.encode_utf16().collect();
        let len = std::cmp::min(utf16.len(), 128);
        volume_guid[..len].copy_from_slice(&utf16[..len]);

        Self {
            command: DriverCommand::StartCaching as u32,
            cache_size_mb,
            block_size_kb,
            volume_guid,
        }
    }

    pub fn new_stop_caching(volume_guid_str: &str) -> Self {
        let mut volume_guid = [0u16; 128];
        let utf16: Vec<u16> = volume_guid_str.encode_utf16().collect();
        let len = std::cmp::min(utf16.len(), 128);
        volume_guid[..len].copy_from_slice(&utf16[..len]);

        Self {
            command: DriverCommand::StopCaching as u32,
            cache_size_mb: 0,
            block_size_kb: 0,
            volume_guid,
        }
    }

    /// Helper to convert the UTF-16 volume GUID back into a String.
    pub fn volume_guid_to_string(&self) -> String {
        let len = self.volume_guid.iter().position(|&c| c == 0).unwrap_or(128);
        String::from_utf16_lossy(&self.volume_guid[..len])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_driver_request_serialization() {
        let req = DriverRequest::new_start_caching("\\\\?\\Volume{123-456}", 1024, 64);
        assert_eq!(req.command, DriverCommand::StartCaching as u32);
        assert_eq!(req.cache_size_mb, 1024);
        assert_eq!(req.block_size_kb, 64);
        assert_eq!(req.volume_guid_to_string(), "\\\\?\\Volume{123-456}");

        // Test zero-copy serialization using bytemuck
        let bytes = bytemuck::bytes_of(&req);
        assert_eq!(bytes.len(), std::mem::size_of::<DriverRequest>());

        let req2: &DriverRequest = bytemuck::from_bytes(bytes);
        assert_eq!(req2.command, req.command);
        assert_eq!(req2.volume_guid_to_string(), req.volume_guid_to_string());
    }

    #[test]
    fn test_driver_response_serialization() {
        let stats = CacheStats {
            total_reads: 100,
            cache_hits_l1: 80,
            cache_hits_l2: 10,
            cache_misses: 10,
            bytes_read_cached: 90 * 65536,
            bytes_read_disk: 10 * 65536,
            bytes_written: 50 * 65536,
            cache_used_mb: 256,
            cache_total_mb: 1024,
            hit_rate_percent: 90.0,
            avg_latency_us: 15.5,
        };
        let resp = DriverResponse {
            status: 0,
            _padding: 0,
            stats,
        };

        let bytes = bytemuck::bytes_of(&resp);
        assert_eq!(bytes.len(), std::mem::size_of::<DriverResponse>());

        let resp2: &DriverResponse = bytemuck::from_bytes(bytes);
        assert_eq!(resp2.status, 0);
        assert_eq!(resp2.stats.total_reads, 100);
        assert_eq!(resp2.stats.hit_rate_percent, 90.0);
    }
}

/// The context structure sent to the driver during port connection.
#[repr(C, packed)]
#[derive(Copy, Clone, Debug)]
pub struct ConnectionContext {
    pub section_handle: u64,
    pub event_name: [u16; 64],
    pub l2_path: [u16; 260],
}

unsafe impl Zeroable for ConnectionContext {}
unsafe impl Pod for ConnectionContext {}

impl ConnectionContext {
    pub fn new(section_handle: u64, event_name: &str, l2_path_str: &str) -> Self {
        let mut name_arr = [0u16; 64];
        let name_wide: Vec<u16> = event_name.encode_utf16().collect();
        let len = name_wide.len().min(63);
        name_arr[..len].copy_from_slice(&name_wide[..len]);

        let mut path_arr = [0u16; 260];
        let path_wide: Vec<u16> = l2_path_str.encode_utf16().collect();
        let path_len = path_wide.len().min(259);
        path_arr[..path_len].copy_from_slice(&path_wide[..path_len]);

        Self {
            section_handle,
            event_name: name_arr,
            l2_path: path_arr,
        }
    }
}
