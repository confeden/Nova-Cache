use std::os::windows::io::RawHandle;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::IO::DeviceIoControl;

// FILE_DEVICE_UNKNOWN = 0x00000022
pub const FILE_DEVICE_UNKNOWN: u32 = 0x00000022;

// Methods
pub const METHOD_BUFFERED: u32 = 0;
pub const METHOD_IN_DIRECT: u32 = 1;
pub const METHOD_OUT_DIRECT: u32 = 2;
pub const METHOD_NEITHER: u32 = 3;

// Access
pub const FILE_ANY_ACCESS: u32 = 0;
pub const FILE_READ_ACCESS: u32 = 1;
pub const FILE_WRITE_ACCESS: u32 = 2;

/// CTL_CODE macro equivalent in Rust.
pub const fn ctl_code(device_type: u32, function: u32, method: u32, access: u32) -> u32 {
    (device_type << 16) | (access << 14) | (function << 2) | method
}

// Custom IOCTL codes for Nova Cache
pub const IOCTL_NOVA_CACHE_START: u32 =
    ctl_code(FILE_DEVICE_UNKNOWN, 0x800, METHOD_BUFFERED, FILE_ANY_ACCESS);
pub const IOCTL_NOVA_CACHE_STOP: u32 =
    ctl_code(FILE_DEVICE_UNKNOWN, 0x801, METHOD_BUFFERED, FILE_ANY_ACCESS);
pub const IOCTL_NOVA_CACHE_GET_STATS: u32 =
    ctl_code(FILE_DEVICE_UNKNOWN, 0x802, METHOD_BUFFERED, FILE_ANY_ACCESS);
pub const IOCTL_NOVA_CACHE_FLUSH: u32 =
    ctl_code(FILE_DEVICE_UNKNOWN, 0x803, METHOD_BUFFERED, FILE_ANY_ACCESS);
pub const IOCTL_NOVA_CACHE_RESIZE: u32 =
    ctl_code(FILE_DEVICE_UNKNOWN, 0x804, METHOD_BUFFERED, FILE_ANY_ACCESS);

/// Send an IOCTL request to the driver device handle.
///
/// # Safety
/// This function performs Windows API FFI calls and casts raw pointers.
pub unsafe fn send_device_ioctl(
    device_handle: RawHandle,
    ioctl_code: u32,
    input_buffer: &[u8],
    output_buffer: &mut [u8],
) -> Result<u32, windows::core::Error> {
    let handle = HANDLE(device_handle as _);
    let mut bytes_returned = 0u32;

    let in_ptr = if input_buffer.is_empty() {
        std::ptr::null()
    } else {
        input_buffer.as_ptr() as *const _
    };

    let out_ptr = if output_buffer.is_empty() {
        std::ptr::null_mut()
    } else {
        output_buffer.as_mut_ptr() as *mut _
    };

    DeviceIoControl(
        handle,
        ioctl_code,
        Some(in_ptr),
        input_buffer.len() as u32,
        Some(out_ptr),
        output_buffer.len() as u32,
        Some(&mut bytes_returned),
        None,
    )?;

    Ok(bytes_returned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ctl_code_calculation() {
        // Test standard values
        let code = ctl_code(FILE_DEVICE_UNKNOWN, 0x800, METHOD_BUFFERED, FILE_ANY_ACCESS);
        // (0x22 << 16) | (0 << 14) | (0x800 << 2) | 0 = 0x222000
        assert_eq!(code, 0x00222000);

        let code_write = ctl_code(
            FILE_DEVICE_UNKNOWN,
            0x800,
            METHOD_BUFFERED,
            FILE_WRITE_ACCESS,
        );
        // (0x22 << 16) | (2 << 14) | (0x800 << 2) | 0 = 0x22A000
        assert_eq!(code_write, 0x0022A000);
    }
}
