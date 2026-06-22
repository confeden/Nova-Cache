//! # Nova Cache KDU (Kernel Driver Utility)
//!
//! Integration with KDU for loading the Nova Cache minifilter driver
//! when Driver Signature Enforcement (DSE) is active. Provides provider
//! selection, HVCI compatibility checks, and driver lifecycle management.
//!
//! ## Modules
//!
//! - [`loader`]: KDU process invocation and driver loading.
//! - [`dse`]: Driver Signature Enforcement status detection.
//! - [`hvci`]: Hypervisor-enforced Code Integrity checks.
//! - [`provider`]: KDU provider enumeration and selection.

pub mod dse;
pub mod hvci;
pub mod loader;
pub mod provider;
pub mod scm;
