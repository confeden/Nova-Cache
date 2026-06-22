use ferrisetw::parser::Parser;
use ferrisetw::provider::Provider;
use ferrisetw::schema_locator::SchemaLocator;
use ferrisetw::trace::UserTrace;
use ferrisetw::EventRecord;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{info, warn};
use windows::core::PCWSTR;
use windows::Win32::Storage::FileSystem::QueryDosDeviceW;

/// Event structure parsed from ETW Kernel-File provider.
#[derive(Debug, Clone)]
pub struct EtwEvent {
    /// Normalized full file path (e.g. `C:\Games\Steam\steam.exe`).
    pub file_name: String,
    /// Byte offset of the read/write request.
    pub offset: u64,
    /// Size of the read/write request in bytes.
    pub size: u64,
    /// Whether the event is a Write (true) or Read (false).
    pub is_write: bool,
    /// PID of the process performing the I/O.
    pub process_id: u32,
    /// Volume ID = drive letter index (A=0, B=1, ..., Z=25). Matches driver assignment.
    pub volume_id: u32,
}

/// Helper to build a mapping of NT device paths (e.g. `\Device\HarddiskVolume3`)
/// to Windows drive letters (e.g. `C:`).
pub fn get_device_mappings() -> HashMap<String, String> {
    let mut mappings = HashMap::new();
    let mut buffer = [0u16; 1024];

    for c in b'A'..=b'Z' {
        let drive = format!("{}:", c as char);
        let drive_u16: Vec<u16> = drive.encode_utf16().chain(std::iter::once(0)).collect();
        let pcwstr = PCWSTR::from_raw(drive_u16.as_ptr());

        unsafe {
            let len = QueryDosDeviceW(pcwstr, Some(&mut buffer));
            if len > 0 {
                let target_len = buffer
                    .iter()
                    .position(|&val| val == 0)
                    .unwrap_or(len as usize);
                let target = String::from_utf16_lossy(&buffer[..target_len]);
                // target looks like \Device\HarddiskVolume3
                mappings.insert(target, drive);
            }
        }
    }

    mappings
}

/// Normalizes an NT device path using the provided device mappings.
pub fn normalize_path(raw_path: &str, mappings: &HashMap<String, String>) -> String {
    for (device_path, drive_letter) in mappings {
        if raw_path.starts_with(device_path) {
            return raw_path.replace(device_path, drive_letter);
        }
    }
    raw_path.to_string()
}

pub struct EtwMonitor {
    trace: Option<UserTrace>,
}

impl EtwMonitor {
    /// Starts a real-time ETW trace session for disk I/O monitoring.
    pub fn start<F>(callback: F) -> anyhow::Result<Self>
    where
        F: Fn(EtwEvent) + Send + Sync + 'static,
    {
        let callback = Arc::new(callback);
        let mappings = Arc::new(get_device_mappings());
        let mappings_clone = Arc::clone(&mappings);
        let callback_clone = Arc::clone(&callback);

        // Microsoft-Windows-Kernel-File provider GUID: edd08927-9cc2-4efd-a0c7-2fad1fd0e716
        let file_provider = Provider::by_guid("edd08927-9cc2-4efd-a0c7-2fad1fd0e716")
            .add_callback(
                move |record: &EventRecord, schema_locator: &SchemaLocator| {
                    let event_id = record.event_id();
                    // 10 is Read, 11 is Write
                    if event_id == 10 || event_id == 11 {
                        if let Ok(schema) = schema_locator.event_schema(record) {
                            let parser = Parser::create(record, &schema);
                            let raw_path: String = parser.try_parse("FileName").unwrap_or_default();

                            // Ignore empty paths or internal memory-mapped files without paths
                            if !raw_path.is_empty() {
                                let file_name = normalize_path(&raw_path, &mappings_clone);
                                let offset: u64 = parser.try_parse("Offset").unwrap_or(0);
                                let size: u32 = parser.try_parse("ByteCount").unwrap_or(0);

                                // Extract drive letter from normalized path (e.g. "G:\..." -> volume_id 6)
                                let volume_id = file_name
                                    .bytes()
                                    .next()
                                    .filter(|&c| c.is_ascii_alphabetic())
                                    .map(|c| {
                                        let idx = (c.to_ascii_uppercase() - b'A') as u32;
                                        idx
                                    })
                                    .unwrap_or(0);

                                let event = EtwEvent {
                                    file_name,
                                    offset,
                                    size: size as u64,
                                    is_write: event_id == 11,
                                    process_id: record.process_id(),
                                    volume_id,
                                };

                                callback_clone(event);
                            }
                        }
                    }
                },
            )
            .build();

        info!("Starting real-time ETW I/O trace session...");

        let trace = UserTrace::new()
            .named("NovaCacheEtwSession".to_string())
            .enable(file_provider)
            .start_and_process()
            .map_err(|e| anyhow::anyhow!("Failed to start ETW trace: {:?}", e))?;

        Ok(Self { trace: Some(trace) })
    }

    pub fn stop(&mut self) {
        if let Some(trace) = self.trace.take() {
            info!("Stopping ETW trace session.");
            if let Err(e) = trace.stop() {
                warn!("Error stopping ETW trace session: {:?}", e);
            }
        }
    }
}

impl Drop for EtwMonitor {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_device_mappings_creation() {
        let mappings = get_device_mappings();
        assert!(
            !mappings.is_empty(),
            "Mappings should not be empty on Windows"
        );
        for (device, drive) in &mappings {
            println!("{} => {}", device, drive);
            assert!(device.starts_with("\\Device\\"));
            assert!(drive.ends_with(':'));
        }
    }

    #[test]
    fn test_path_normalization() {
        let mut mappings = HashMap::new();
        mappings.insert("\\Device\\HarddiskVolume3".to_string(), "C:".to_string());

        let raw = "\\Device\\HarddiskVolume3\\Windows\\System32\\notepad.exe";
        let normalized = normalize_path(raw, &mappings);
        assert_eq!(normalized, "C:\\Windows\\System32\\notepad.exe");
    }
}
