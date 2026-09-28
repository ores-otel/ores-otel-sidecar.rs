//! Integration boundary between a held distributed lifecycle lease and the
//! pure lifecycle reconciler.
//!
//! Product/infra code should prefer this entrypoint once it has acquired a
//! `FencedLifecycleGrant`: the fencing token comes from the held grant rather
//! than from a caller-provided integer. Trusted host demand/admission, durable
//! storage, cooperative product control, and process effects remain injected
//! independently so none can impersonate another authority.

#![forbid(unsafe_code)]

use crate::adapters::process_lifecycle_lease::FencedLifecycleGrant;
use crate::process_lifecycle::LifecyclePolicy;
use crate::process_lifecycle_agent::{
    reconcile_once, ControllerScope, HostLifecycleControl, LifecycleEffects,
    LifecycleRecordStore, ProductLifecycleControl, ReconcileError, ReconcileOutcome,
};

pub fn reconcile_with_fenced_grant<S, H, P, E>(
    grant: &FencedLifecycleGrant,
    scope: &ControllerScope,
    policy: LifecyclePolicy,
    store: &mut S,
    host: &mut H,
    product: &mut P,
    effects: &mut E,
) -> Result<ReconcileOutcome, ReconcileError>
where
    S: LifecycleRecordStore,
    H: HostLifecycleControl,
    P: ProductLifecycleControl,
    E: LifecycleEffects,
{
    return reconcile_once(
        scope,
        grant.fencing_token(),
        policy,
        store,
        host,
        product,
        effects,
    );
}
