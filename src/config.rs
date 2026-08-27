#![forbid(unsafe_code)]

use std::net::SocketAddr;

use crate::bind::{allow_non_loopback_from_env, parse_bind};
use crate::error::SidecarError;
use crate::identity::SidecarIdentity;

#[derive(Clone, Debug)]
pub struct SidecarConfig {
    pub identity: SidecarIdentity,
    pub listen: SocketAddr,
}

impl SidecarConfig {
    pub fn from_env(identity: SidecarIdentity) -> Self {
        match Self::try_from_env(identity) {
            Ok(config) => config,
            Err(err) => {
                crate::log::write_stderr(identity.service, "fatal", err.to_string(), false);
                std::process::exit(1);
            }
        }
    }

    pub fn try_from_env(identity: SidecarIdentity) -> Result<Self, SidecarError> {
        let raw =
            std::env::var(identity.bind_env).unwrap_or_else(|_| identity.default_bind.to_string());
        Self::from_bind(identity, &raw, allow_non_loopback_from_env())
    }

    pub fn from_bind(
        identity: SidecarIdentity,
        raw: &str,
        allow_non_loopback: bool,
    ) -> Result<Self, SidecarError> {
        Ok(Self {
            listen: parse_bind(raw, allow_non_loopback)?,
            identity,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_bind_is_loopback() {
        let cfg = SidecarConfig::from_bind(
            SidecarIdentity::ORES_OTEL,
            SidecarIdentity::DEFAULT_BIND,
            false,
        )
        .unwrap();
        assert!(cfg.listen.ip().is_loopback());
    }
}
