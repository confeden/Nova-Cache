use windows::core::w;
use windows::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ, REG_DWORD,
    REG_VALUE_TYPE, REG_SZ,
};

/// Check if Hypervisor-protected Code Integrity (HVCI / Memory Integrity) is enabled.
///
/// HVCI prevents loading of unsigned drivers even if Driver Signature Enforcement (DSE)
/// is bypassed.
pub fn is_hvci_enabled() -> bool {
    let mut hkey = HKEY::default();
    let subkey = w!("SYSTEM\\CurrentControlSet\\Control\\DeviceGuard\\Scenarios\\HypervisorEnforcedCodeIntegrity");

    unsafe {
        if RegOpenKeyExW(HKEY_LOCAL_MACHINE, subkey, 0, KEY_READ, &mut hkey).is_err() {
            return false;
        }

        let mut value_type = REG_VALUE_TYPE::default();
        let mut data = 0u32;
        let mut data_size = std::mem::size_of::<u32>() as u32;
        let value_name = w!("Enabled");

        let res = RegQueryValueExW(
            hkey,
            value_name,
            None,
            Some(&mut value_type),
            Some(&mut data as *mut u32 as *mut u8),
            Some(&mut data_size),
        );

        let _ = RegCloseKey(hkey);

        if res.is_ok() && value_type == REG_DWORD {
            data == 1
        } else {
            false
        }
    }
}

/// Check if Virtualization-Based Security (VBS) is configured/enabled.
pub fn is_vbs_enabled() -> bool {
    let mut hkey = HKEY::default();
    let subkey = w!("SYSTEM\\CurrentControlSet\\Control\\DeviceGuard");

    unsafe {
        if RegOpenKeyExW(HKEY_LOCAL_MACHINE, subkey, 0, KEY_READ, &mut hkey).is_err() {
            return false;
        }

        let mut value_type = REG_VALUE_TYPE::default();
        let mut data = 0u32;
        let mut data_size = std::mem::size_of::<u32>() as u32;
        let value_name = w!("EnableVirtualizationBasedSecurity");

        let res = RegQueryValueExW(
            hkey,
            value_name,
            None,
            Some(&mut value_type),
            Some(&mut data as *mut u32 as *mut u8),
            Some(&mut data_size),
        );

        let _ = RegCloseKey(hkey);

        if res.is_ok() && value_type == REG_DWORD {
            data == 1
        } else {
            false
        }
    }
}

/// Query the Windows build number from registry.
pub fn get_windows_build_number() -> Option<u32> {
    let mut hkey = HKEY::default();
    let subkey = w!("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion");

    unsafe {
        if RegOpenKeyExW(HKEY_LOCAL_MACHINE, subkey, 0, KEY_READ, &mut hkey).is_err() {
            return None;
        }

        let mut value_type = REG_VALUE_TYPE::default();
        let mut data_size = 0u32;
        let value_name = w!("CurrentBuild");

        let res = RegQueryValueExW(
            hkey,
            value_name,
            None,
            Some(&mut value_type),
            None,
            Some(&mut data_size),
        );

        if res.is_err() || value_type != REG_SZ {
            let _ = RegCloseKey(hkey);
            return None;
        }

        let mut buffer = vec![0u16; (data_size / 2) as usize];
        let res = RegQueryValueExW(
            hkey,
            value_name,
            None,
            None,
            Some(buffer.as_mut_ptr() as *mut u8),
            Some(&mut data_size),
        );

        let _ = RegCloseKey(hkey);

        if res.is_ok() {
            let build_str = String::from_utf16_lossy(&buffer);
            let cleaned = build_str.trim_matches('\0').trim();
            cleaned.parse::<u32>().ok()
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hvci_and_vbs_detection_runs() {
        // We can't guarantee if HVCI is enabled or not on the test runner,
        // but we can verify that the functions execute without panicking.
        let hvci = is_hvci_enabled();
        let vbs = is_vbs_enabled();
        let build = get_windows_build_number();
        println!("HVCI Enabled: {}, VBS Enabled: {}, Build: {:?}", hvci, vbs, build);
    }
}
