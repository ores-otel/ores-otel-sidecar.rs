#![forbid(unsafe_code)]

use crate::config::SidecarConfig;
use crate::error::SidecarError;
use crate::http::{bind, serve_listener};
use crate::log;
use crate::probe::ProductProbe;

pub fn run(config: &SidecarConfig) {
    if let Err(err) = run_with_probe(config, config.overrides()) {
        log::write_stderr(config.identity.service, "fatal", err.to_string(), false);
        std::process::exit(1);
    }
}

pub fn run_with_probe(
    config: &SidecarConfig,
    probe: &(impl ProductProbe + ?Sized),
) -> Result<(), SidecarError> {
    let listener = bind(config.listen)?;
    let local = listener.local_addr().unwrap_or(config.listen);
    log::write_stderr(
        config.identity.service,
        "listen",
        format!("http://{local}/healthz"),
        true,
    );
    serve_listener(listener, config, probe)?;
    Ok(())
}
