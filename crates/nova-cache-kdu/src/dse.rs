use std::ffi::c_void;
use windows::core::w;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ, REG_SZ,
    REG_VALUE_TYPE,
};

#[allow(non_upper_case_globals)]
pub const SystemCodeIntegrityInformation: i32 = 103;

#[allow(non_snake_case)]
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SYSTEM_CODEINTEGRITY_INFORMATION {
    pub Length: u32,
    pub CodeIntegrityOptions: u32,
}

pub const CODEINTEGRITY_OPTION_ENABLED: u32 = 0x01;
pub const CODEINTEGRITY_OPTION_TESTSIGN: u32 = 0x02;

type NtQuerySystemInformationFn = unsafe extern "system" fn(
    system_information_class: i32,
    system_information: *mut c_void,
    system_information_length: u32,
    return_length: *mut u32,
) -> i32;

/// Get system code integrity options from NtQuerySystemInformation.
pub fn get_code_integrity_options() -> Option<u32> {
    unsafe {
        let ntdll = GetModuleHandleW(w!("ntdll.dll")).ok()?;
        let nt_query_sys_info_ptr =
            GetProcAddress(ntdll, windows::core::s!("NtQuerySystemInformation"))?;
        let nt_query_sys_info: NtQuerySystemInformationFn =
            std::mem::transmute(nt_query_sys_info_ptr);

        let mut info = SYSTEM_CODEINTEGRITY_INFORMATION {
            Length: std::mem::size_of::<SYSTEM_CODEINTEGRITY_INFORMATION>() as u32,
            CodeIntegrityOptions: 0,
        };
        let mut ret_len = 0u32;

        let status = nt_query_sys_info(
            SystemCodeIntegrityInformation,
            &mut info as *mut _ as *mut c_void,
            info.Length,
            &mut ret_len,
        );

        if status == 0 {
            Some(info.CodeIntegrityOptions)
        } else {
            None
        }
    }
}

/// Check if Test Mode is enabled via the registry start options (SystemStartOptions).
pub fn is_test_mode_registry() -> bool {
    let mut hkey = HKEY::default();
    let subkey = w!("SYSTEM\\CurrentControlSet\\Control");

    unsafe {
        if RegOpenKeyExW(HKEY_LOCAL_MACHINE, subkey, 0, KEY_READ, &mut hkey).is_err() {
            return false;
        }

        let mut value_type = REG_VALUE_TYPE::default();
        let mut data_size = 0u32;
        let value_name = w!("SystemStartOptions");

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
            return false;
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
            let options = String::from_utf16_lossy(&buffer);
            options.contains("TESTSIGNING")
        } else {
            false
        }
    }
}

/// Determine whether Driver Signature Enforcement (DSE) is fully enabled and active.
///
/// Returns `true` if Driver Signature Enforcement is active and DSE bypass is required
/// to load unsigned drivers. Returns `false` if Test Mode or test signing is enabled.
pub fn is_dse_enabled() -> bool {
    if let Some(options) = get_code_integrity_options() {
        let enabled = (options & CODEINTEGRITY_OPTION_ENABLED) != 0;
        let testsign = (options & CODEINTEGRITY_OPTION_TESTSIGN) != 0;
        enabled && !testsign
    } else {
        // Fallback to registry if API call fails
        !is_test_mode_registry()
    }
}

/// Check if the system is currently in Test Mode (which allows unsigned/test-signed drivers).
pub fn is_test_mode() -> bool {
    if let Some(options) = get_code_integrity_options() {
        (options & CODEINTEGRITY_OPTION_TESTSIGN) != 0
    } else {
        is_test_mode_registry()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dse_checks_run() {
        let dse = is_dse_enabled();
        let test_mode = is_test_mode();
        println!("DSE Active: {}, Test Mode Active: {}", dse, test_mode);
    }
}
