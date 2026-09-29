//! Cloudflare Durable Object lease wire adapter for lifecycle fencing.
//!
//! This module owns the Cloudflare Worker request/response contract and bearer
//! authentication while deliberately leaving the concrete HTTPS engine injected.
//! That keeps the shared sidecar free of a second async runtime/TLS stack and lets
//! the executable choose a reviewed HTTP client without duplicating lease wire
//! semantics. The bearer itself is loaded from a systemd credential file; it is
//! never accepted as an argv or ordinary environment value.

#![forbid(unsafe_code)]

use std::future::Future;
use std::fs;
use std::path::{Component, Path, PathBuf};

use ores_locks_and_leases::{
    LeaseGrant, LockKey, ManagedAcquireResult, ManagedGrant, ManagedLeaseBackend,
    ManagedLeaseTransport, ManagedRenewResult,
};
use serde_json::{Value, json};

const MAX_SAFE_FENCING_TOKEN: u64 = 9_007_199_254_740_991;
const MAX_HTTP_BODY_BYTES: usize = 16 * 1024;
const MAX_BEARER_BYTES: usize = 4096;
const MAX_CREDENTIAL_NAME_BYTES: usize = 128;
const MAX_BASE_URL_BYTES: usize = 2048;
const CREDENTIALS_DIRECTORY_ENV: &str = "CREDENTIALS_DIRECTORY";

const ACQUIRE_PATH: &str = "/v1/leases/acquire";
const RENEW_PATH: &str = "/v1/leases/renew";
const RELEASE_PATH: &str = "/v1/leases/release";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaseHttpResponse {
    pub status: u16,
    pub body: String,
}

/// Minimal injected HTTPS seam.
///
/// Implementations must issue exactly one POST with the supplied JSON body and
/// Authorization header, must not follow redirects, and must bound response
/// buffering. Errors must not include request headers or bearer material.
pub trait LeaseHttpExecutor: Sync {
    fn post_json(
        &self,
        url: &str,
        authorization: &str,
        body: &str,
    ) -> impl Future<Output = Result<LeaseHttpResponse, String>> + Send;
}

/// Consumer-owned `ManagedLeaseTransport` for the bearer-protected Cloudflare
/// Durable Object Worker API shipped by `ores-locks-and-leases`.
pub struct CloudflareDoWorkerTransport<H> {
    base_url: String,
    bearer: String,
    http: H,
}

impl<H> CloudflareDoWorkerTransport<H> {
    pub fn from_systemd_credential(
        base_url: &str,
        credential_name: &str,
        http: H,
    ) -> Result<Self, String> {
        let validated_base_url = validate_base_url(base_url)?;
        let bearer = load_systemd_credential(credential_name)?;
        return Ok(Self {
            base_url: validated_base_url,
            bearer,
            http,
        });
    }

    #[cfg(test)]
    fn for_test(base_url: &str, bearer: &str, http: H) -> Result<Self, String> {
        let validated_base_url = validate_base_url(base_url)?;
        validate_bearer(bearer)?;
        return Ok(Self {
            base_url: validated_base_url,
            bearer: bearer.to_owned(),
            http,
        });
    }
}

impl<H> CloudflareDoWorkerTransport<H>
where
    H: LeaseHttpExecutor,
{
    async fn post(&self, path: &str, body: Value) -> Result<Value, String> {
        let encoded = serde_json::to_string(&body)
            .map_err(|_error| "cloudflare-do request encoding failed".to_owned())?;
        if encoded.len() > MAX_HTTP_BODY_BYTES {
            return Err("cloudflare-do request exceeded its bound".to_owned());
        }

        let url = format!("{}{}", self.base_url, path);
        let authorization = format!("Bearer {}", self.bearer);
        let response = self
            .http
            .post_json(&url, &authorization, &encoded)
            .await
            .map_err(|_error| "cloudflare-do transport failed".to_owned())?;
        if !(200..300).contains(&response.status) {
            return Err("cloudflare-do authority returned a non-success status".to_owned());
        }
        if response.body.len() > MAX_HTTP_BODY_BYTES {
            return Err("cloudflare-do response exceeded its bound".to_owned());
        }

        let parsed = serde_json::from_str::<Value>(&response.body)
            .map_err(|_error| "cloudflare-do authority returned invalid JSON".to_owned())?;
        if !parsed.is_object() {
            return Err("cloudflare-do authority response was not an object".to_owned());
        }
        return Ok(parsed);
    }

    async fn renew_replayed_grant(
        &self,
        key: &LockKey,
        holder: &str,
        fencing_token: u64,
        ttl_ms: u64,
    ) -> Result<Option<u64>, String> {
        let response = self
            .post(
                RENEW_PATH,
                json!({
                    "key": key.as_str(),
                    "holder": holder,
                    "fencing_token": fencing_token.to_string(),
                    "ttl_ms": ttl_ms,
                }),
            )
            .await?;
        if response.get("renewed").and_then(Value::as_bool) != Some(true) {
            return Err("cloudflare-do replay renewal was refused".to_owned());
        }
        return optional_safe_millis(&response, "lease_expires_ms");
    }
}

impl<H> ManagedLeaseTransport for CloudflareDoWorkerTransport<H>
where
    H: LeaseHttpExecutor,
{
    async fn acquire(
        &self,
        backend: ManagedLeaseBackend,
        key: &LockKey,
        holder: &str,
        request_id: &str,
        ttl_ms: u64,
    ) -> Result<ManagedAcquireResult, String> {
        require_cloudflare_backend(backend)?;
        let response = self
            .post(
                ACQUIRE_PATH,
                json!({
                    "key": key.as_str(),
                    "holder": holder,
                    "ttl_ms": ttl_ms,
                    "request_id": request_id,
                }),
            )
            .await?;

        if response.get("acquired").and_then(Value::as_bool) == Some(false) {
            return Ok(ManagedAcquireResult::Contended);
        }
        if response.get("acquired").and_then(Value::as_bool) != Some(true) {
            return Err("cloudflare-do acquire response omitted acquired".to_owned());
        }

        let fencing_token = required_positive_safe_u64(&response, "fencing_token")?;
        let lease_expires_ms = if response.get("replayed").and_then(Value::as_bool) == Some(true) {
            self.renew_replayed_grant(key, holder, fencing_token, ttl_ms)
                .await?
        } else {
            optional_safe_millis(&response, "lease_expires_ms")?
        };

        return Ok(ManagedAcquireResult::Acquired(ManagedGrant {
            fencing_token,
            lease_expires_ms,
        }));
    }

    async fn renew(
        &self,
        backend: ManagedLeaseBackend,
        grant: &LeaseGrant,
        ttl_ms: u64,
    ) -> Result<ManagedRenewResult, String> {
        require_cloudflare_backend(backend)?;
        let response = self
            .post(
                RENEW_PATH,
                json!({
                    "key": grant.key.as_str(),
                    "holder": grant.holder,
                    "fencing_token": grant.fencing_token.to_string(),
                    "ttl_ms": ttl_ms,
                }),
            )
            .await?;

        match response.get("renewed").and_then(Value::as_bool) {
            Some(true) => {
                return Ok(ManagedRenewResult::Renewed {
                    lease_expires_ms: optional_safe_millis(&response, "lease_expires_ms")?,
                });
            }
            Some(false) => {
                return Ok(ManagedRenewResult::Lost);
            }
            None => {
                return Err("cloudflare-do renew response omitted renewed".to_owned());
            }
        }
    }

    async fn release(
        &self,
        backend: ManagedLeaseBackend,
        grant: &LeaseGrant,
    ) -> Result<bool, String> {
        require_cloudflare_backend(backend)?;
        let response = self
            .post(
                RELEASE_PATH,
                json!({
                    "key": grant.key.as_str(),
                    "holder": grant.holder,
                    "fencing_token": grant.fencing_token.to_string(),
                }),
            )
            .await?;
        return response
            .get("released")
            .and_then(Value::as_bool)
            .ok_or_else(|| "cloudflare-do release response omitted released".to_owned());
    }
}

fn require_cloudflare_backend(backend: ManagedLeaseBackend) -> Result<(), String> {
    if backend != ManagedLeaseBackend::CloudflareDurableObject {
        return Err("cloudflare-do transport received the wrong backend".to_owned());
    }
    return Ok(());
}

fn validate_base_url(value: &str) -> Result<String, String> {
    let trimmed = value.trim_end_matches('/');
    let authority = trimmed
        .strip_prefix("https://")
        .ok_or_else(|| "cloudflare-do endpoint must use https".to_owned())?;
    if authority.is_empty()
        || trimmed.len() > MAX_BASE_URL_BYTES
        || trimmed.bytes().any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
        || trimmed.contains('?')
        || trimmed.contains('#')
        || authority.contains('@')
    {
        return Err("cloudflare-do endpoint is invalid".to_owned());
    }
    return Ok(trimmed.to_owned());
}

fn load_systemd_credential(credential_name: &str) -> Result<String, String> {
    validate_credential_name(credential_name)?;
    let directory = std::env::var_os(CREDENTIALS_DIRECTORY_ENV)
        .map(PathBuf::from)
        .ok_or_else(|| "systemd credentials directory is unavailable".to_owned())?;
    validate_credentials_directory(&directory)?;

    let path = directory.join(credential_name);
    let metadata = fs::symlink_metadata(&path)
        .map_err(|_error| "lifecycle lease credential is unavailable".to_owned())?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("lifecycle lease credential is not a regular file".to_owned());
    }
    if metadata.len() == 0 || metadata.len() > (MAX_BEARER_BYTES + 2) as u64 {
        return Err("lifecycle lease credential size is invalid".to_owned());
    }

    let raw = fs::read_to_string(&path)
        .map_err(|_error| "lifecycle lease credential could not be read".to_owned())?;
    let bearer = raw.trim_end_matches(['\r', '\n']);
    validate_bearer(bearer)?;
    return Ok(bearer.to_owned());
}

fn validate_credentials_directory(path: &Path) -> Result<(), String> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err("systemd credentials directory is invalid".to_owned());
    }
    let metadata = fs::symlink_metadata(path)
        .map_err(|_error| "systemd credentials directory is unavailable".to_owned())?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err("systemd credentials directory is invalid".to_owned());
    }
    return Ok(());
}

fn validate_credential_name(value: &str) -> Result<(), String> {
    let valid = !value.is_empty()
        && value.len() <= MAX_CREDENTIAL_NAME_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    if !valid || value == "." || value == ".." {
        return Err("lifecycle lease credential name is invalid".to_owned());
    }
    return Ok(());
}

fn validate_bearer(value: &str) -> Result<(), String> {
    let valid = !value.is_empty()
        && value.len() <= MAX_BEARER_BYTES
        && value.bytes().all(|byte| (0x21..=0x7e).contains(&byte));
    if !valid {
        return Err("lifecycle lease credential payload is invalid".to_owned());
    }
    return Ok(());
}

fn required_positive_safe_u64(object: &Value, field: &str) -> Result<u64, String> {
    let value = object
        .get(field)
        .ok_or_else(|| format!("cloudflare-do response omitted {field}"))?;
    let parsed = parse_safe_u64(value)
        .filter(|candidate| *candidate > 0)
        .ok_or_else(|| format!("cloudflare-do response contained invalid {field}"))?;
    return Ok(parsed);
}

fn optional_safe_millis(object: &Value, field: &str) -> Result<Option<u64>, String> {
    let Some(value) = object.get(field) else {
        return Ok(None);
    };
    let parsed = parse_safe_u64(value)
        .ok_or_else(|| format!("cloudflare-do response contained invalid {field}"))?;
    return Ok(Some(parsed));
}

fn parse_safe_u64(value: &Value) -> Option<u64> {
    let parsed = if let Some(value) = value.as_u64() {
        value
    } else if let Some(value) = value.as_str() {
        value.parse::<u64>().ok()?
    } else {
        return None;
    };
    if parsed > MAX_SAFE_FENCING_TOKEN {
        return None;
    }
    return Some(parsed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_constructor_enforces_https_and_bearer_bounds() {
        let valid = CloudflareDoWorkerTransport::for_test(
            "https://lease.example.test/",
            "opaque-bearer",
            (),
        );
        assert!(valid.is_ok());

        let insecure = CloudflareDoWorkerTransport::for_test(
            "http://lease.example.test",
            "opaque-bearer",
            (),
        );
        assert!(insecure.is_err());

        let multiline = CloudflareDoWorkerTransport::for_test(
            "https://lease.example.test",
            "secret\nsecond-line",
            (),
        );
        assert!(multiline.is_err());
    }

    #[test]
    fn parser_rejects_zero_and_non_js_safe_fencing_tokens() {
        assert!(required_positive_safe_u64(&json!({"fencing_token": "1"}), "fencing_token").is_ok());
        assert!(required_positive_safe_u64(&json!({"fencing_token": "0"}), "fencing_token").is_err());
        assert!(required_positive_safe_u64(&json!({"fencing_token": "9007199254740992"}), "fencing_token").is_err());
    }
}
