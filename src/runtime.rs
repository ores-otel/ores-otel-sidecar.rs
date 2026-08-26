#![forbid(unsafe_code)]

use crate::config::SidecarConfig;
use crate::health;

pub fn run(config: &SidecarConfig) {
    println!(
        "{}",
        serde_json::to_string(&health::current()).expect("health is valid json")
    );
    let _ = config.listen.as_str();
}

