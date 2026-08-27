#![forbid(unsafe_code)]

use std::fmt::{Display, Formatter};
use std::io;

#[derive(Debug)]
pub enum SidecarError {
    InvalidBind { value: String },
    UnresolvedBind { value: String },
    NonLoopbackBind { value: String },
    Io(io::Error),
}

impl Display for SidecarError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidBind { value } => write!(f, "invalid bind address {value:?}"),
            Self::UnresolvedBind { value } => write!(f, "bind address {value:?} did not resolve"),
            Self::NonLoopbackBind { value } => write!(
                f,
                "refusing non-loopback bind {value:?}; set ORES_OTEL_SIDECAR_ALLOW_NON_LOOPBACK=1 to override"
            ),
            Self::Io(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for SidecarError {}

impl From<io::Error> for SidecarError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}
