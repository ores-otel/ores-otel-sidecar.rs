//! Shared k8s sidecar runtime inherited by product `*-sidecar.rs` crates.
//!
//! The process listens on loopback HTTP for `/healthz`, `/readyz`, and `/metrics`.
//! The optional dual-stream receiver module is transport-only and never makes
//! product backend/export policy decisions. Diagnostics go to stderr as JSON.

#![forbid(unsafe_code)]

pub mod bind;
pub mod cli;
pub mod config;
pub mod error;
pub mod file_config;
pub mod health;
pub mod hooks;
#[path = "http_api.rs"]
pub mod http;
#[allow(dead_code)]
#[path = "http.rs"]
mod http_impl;
pub mod identity;
pub mod log;
pub mod probe;
pub mod receiver;
pub mod runtime;
pub mod runtime_updates;
pub mod runtime_values;
pub mod startup_env;

pub use cli::{CliError, CliResolution, SidecarCommand};
pub use config::SidecarConfig;
pub use error::SidecarError;
pub use file_config::{
    OresSidecarFile, ResolvedSidecarFile, RuntimeUpdateMode, RuntimeUpdatePolicy,
    RuntimeUpdateProvider, RuntimeValueDefinition, SidecarDefinition, CONFIG_PROTOCOL,
    DEFAULT_CONFIG_PATH as DEFAULT_SIDECAR_CONFIG_PATH, MAX_RECONCILE_SECONDS,
};
pub use health::Health;
pub use hooks::{DefaultOverrides, SidecarHooks, SidecarOverrides};
pub use identity::{SidecarEnv, SidecarIdentity};
pub use probe::{NoopProbe, ProductProbe};
pub use receiver::{
    receive_all, receive_one, ReceiverError, ReceiverFrame, ReceiverLimits,
    DEFAULT_MAX_DATA_CHUNK_BYTES, DEFAULT_MAX_METADATA_LINE_BYTES,
};
pub use runtime_updates::{
    RuntimeUpdateBatch, RuntimeUpdateController, RuntimeUpdateOperation, RuntimeUpdateOutcome,
    RuntimeUpdateState, RuntimeUpdateTarget, MAX_RUNTIME_KEYSPACE_SEGMENT_BYTES,
    MAX_SAFE_RUNTIME_REVISION,
};
pub use runtime_values::{is_sensitive_runtime_key, RuntimeValueUpdate, RuntimeValues};
pub use startup_env::{preflight_startup, preflight_startup_with_keys, StartupEnv};

#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
