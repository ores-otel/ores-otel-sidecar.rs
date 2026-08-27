//! Shared k8s sidecar runtime inherited by product `*-sidecar.rs` crates.
//!
//! Product binaries depend on this crate via Cargo (`git` + `rev`) and declare
//! the same intent in `.zpkg.toml` as `ores-otel/ores-otel-sidecar`.

#![forbid(unsafe_code)]

pub mod config;
pub mod health;
pub mod identity;
pub mod probe;
pub mod runtime;

pub use config::SidecarConfig;
pub use health::Health;
pub use identity::SidecarIdentity;
pub use probe::{NoopProbe, ProductProbe};
