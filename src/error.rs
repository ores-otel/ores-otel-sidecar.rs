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
                "refusing non-loopback bind {value:?}; set {}=1 to override",
                crate::identity::ALLOW_NON_LOOPBACK
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

#[cfg(test)]
mod tests {
    use super::SidecarError;

    #[test]
    fn non_loopback_message_names_generated_override() {
        let err = SidecarError::NonLoopbackBind {
            value: "0.0.0.0:9090".into(),
        };
        let shown = err.to_string();
        assert!(shown.contains(crate::identity::ALLOW_NON_LOOPBACK), "{shown}");
        assert!(shown.contains("0.0.0.0:9090"), "{shown}");
    }
}
