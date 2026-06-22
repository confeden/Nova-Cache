use crate::messages::ConnectionContext;
use std::os::windows::io::RawHandle;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::InstallableFileSystems::{
    FilterConnectCommunicationPort, FilterGetMessage, FilterReplyMessage, FilterSendMessage,
};

/// Wrapper around the Windows Minifilter Communication Port handle.
pub struct FilterPort {
    handle: HANDLE,
}

impl FilterPort {
    /// Connects to a minifilter communication port by name, passing connection context.
    ///
    /// The port name must start with a backslash (e.g. `\NovaCachePort`).
    pub fn connect(
        port_name: &str,
        context: &ConnectionContext,
    ) -> Result<Self, windows::core::Error> {
        let port_name_u16: Vec<u16> = port_name.encode_utf16().chain(std::iter::once(0)).collect();
        let pcwstr = PCWSTR::from_raw(port_name_u16.as_ptr());

        let handle = unsafe {
            FilterConnectCommunicationPort(
                pcwstr,
                0,
                Some(context as *const _ as *const std::ffi::c_void),
                std::mem::size_of::<ConnectionContext>() as u16,
                None,
            )?
        };

        Ok(Self { handle })
    }

    /// Sends a message to the minifilter and blocks waiting for a response.
    pub fn send_message(
        &self,
        request: &[u8],
        response: &mut [u8],
    ) -> Result<u32, windows::core::Error> {
        let mut bytes_returned = 0u32;

        unsafe {
            FilterSendMessage(
                self.handle,
                request.as_ptr() as *const _,
                request.len() as u32,
                Some(response.as_mut_ptr() as *mut _),
                response.len() as u32,
                &mut bytes_returned,
            )?;
        }

        Ok(bytes_returned)
    }

    /// Reads an unsolicited message from the driver.
    ///
    /// The buffer must be large enough to hold `FILTER_MESSAGE_HEADER` plus the payload.
    pub fn get_message(&self, buffer: &mut [u8]) -> Result<(), windows::core::Error> {
        unsafe {
            FilterGetMessage(
                self.handle,
                buffer.as_mut_ptr() as *mut _,
                buffer.len() as u32,
                None,
            )?;
        }

        Ok(())
    }

    /// Sends a reply to a message received from the driver.
    ///
    /// The reply buffer must contain the `FILTER_REPLY_HEADER` followed by the payload.
    pub fn reply_message(&self, reply: &[u8]) -> Result<(), windows::core::Error> {
        unsafe {
            FilterReplyMessage(self.handle, reply.as_ptr() as *const _, reply.len() as u32)?;
        }

        Ok(())
    }

    /// Exposes the raw Windows handle for this port.
    pub fn raw_handle(&self) -> RawHandle {
        self.handle.0 as _
    }
}

impl Drop for FilterPort {
    fn drop(&mut self) {
        if !self.handle.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.handle);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_port_connect_fails_when_driver_not_loaded() {
        let ctx = ConnectionContext::new(0, "", "I:\\l2_cache.dat");
        // Connecting to a non-existent port should return an error, not panic.
        let res = FilterPort::connect("\\NovaCacheNonExistentPort", &ctx);
        assert!(res.is_err());
    }
}
