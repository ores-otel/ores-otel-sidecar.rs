//! Explicit persisted lifecycle state-machine recovery adapters.

pub(crate) use crate::adapters::process_lifecycle_freeze;
pub(crate) use crate::adapters::process_lifecycle_lease;

pub mod process_lifecycle_recovery;
