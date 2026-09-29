//! Outward effect adapters for the shared sidecar runtime.

pub mod linux_process_lifecycle;
pub mod process_lifecycle_attested_freeze;
pub mod process_lifecycle_cloudflare_do;
pub mod process_lifecycle_controller;
pub mod process_lifecycle_file_store;
pub mod process_lifecycle_freeze;
pub mod process_lifecycle_identity;
pub mod process_lifecycle_lease;
pub mod process_lifecycle_product_control;
pub mod state_machine;
mod stream;
