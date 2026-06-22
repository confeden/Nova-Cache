//! # Nova Cache Driver Communication
//!
//! Communication layer between the user-mode cache service and the kernel
//! minifilter driver. Uses `FilterConnectCommunicationPort` for bidirectional
//! message passing and IOCTL wrappers for direct device control.
//!
//! ## Modules
//!
//! - [`port`]: Filter communication port management (connect, send, receive).
//! - [`messages`]: Shared message structures between user-mode and kernel.
//! - [`ioctl`]: IOCTL code definitions and `DeviceIoControl` wrappers.

pub mod ioctl;
pub mod messages;
pub mod port;
pub mod shared_mem;
