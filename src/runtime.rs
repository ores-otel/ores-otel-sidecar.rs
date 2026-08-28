#![forbid(unsafe_code)]

use crate::config::SidecarConfig;
use crate::error::SidecarError;
use crate::http::{bind, probe_get, serve_listener};
use crate::log;
use crate::probe::ProductProbe;

pub fn run(config: &SidecarConfig) {
    run_or_probe(config);
}

/// Serve probes, or run a one-shot in-container check when argv is `probe`.
///
/// Kubernetes liveness should `exec` this same binary with `probe` so kubelet
/// never `httpGet`s the pod IP (invisible to a loopback listener) and never
/// takes the app out of a Service via sidecar `readinessProbe`.
pub fn run_or_probe(config: &SidecarConfig) {
    match std::env::args().nth(1).as_deref() {
        Some("probe") | Some("probe-healthz") => {
            std::process::exit(probe_exit(config.listen, "/healthz"));
        }
        Some("probe-readyz") => std::process::exit(probe_exit(config.listen, "/readyz")),
        Some(other) => {
            log::write_stderr(
                config.identity.service,
                "fatal",
                format!("unknown argument {other:?}; expected probe, probe-healthz, or probe-readyz"),
                false,
            );
            std::process::exit(2);
        }
        None => {
            if let Err(err) = run_with_probe(config, config.overrides()) {
                log::write_stderr(config.identity.service, "fatal", err.to_string(), false);
                std::process::exit(1);
            }
        }
    }
}

pub fn probe_exit(addr: std::net::SocketAddr, path: &str) -> i32 {
    match probe_get(addr, path) {
        Ok(200) => 0,
        _ => 1,
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
