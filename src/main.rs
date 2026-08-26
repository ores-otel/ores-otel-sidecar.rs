#![forbid(unsafe_code)]

use ores_otel_sidecar::{config::SidecarConfig, runtime};

fn main() {
    let cfg = SidecarConfig::from_env();
    runtime::run(&cfg);
}

