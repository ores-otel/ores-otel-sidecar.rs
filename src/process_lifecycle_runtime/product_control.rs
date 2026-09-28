use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use super::{LifecycleAgentConfig, LifecycleRuntimeError};

const MAX_CONTROL_RESPONSE_BYTES: usize = 1024;
const CONTROL_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CooperativeAdmission {
    Accepting,
    Quiescing,
    Sealed,
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CooperativeProductStatus {
    pub admission: CooperativeAdmission,
    pub in_flight: u64,
    pub queue_depth: u64,
    pub idle_for_ms: u64,
}

/// The product status is cooperative evidence only. A hostile tenant runtime may
/// lie about these values, so callers MUST NOT use this type as trusted lifecycle
/// demand or admission authority.
#[derive(Clone, Debug)]
pub struct ProductControlClient {
    socket_path: PathBuf,
}

impl ProductControlClient {
    #[must_use]
    pub fn new(config: &LifecycleAgentConfig) -> Self {
        return Self {
            socket_path: config.product_socket.clone(),
        };
    }

    pub fn status(&self) -> Result<CooperativeProductStatus, LifecycleRuntimeError> {
        let response = self.request(b"v1 status\n")?;
        return parse_status_response(&response);
    }

    pub fn quiesce(&self, timeout_ms: u64) -> Result<(), LifecycleRuntimeError> {
        if timeout_ms > 300_000 {
            return Err(LifecycleRuntimeError::InvalidControlRequest);
        }
        let request = format!("v1 quiesce {timeout_ms}\n");
        let response = self.request(request.as_bytes())?;
        if response == "ok sealed" {
            return Ok(());
        }
        if response == "error drain_timeout" {
            return Err(LifecycleRuntimeError::ProductDrainTimeout);
        }
        if response == "error demand_returned" {
            return Err(LifecycleRuntimeError::ProductDemandReturned);
        }
        if response == "error lifecycle_busy" {
            return Err(LifecycleRuntimeError::ProductLifecycleBusy);
        }
        return Err(LifecycleRuntimeError::InvalidControlResponse);
    }

    pub fn resume(&self) -> Result<(), LifecycleRuntimeError> {
        let response = self.request(b"v1 resume\n")?;
        if response == "ok running" {
            return Ok(());
        }
        return Err(LifecycleRuntimeError::InvalidControlResponse);
    }

    fn request(&self, request: &[u8]) -> Result<String, LifecycleRuntimeError> {
        let stream = UnixStream::connect(&self.socket_path)
            .map_err(|_error| LifecycleRuntimeError::ProductControlUnavailable)?;
        configure_timeouts(&stream)?;
        return write_request(stream, request);
    }
}

fn configure_timeouts(stream: &UnixStream) -> Result<(), LifecycleRuntimeError> {
    stream
        .set_read_timeout(Some(CONTROL_TIMEOUT))
        .map_err(|_error| LifecycleRuntimeError::ProductControlUnavailable)?;
    stream
        .set_write_timeout(Some(CONTROL_TIMEOUT))
        .map_err(|_error| LifecycleRuntimeError::ProductControlUnavailable)?;
    return Ok(());
}

fn write_request(
    mut stream: UnixStream,
    request: &[u8],
) -> Result<String, LifecycleRuntimeError> {
    stream
        .write_all(request)
        .map_err(|_error| LifecycleRuntimeError::ProductControlUnavailable)?;
    stream
        .shutdown(Shutdown::Write)
        .map_err(|_error| LifecycleRuntimeError::ProductControlUnavailable)?;
    return read_response(stream, [0_u8; 256], Vec::new());
}

fn read_response(
    mut stream: UnixStream,
    mut chunk: [u8; 256],
    mut bytes: Vec<u8>,
) -> Result<String, LifecycleRuntimeError> {
    let count = stream
        .read(&mut chunk)
        .map_err(|_error| LifecycleRuntimeError::ProductControlUnavailable)?;
    if count == 0 {
        return finish_response(bytes);
    }
    bytes.extend_from_slice(&chunk[..count]);
    if bytes.len() > MAX_CONTROL_RESPONSE_BYTES {
        return Err(LifecycleRuntimeError::ControlResponseTooLarge);
    }
    return read_response(stream, [0_u8; 256], bytes);
}

fn finish_response(bytes: Vec<u8>) -> Result<String, LifecycleRuntimeError> {
    if bytes.is_empty() || bytes.last() != Some(&b'\n') {
        return Err(LifecycleRuntimeError::InvalidControlResponse);
    }
    let line = &bytes[..bytes.len() - 1];
    if line.is_empty() || line.contains(&b'\n') || line.contains(&b'\r') {
        return Err(LifecycleRuntimeError::InvalidControlResponse);
    }
    let text = std::str::from_utf8(line)
        .map_err(|_error| LifecycleRuntimeError::InvalidControlResponse)?;
    if text.trim() != text {
        return Err(LifecycleRuntimeError::InvalidControlResponse);
    }
    return Ok(text.to_owned());
}

fn parse_status_response(response: &str) -> Result<CooperativeProductStatus, LifecycleRuntimeError> {
    let parts = response.split_ascii_whitespace().collect::<Vec<_>>();
    if parts.len() != 6 || parts[0] != "ok" || parts[1] != "status" {
        return Err(LifecycleRuntimeError::InvalidControlResponse);
    }
    let admission = parse_admission(parts[2])?;
    let in_flight = parse_bounded_counter(parts[3])?;
    let queue_depth = parse_bounded_counter(parts[4])?;
    let idle_for_ms = parse_bounded_counter(parts[5])?;
    return Ok(CooperativeProductStatus {
        admission,
        in_flight,
        queue_depth,
        idle_for_ms,
    });
}

fn parse_admission(value: &str) -> Result<CooperativeAdmission, LifecycleRuntimeError> {
    if value == "accepting" {
        return Ok(CooperativeAdmission::Accepting);
    }
    if value == "quiescing" {
        return Ok(CooperativeAdmission::Quiescing);
    }
    if value == "sealed" {
        return Ok(CooperativeAdmission::Sealed);
    }
    if value == "unavailable" {
        return Ok(CooperativeAdmission::Unavailable);
    }
    return Err(LifecycleRuntimeError::InvalidControlResponse);
}

fn parse_bounded_counter(value: &str) -> Result<u64, LifecycleRuntimeError> {
    let parsed = value
        .parse::<u64>()
        .map_err(|_error| LifecycleRuntimeError::InvalidControlResponse)?;
    if parsed > i64::MAX as u64 {
        return Err(LifecycleRuntimeError::InvalidControlResponse);
    }
    return Ok(parsed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_status_parser_accepts_shared_v1_shape() {
        assert_eq!(
            parse_status_response("ok status sealed 0 0 125"),
            Ok(CooperativeProductStatus {
                admission: CooperativeAdmission::Sealed,
                in_flight: 0,
                queue_depth: 0,
                idle_for_ms: 125,
            })
        );
        assert_eq!(
            parse_status_response("ok status accepting -1 0 0"),
            Err(LifecycleRuntimeError::InvalidControlResponse)
        );
        assert_eq!(
            parse_status_response("ok status accepting 0 0 0 extra"),
            Err(LifecycleRuntimeError::InvalidControlResponse)
        );
    }

    #[test]
    fn control_responses_require_one_exact_newline_terminated_frame() {
        assert_eq!(
            finish_response(b"ok running\n".to_vec()),
            Ok("ok running".to_owned())
        );
        assert_eq!(
            finish_response(b"ok running".to_vec()),
            Err(LifecycleRuntimeError::InvalidControlResponse)
        );
        assert_eq!(
            finish_response(b"ok running\nignored\n".to_vec()),
            Err(LifecycleRuntimeError::InvalidControlResponse)
        );
        assert_eq!(
            finish_response(b" ok running\n".to_vec()),
            Err(LifecycleRuntimeError::InvalidControlResponse)
        );
    }
}
