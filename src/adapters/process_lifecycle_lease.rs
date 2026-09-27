//! Fenced lease adapter for process lifecycle mutations.
//!
//! The lifecycle policy is backend-agnostic. This outward adapter uses
//! `ores-locks-and-leases` so every suspend/resume transition can be serialized
//! across controllers and guarded by a monotonic fencing token.

#![forbid(unsafe_code)]

use std::fmt::{Display, Formatter};
use std::time::Duration;

use ores_locks_and_leases::{
    AcquireOptions, Lease, LeaseGrant, LockError, LockErrorKind, LockKey, ManagedLease,
};

const LOCK_PREFIX: &str = "process-lifecycle";
const MAX_SEGMENT_BYTES: usize = 96;

/// Stable logical identity used to derive the distributed lifecycle lock key.
///
/// Node identity is deliberately excluded. A shard/workload may move between
/// hosts during scale-down, failover, or rebalancing, and old/new controllers
/// must still contend on the same fenced authority. Put node identity in the
/// lease holder/controller metadata instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LifecycleLeaseScope {
    product: String,
    cluster: String,
    workload: String,
}

impl LifecycleLeaseScope {
    pub fn new(
        product: impl Into<String>,
        cluster: impl Into<String>,
        workload: impl Into<String>,
    ) -> Result<Self, LifecycleLeaseScopeError> {
        let product = product.into();
        let cluster = cluster.into();
        let workload = workload.into();

        validate_segment("product", &product)?;
        validate_segment("cluster", &cluster)?;
        validate_segment("workload", &workload)?;

        return Ok(Self {
            product,
            cluster,
            workload,
        });
    }

    pub fn lock_key(&self) -> Result<LockKey, LifecycleLeaseScopeError> {
        let value = format!(
            "{LOCK_PREFIX}/{}/{}/{}",
            self.product, self.cluster, self.workload
        );

        return LockKey::new(value).map_err(|_error| LifecycleLeaseScopeError::LockKeyTooLong);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleLeaseScopeError {
    InvalidSegment { field: &'static str },
    LockKeyTooLong,
}

impl Display for LifecycleLeaseScopeError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidSegment { field } => {
                return write!(formatter, "invalid process lifecycle lease {field} segment");
            }
            Self::LockKeyTooLong => {
                return formatter.write_str("process lifecycle lease key is too long");
            }
        }
    }
}

impl std::error::Error for LifecycleLeaseScopeError {}

fn validate_segment(
    field: &'static str,
    value: &str,
) -> Result<(), LifecycleLeaseScopeError> {
    let valid = !value.is_empty()
        && value.len() <= MAX_SEGMENT_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'));

    if !valid {
        return Err(LifecycleLeaseScopeError::InvalidSegment { field });
    }

    return Ok(());
}

/// Held fenced authority for one lifecycle mutation.
///
/// Persist `fencing_token()` with the lifecycle state/checkpoint metadata before
/// executing the requested process effect. A stale controller must never be
/// allowed to overwrite a record whose fencing token is newer.
pub struct FencedLifecycleGrant {
    grant: LeaseGrant,
}

impl FencedLifecycleGrant {
    #[must_use]
    pub const fn fencing_token(&self) -> u64 {
        return self.grant.fencing_token;
    }

    #[must_use]
    pub fn holder(&self) -> &str {
        return &self.grant.holder;
    }

    #[must_use]
    pub const fn lease_expires_ms(&self) -> Option<u64> {
        return self.grant.lease_expires_ms;
    }

    /// Renew long-running checkpoint/restore work without changing its fence.
    pub async fn renew<L>(
        self,
        lease: &L,
        ttl: Duration,
    ) -> Result<Self, LockError>
    where
        L: Lease + Sync,
    {
        let previous = self.grant;
        let renewed = lease.renew(&previous, ttl).await?;
        validate_renewed_grant(&previous, &renewed)?;
        return Ok(Self { grant: renewed });
    }

    /// Release exact ownership. `Ok(false)` means authority was already lost.
    pub async fn release<L>(self, lease: &L) -> Result<bool, LockError>
    where
        L: Lease + Sync,
    {
        return lease.release(&self.grant).await;
    }
}

fn validate_renewed_grant(
    previous: &LeaseGrant,
    renewed: &LeaseGrant,
) -> Result<(), LockError> {
    if renewed.key != previous.key
        || renewed.holder != previous.holder
        || renewed.fencing_token != previous.fencing_token
    {
        return Err(LockError::new(
            LockErrorKind::LostLease,
            &previous.key,
            "lifecycle lease renewal changed key, holder, or fencing token",
        ));
    }

    return Ok(());
}

#[derive(Debug)]
pub enum LifecycleLeaseAcquireError {
    InvalidScope(LifecycleLeaseScopeError),
    Lock(LockError),
}

impl Display for LifecycleLeaseAcquireError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidScope(error) => {
                return Display::fmt(error, formatter);
            }
            Self::Lock(error) => {
                return Display::fmt(error, formatter);
            }
        }
    }
}

impl std::error::Error for LifecycleLeaseAcquireError {}

/// Acquire fenced authority before reading/modifying persisted lifecycle state.
pub async fn acquire_lifecycle_lease<L>(
    lease: &L,
    scope: &LifecycleLeaseScope,
    options: &AcquireOptions,
    wait: bool,
) -> Result<FencedLifecycleGrant, LifecycleLeaseAcquireError>
where
    L: Lease + Sync,
{
    let key = scope
        .lock_key()
        .map_err(LifecycleLeaseAcquireError::InvalidScope)?;
    let grant = lease
        .acquire(&key, options, wait)
        .await
        .map_err(LifecycleLeaseAcquireError::Lock)?;

    return Ok(FencedLifecycleGrant { grant });
}

/// Initial production authority for Scintilla/BeamScale host agents.
///
/// The transport owns HTTP/auth details. Swapping to Fiducia or a future native
/// authority only changes the `Lease` implementation passed to
/// `acquire_lifecycle_lease`; lifecycle policy has no dependency on it.
pub const fn cloudflare_durable_object_lease<T>(transport: T) -> ManagedLease<T> {
    return ManagedLease::cloudflare(transport);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_derives_node_independent_lock_key() {
        let key = LifecycleLeaseScope::new("beamscale", "prod-us-east", "tenant-42")
            .and_then(|scope| scope.lock_key())
            .map(|key| key.as_str().to_owned());

        assert_eq!(
            key,
            Ok("process-lifecycle/beamscale/prod-us-east/tenant-42".to_owned())
        );
    }

    #[test]
    fn path_separator_is_rejected_inside_segments() {
        let scope = LifecycleLeaseScope::new("scintilla-run", "prod", "worker/escape");

        assert_eq!(
            scope,
            Err(LifecycleLeaseScopeError::InvalidSegment { field: "workload" })
        );
    }

    #[test]
    fn renewed_grant_must_preserve_exact_fenced_identity() {
        let key = LockKey::new("process-lifecycle/beamscale/prod/workload").unwrap();
        let previous = LeaseGrant {
            key: key.clone(),
            holder: "host-a-transition-1".to_owned(),
            fencing_token: 41,
            lease_expires_ms: Some(1000),
            ttl_ms: 500,
        };
        let mut changed = previous.clone();
        changed.fencing_token = 42;

        let error = validate_renewed_grant(&previous, &changed).unwrap_err();
        assert_eq!(error.kind, LockErrorKind::LostLease);

        let mut extended = previous.clone();
        extended.lease_expires_ms = Some(1500);
        assert_eq!(validate_renewed_grant(&previous, &extended), Ok(()));
    }

    #[test]
    fn long_segment_is_rejected_before_lock_key_construction() {
        let workload = "w".repeat(MAX_SEGMENT_BYTES + 1);
        let scope = LifecycleLeaseScope::new("beamscale", "prod", workload);

        assert_eq!(
            scope,
            Err(LifecycleLeaseScopeError::InvalidSegment { field: "workload" })
        );
    }
}
