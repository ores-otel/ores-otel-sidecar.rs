#![forbid(unsafe_code)]

use std::net::{SocketAddr, ToSocketAddrs};

use crate::error::SidecarError;

pub fn parse_bind(raw: &str, allow_non_loopback: bool) -> Result<SocketAddr, SidecarError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(SidecarError::UnresolvedBind {
            value: raw.to_string(),
        });
    }
    let addr = if let Ok(parsed) = trimmed.parse::<SocketAddr>() {
        parsed
    } else {
        trimmed
            .to_socket_addrs()
            .map_err(|_| SidecarError::InvalidBind {
                value: trimmed.to_string(),
            })?
            .next()
            .ok_or_else(|| SidecarError::UnresolvedBind {
                value: trimmed.to_string(),
            })?
    };
    if addr.ip().is_unspecified() || (!addr.ip().is_loopback() && !allow_non_loopback) {
        return Err(SidecarError::NonLoopbackBind {
            value: trimmed.to_string(),
        });
    }
    Ok(addr)
}

pub fn allow_non_loopback_from_env() -> bool {
    matches!(
        std::env::var(crate::identity::ALLOW_NON_LOOPBACK)
            .ok()
            .as_deref()
            .map(str::trim),
        Some("1" | "true" | "TRUE" | "yes" | "YES")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_is_accepted() {
        let addr = parse_bind("127.0.0.1:9090", false).unwrap();
        assert!(addr.ip().is_loopback());
        assert_eq!(addr.port(), 9090);
    }

    #[test]
    fn unspecified_is_rejected_even_with_override() {
        assert!(matches!(
            parse_bind("0.0.0.0:9090", false),
            Err(SidecarError::NonLoopbackBind { .. })
        ));
        assert!(matches!(
            parse_bind("[::]:9090", true),
            Err(SidecarError::NonLoopbackBind { .. })
        ));
    }

    #[test]
    fn public_bind_requires_override() {
        assert!(parse_bind("1.1.1.1:9090", false).is_err());
        assert!(parse_bind("1.1.1.1:9090", true).is_ok());
    }

    #[test]
    fn empty_and_garbage_fail_closed() {
        assert!(parse_bind("   ", false).is_err());
        assert!(parse_bind("not-a-bind", false).is_err());
    }

    #[test]
    fn ipv6_loopback_is_accepted() {
        let addr = parse_bind("[::1]:9090", false).unwrap();
        assert!(addr.ip().is_loopback());
        assert_eq!(addr.port(), 9090);
    }

    #[test]
    fn whitespace_around_loopback_is_accepted() {
        let addr = parse_bind("  127.0.0.1:9090  ", false).unwrap();
        assert!(addr.ip().is_loopback());
    }

    #[test]
    fn allow_non_loopback_env_is_fail_closed_and_uses_generated_key() {
        let _guard = crate::ENV_LOCK.lock().unwrap();
        let key = crate::identity::ALLOW_NON_LOOPBACK;
        let previous = std::env::var(key).ok();
        std::env::remove_var(key);
        assert!(!allow_non_loopback_from_env());
        std::env::set_var(key, "false");
        assert!(!allow_non_loopback_from_env());
        std::env::set_var(key, "1");
        assert!(allow_non_loopback_from_env());
        std::env::set_var(key, "true");
        assert!(allow_non_loopback_from_env());
        match previous {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }
}
