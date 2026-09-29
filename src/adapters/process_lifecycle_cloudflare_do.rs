//! Bearer-authenticated Cloudflare Durable Object lease transport.
//!
//! This is the consumer-owned HTTP adapter required by `ores-locks-and-leases`.
//! The generic lock crate remains HTTP-client agnostic; this sidecar owns TLS,
//! systemd credential loading, redirect policy, response bounds, and the exact
//! Worker wire contract used by the process-lifecycle agent.

#![forbid(unsafe_code)]

use std::env;
use std::fmt::{Display, Formatter};
use std::fs;
use std::path::Path;
use std::time::Duration;

use ores_locks_and_leases::{
    LeaseGrant, LockKey, ManagedAcquireResult, ManagedGrant, ManagedLeaseBackend,
    ManagedLeaseTransport, ManagedRenewResult,
};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};

const ACQUIRE_PATH: &str = "/v1/leases/acquire";
const RENEW_PATH: &str = "/v1/leases/renew";
const RELEASE_PATH: &str = "/v1/leases/release";
const MAX_RESPONSE_BYTES: u64 = 64 * 1024;
const MAX_BEARER_BYTES: usize = 4096;
const MAX_JSON_SAFE_U64: u64 = 9_007_199_254_740_991;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloudflareDoTransportConfigError {
    InvalidBaseUrl,
    InvalidCredentialName,
    MissingCredentialsDirectory,
    InvalidCredentialsDirectory,
    CredentialUnavailable,
    InvalidCredential,
    HttpClientUnavailable,
}

impl Display for CloudflareDoTransportConfigError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        return formatter.write_str(match self {
            Self::InvalidBaseUrl => "Cloudflare lease endpoint must be an absolute HTTPS URL without credentials, query, or fragment",
            Self::InvalidCredentialName => "Cloudflare lease credential name is invalid",
            Self::MissingCredentialsDirectory => "systemd CREDENTIALS_DIRECTORY is unavailable",
            Self::InvalidCredentialsDirectory => "systemd credentials directory is invalid",
            Self::CredentialUnavailable => "Cloudflare lease credential is unavailable",
            Self::InvalidCredential => "Cloudflare lease credential is invalid",
            Self::HttpClientUnavailable => "Cloudflare lease HTTP client could not be constructed",
        });
    }
}

impl std::error::Error for CloudflareDoTransportConfigError {}

pub struct CloudflareDoLeaseTransport {
    client: Client,
    base_url: String,
    bearer: String,
}

impl CloudflareDoLeaseTransport {
    pub fn from_systemd_credential(
        base_url: &str,
        credential_name: &str,
    ) -> Result<Self, CloudflareDoTransportConfigError> {
        let directory = env::var_os("CREDENTIALS_DIRECTORY")
            .ok_or(CloudflareDoTransportConfigError::MissingCredentialsDirectory)?;
        return Self::from_credential_directory(base_url, credential_name, Path::new(&directory));
    }

    fn from_credential_directory(
        base_url: &str,
        credential_name: &str,
        directory: &Path,
    ) -> Result<Self, CloudflareDoTransportConfigError> {
        validate_credential_name(credential_name)?;
        validate_credentials_directory(directory)?;
        let credential_path = directory.join(credential_name);
        let metadata = fs::symlink_metadata(&credential_path)
            .map_err(|_error| CloudflareDoTransportConfigError::CredentialUnavailable)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(CloudflareDoTransportConfigError::CredentialUnavailable);
        }
        let bearer = fs::read_to_string(credential_path)
            .map_err(|_error| CloudflareDoTransportConfigError::CredentialUnavailable)?;
        let bearer = bearer.trim_end_matches(['\r', '\n']).to_owned();
        validate_bearer(&bearer)?;
        return Self::from_bearer(base_url, bearer);
    }

    fn from_bearer(
        base_url: &str,
        bearer: String,
    ) -> Result<Self, CloudflareDoTransportConfigError> {
        let base_url = normalize_base_url(base_url)?;
        let client = Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|_error| CloudflareDoTransportConfigError::HttpClientUnavailable)?;
        return Ok(Self {
            client,
            base_url,
            bearer,
        });
    }

    async fn post_json<Request, Response>(
        &self,
        path: &str,
        request: &Request,
    ) -> Result<Response, String>
    where
        Request: Serialize + ?Sized,
        Response: for<'de> Deserialize<'de>,
    {
        let mut response = self
            .client
            .post(format!("{}{}", self.base_url, path))
            .bearer_auth(&self.bearer)
            .json(request)
            .send()
            .await
            .map_err(|_error| "cloudflare-do lease transport failed".to_owned())?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!(
                "cloudflare-do lease authority returned HTTP {}",
                status.as_u16()
            ));
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES)
        {
            return Err("cloudflare-do lease response exceeded its bound".to_owned());
        }

        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_error| "cloudflare-do lease response body failed".to_owned())?
        {
            append_bounded_response_chunk(&mut body, &chunk)?;
        }
        return serde_json::from_slice(&body)
            .map_err(|_error| "cloudflare-do lease response was invalid JSON".to_owned());
    }

    async fn renew_wire(
        &self,
        key: &str,
        holder: &str,
        fencing_token: u64,
        ttl_ms: u64,
    ) -> Result<ManagedRenewResult, String> {
        let response = self
            .post_json::<_, RenewResponse>(
                RENEW_PATH,
                &RenewRequest {
                    key,
                    holder,
                    fencing_token: fencing_token.to_string(),
                    ttl_ms,
                },
            )
            .await?;
        return parse_renew_response(response);
    }
}

impl ManagedLeaseTransport for CloudflareDoLeaseTransport {
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
            .post_json::<_, AcquireResponse>(
                ACQUIRE_PATH,
                &AcquireRequest {
                    key: key.as_str(),
                    holder,
                    request_id,
                    ttl_ms,
                },
            )
            .await?;
        let (result, replayed) = parse_acquire_response(response)?;
        if !replayed {
            return Ok(result);
        }
        let ManagedAcquireResult::Acquired(grant) = result else {
            return Err("cloudflare-do replay response was not acquired".to_owned());
        };
        let renewed = self
            .renew_wire(key.as_str(), holder, grant.fencing_token, ttl_ms)
            .await?;
        return match renewed {
            ManagedRenewResult::Renewed { lease_expires_ms } => {
                Ok(ManagedAcquireResult::Acquired(ManagedGrant {
                    fencing_token: grant.fencing_token,
                    lease_expires_ms,
                }))
            }
            ManagedRenewResult::Lost => {
                Err("cloudflare-do replay renewal refused; fenced authority is lost".to_owned())
            }
        };
    }

    async fn renew(
        &self,
        backend: ManagedLeaseBackend,
        grant: &LeaseGrant,
        ttl_ms: u64,
    ) -> Result<ManagedRenewResult, String> {
        require_cloudflare_backend(backend)?;
        return self
            .renew_wire(
                grant.key.as_str(),
                &grant.holder,
                grant.fencing_token,
                ttl_ms,
            )
            .await;
    }

    async fn release(
        &self,
        backend: ManagedLeaseBackend,
        grant: &LeaseGrant,
    ) -> Result<bool, String> {
        require_cloudflare_backend(backend)?;
        let response = self
            .post_json::<_, ReleaseResponse>(
                RELEASE_PATH,
                &ReleaseRequest {
                    key: grant.key.as_str(),
                    holder: &grant.holder,
                    fencing_token: grant.fencing_token.to_string(),
                },
            )
            .await?;
        return Ok(response.released);
    }
}

fn require_cloudflare_backend(backend: ManagedLeaseBackend) -> Result<(), String> {
    if backend != ManagedLeaseBackend::CloudflareDurableObject {
        return Err("Cloudflare DO transport received a non-Cloudflare lease backend".to_owned());
    }
    return Ok(());
}

fn normalize_base_url(value: &str) -> Result<String, CloudflareDoTransportConfigError> {
    let parsed = Url::parse(value).map_err(|_error| CloudflareDoTransportConfigError::InvalidBaseUrl)?;
    if parsed.scheme() != "https"
        || parsed.host_str().is_none()
        || parsed.path() != "/"
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(CloudflareDoTransportConfigError::InvalidBaseUrl);
    }
    return Ok(value.trim_end_matches('/').to_owned());
}

fn append_bounded_response_chunk(body: &mut Vec<u8>, chunk: &[u8]) -> Result<(), String> {
    let next_len = body
        .len()
        .checked_add(chunk.len())
        .ok_or_else(|| "cloudflare-do lease response exceeded its bound".to_owned())?;
    if next_len as u64 > MAX_RESPONSE_BYTES {
        return Err("cloudflare-do lease response exceeded its bound".to_owned());
    }
    body.extend_from_slice(chunk);
    return Ok(());
}

fn validate_credential_name(value: &str) -> Result<(), CloudflareDoTransportConfigError> {
    let valid = !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    if !valid || value == "." || value == ".." {
        return Err(CloudflareDoTransportConfigError::InvalidCredentialName);
    }
    return Ok(());
}

fn validate_credentials_directory(
    directory: &Path,
) -> Result<(), CloudflareDoTransportConfigError> {
    if !directory.is_absolute() {
        return Err(CloudflareDoTransportConfigError::InvalidCredentialsDirectory);
    }
    let metadata = fs::symlink_metadata(directory)
        .map_err(|_error| CloudflareDoTransportConfigError::InvalidCredentialsDirectory)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(CloudflareDoTransportConfigError::InvalidCredentialsDirectory);
    }
    return Ok(());
}

fn validate_bearer(value: &str) -> Result<(), CloudflareDoTransportConfigError> {
    if value.is_empty()
        || value.len() > MAX_BEARER_BYTES
        || !value.bytes().all(|byte| byte.is_ascii_graphic())
    {
        return Err(CloudflareDoTransportConfigError::InvalidCredential);
    }
    return Ok(());
}

#[derive(Serialize)]
struct AcquireRequest<'a> {
    key: &'a str,
    holder: &'a str,
    request_id: &'a str,
    ttl_ms: u64,
}

#[derive(Deserialize)]
struct AcquireResponse {
    acquired: bool,
    fencing_token: Option<WireU64>,
    lease_expires_ms: Option<WireU64>,
    #[serde(default)]
    replayed: bool,
}

#[derive(Serialize)]
struct RenewRequest<'a> {
    key: &'a str,
    holder: &'a str,
    fencing_token: String,
    ttl_ms: u64,
}

#[derive(Deserialize)]
struct RenewResponse {
    renewed: bool,
    lease_expires_ms: Option<WireU64>,
}

#[derive(Serialize)]
struct ReleaseRequest<'a> {
    key: &'a str,
    holder: &'a str,
    fencing_token: String,
}

#[derive(Deserialize)]
struct ReleaseResponse {
    released: bool,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum WireU64 {
    Number(u64),
    String(String),
}

fn parse_acquire_response(
    response: AcquireResponse,
) -> Result<(ManagedAcquireResult, bool), String> {
    if !response.acquired {
        if response.replayed {
            return Err("cloudflare-do contention response cannot be replayed".to_owned());
        }
        return Ok((ManagedAcquireResult::Contended, false));
    }
    let fencing_token = parse_wire_u64(
        response.fencing_token,
        "fencing_token",
        true,
    )?;
    let lease_expires_ms = parse_optional_wire_u64(response.lease_expires_ms, "lease_expires_ms")?;
    return Ok((
        ManagedAcquireResult::Acquired(ManagedGrant {
            fencing_token,
            lease_expires_ms,
        }),
        response.replayed,
    ));
}

fn parse_renew_response(response: RenewResponse) -> Result<ManagedRenewResult, String> {
    if !response.renewed {
        return Ok(ManagedRenewResult::Lost);
    }
    let lease_expires_ms = parse_optional_wire_u64(response.lease_expires_ms, "lease_expires_ms")?;
    return Ok(ManagedRenewResult::Renewed { lease_expires_ms });
}

fn parse_optional_wire_u64(value: Option<WireU64>, field: &str) -> Result<Option<u64>, String> {
    return match value {
        Some(value) => parse_wire_u64(Some(value), field, false).map(Some),
        None => Ok(None),
    };
}

fn parse_wire_u64(value: Option<WireU64>, field: &str, positive: bool) -> Result<u64, String> {
    let value = value.ok_or_else(|| format!("cloudflare-do response omitted {field}"))?;
    let parsed = match value {
        WireU64::Number(value) => value,
        WireU64::String(value) => value
            .parse::<u64>()
            .map_err(|_error| format!("cloudflare-do response has invalid {field}"))?,
    };
    if parsed > MAX_JSON_SAFE_U64 || (positive && parsed == 0) {
        return Err(format!("cloudflare-do response has invalid {field}"));
    }
    return Ok(parsed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_requires_https_without_redirectable_authority_material() {
        assert_eq!(
            normalize_base_url("http://locks.example.test"),
            Err(CloudflareDoTransportConfigError::InvalidBaseUrl)
        );
        assert_eq!(
            normalize_base_url("https://user:secret@locks.example.test"),
            Err(CloudflareDoTransportConfigError::InvalidBaseUrl)
        );
        assert_eq!(
            normalize_base_url("https://locks.example.test?next=https://evil.test"),
            Err(CloudflareDoTransportConfigError::InvalidBaseUrl)
        );
        assert_eq!(
            normalize_base_url("https://locks.example.test/"),
            Ok("https://locks.example.test".to_owned())
        );
        assert_eq!(
            normalize_base_url("https://locks.example.test/nested"),
            Err(CloudflareDoTransportConfigError::InvalidBaseUrl)
        );
    }

    #[test]
    fn response_bound_is_enforced_while_streaming() {
        let mut body = vec![0_u8; (MAX_RESPONSE_BYTES - 1) as usize];
        assert!(append_bounded_response_chunk(&mut body, &[1]).is_ok());
        assert_eq!(body.len() as u64, MAX_RESPONSE_BYTES);
        assert!(append_bounded_response_chunk(&mut body, &[2]).is_err());
        assert_eq!(body.len() as u64, MAX_RESPONSE_BYTES);
    }

    #[test]
    fn credential_name_rejects_traversal_and_path_separators() {
        assert_eq!(
            validate_credential_name("../token"),
            Err(CloudflareDoTransportConfigError::InvalidCredentialName)
        );
        assert_eq!(
            validate_credential_name("token/name"),
            Err(CloudflareDoTransportConfigError::InvalidCredentialName)
        );
        assert_eq!(validate_credential_name("ores-locks-api-token"), Ok(()));
    }

    #[test]
    fn acquired_grant_requires_positive_json_safe_fence() {
        let valid = parse_acquire_response(AcquireResponse {
            acquired: true,
            fencing_token: Some(WireU64::String("41".to_owned())),
            lease_expires_ms: Some(WireU64::Number(2_000_000_000_000)),
            replayed: false,
        });
        assert!(matches!(
            valid,
            Ok((ManagedAcquireResult::Acquired(ManagedGrant { fencing_token: 41, .. }), false))
        ));

        let zero = parse_acquire_response(AcquireResponse {
            acquired: true,
            fencing_token: Some(WireU64::Number(0)),
            lease_expires_ms: None,
            replayed: false,
        });
        assert!(zero.is_err());

        let unsafe_json_integer = parse_acquire_response(AcquireResponse {
            acquired: true,
            fencing_token: Some(WireU64::Number(MAX_JSON_SAFE_U64 + 1)),
            lease_expires_ms: None,
            replayed: false,
        });
        assert!(unsafe_json_integer.is_err());
    }

    #[test]
    fn replayed_acquire_is_distinguished_from_fresh_acquire() {
        let replay = parse_acquire_response(AcquireResponse {
            acquired: true,
            fencing_token: Some(WireU64::String("9".to_owned())),
            lease_expires_ms: Some(WireU64::Number(20_000)),
            replayed: true,
        });
        assert!(matches!(
            replay,
            Ok((ManagedAcquireResult::Acquired(ManagedGrant { fencing_token: 9, .. }), true))
        ));
    }

    #[test]
    fn credential_file_must_be_regular_and_secret_is_never_part_of_config_errors() {
        let root = std::env::temp_dir().join(format!(
            "ores-cloudflare-do-credential-{}",
            std::process::id()
        ));
        let _cleanup_before = fs::remove_dir_all(&root);
        assert!(fs::create_dir_all(&root).is_ok());
        assert!(fs::write(root.join("api-token"), b"secret-token\n").is_ok());
        let transport = CloudflareDoLeaseTransport::from_credential_directory(
            "https://locks.example.test",
            "api-token",
            &root,
        );
        assert!(transport.is_ok());
        let _cleanup_after = fs::remove_dir_all(root);
    }
}
