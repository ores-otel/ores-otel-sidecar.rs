//! Product-control adapter for the shared lifecycle reconciler.
//!
//! The Unix socket remains cooperative evidence only. This adapter maps the
//! bounded v1 product protocol into `ProductLifecycleControl`; it does not make
//! queue depth, process identity, placement, fencing, or post-wake admission
//! authoritative.

#![forbid(unsafe_code)]

use crate::process_lifecycle_agent::{ProductLifecycleControl, ProductQuiesceOutcome};
use crate::process_lifecycle_runtime::{LifecycleRuntimeError, ProductControlClient};

const QUIESCE_TIMEOUT_MS: u64 = 30_000;

impl ProductLifecycleControl for ProductControlClient {
    fn quiesce(&mut self) -> Result<ProductQuiesceOutcome, String> {
        return map_quiesce_result(ProductControlClient::quiesce(
            self,
            QUIESCE_TIMEOUT_MS,
        ));
    }

    fn cancel_quiesce(&mut self) -> Result<(), String> {
        return ProductControlClient::resume(self).map_err(|error| error.to_string());
    }
}

fn map_quiesce_result(
    result: Result<(), LifecycleRuntimeError>,
) -> Result<ProductQuiesceOutcome, String> {
    match result {
        Ok(()) => {
            return Ok(ProductQuiesceOutcome::Drained);
        }
        Err(LifecycleRuntimeError::ProductDemandReturned) => {
            return Ok(ProductQuiesceOutcome::DemandReturned);
        }
        Err(error) => {
            return Err(error.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn product_demand_is_a_non_error_quiesce_outcome() {
        assert_eq!(
            map_quiesce_result(Err(LifecycleRuntimeError::ProductDemandReturned)),
            Ok(ProductQuiesceOutcome::DemandReturned)
        );
    }

    #[test]
    fn drain_failure_remains_fail_closed() {
        assert_eq!(
            map_quiesce_result(Err(LifecycleRuntimeError::ProductDrainTimeout)),
            Err(LifecycleRuntimeError::ProductDrainTimeout.to_string())
        );
    }
}
