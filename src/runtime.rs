#![forbid(unsafe_code)]

use crate::config::SidecarConfig;
use crate::health;
use crate::probe::{NoopProbe, ProductProbe};

pub fn run(config: &SidecarConfig) {
    run_with_probe(config, &NoopProbe);
}

pub fn run_with_probe(config: &SidecarConfig, probe: &impl ProductProbe) {
    let payload = health::current(config.identity, probe.extra_health());
    println!(
        "{}",
        serde_json::to_string(&payload).expect("health is valid json")
    );
    let _ = config.listen.as_str();
}
