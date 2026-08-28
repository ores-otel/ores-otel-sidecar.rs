//! Shared k8s sidecar runtime inherited by product `*-sidecar.rs` crates.
//!
//! The process listens on loopback HTTP for `/healthz`, `/readyz`, and `/metrics`.
//! Protocol never uses stdin or stdout; diagnostics go to stderr as JSON.

#![forbid(unsafe_code)]

pub mod bind;
pub mod config;
pub mod error;
pub mod health;
pub mod hooks;
pub mod http;
pub mod identity;
pub mod log;
pub mod probe;
pub mod runtime;

pub use config::SidecarConfig;
pub use error::SidecarError;
pub use health::Health;
pub use hooks::{DefaultOverrides, SidecarHooks, SidecarOverrides};
pub use identity::{SidecarEnv, SidecarIdentity};
pub use probe::{NoopProbe, ProductProbe};

#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
