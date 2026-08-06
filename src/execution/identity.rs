//! Versioned execution identity validation and send leases (WHI-520).

use alloy::primitives::B256;
use thiserror::Error;

use super::fee_context::{BlockFeeContext, BlockFeeContextCache};
use super::gas_profile::RouteKey;
use super::gas_runtime::RuntimeGasProfile;
use crate::state_space::{
    BlockHeaderContext, IdentityBarrier, IdentityReadLease, SnapshotId, SnapshotPublisher,
};
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionIdentity {
    pub snapshot_id: SnapshotId,
    pub header: BlockHeaderContext,
    pub pool_universe_fingerprint: B256,
    pub route: RouteKey,
    pub fee_context: BlockFeeContext,
    pub gas_profile_identity: String,
}

#[derive(Debug)]
pub struct ExecutionIdentityLease {
    identity: ExecutionIdentity,
    _lease: IdentityReadLease,
}

impl ExecutionIdentityLease {
    /// Construct a lease from a validated identity + barrier permit.
    ///
    /// Callers must have already validated `identity` against a live ready tip
    /// and acquired `lease` from the same [`crate::state_space::IdentityBarrier`].
    pub fn new(identity: ExecutionIdentity, lease: IdentityReadLease) -> Self {
        Self {
            identity,
            _lease: lease,
        }
    }

    pub fn identity(&self) -> &ExecutionIdentity {
        &self.identity
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum IdentityError {
    #[error("snapshot publisher is not Ready")]
    PublisherNotReady,
    #[error("snapshot or header identity changed")]
    StaleHeader,
    #[error("pool universe topology changed")]
    StaleTopology,
    #[error("fee context changed: {0}")]
    StaleFeeContext(String),
    #[error("runtime gas profile or route is invalid: {0}")]
    InvalidRouteProfile(String),
    /// This identity source is not authorized to hand out send leases (e.g. a
    /// wallet-free / closed-gate stand-in source). Fail closed instead of panicking.
    #[error("send lease refused by this identity source: {0}")]
    SendLeaseUnavailable(String),
}

#[allow(async_fn_in_trait)]
pub trait ExecutionIdentitySource: Send + Sync {
    async fn validate(&self, identity: &ExecutionIdentity) -> Result<(), IdentityError>;
    async fn acquire_send_lease(
        &self,
        identity: &ExecutionIdentity,
    ) -> Result<ExecutionIdentityLease, IdentityError>;
}

#[derive(Clone)]
pub struct LiveExecutionIdentitySource {
    publisher: SnapshotPublisher,
    fee_contexts: Arc<BlockFeeContextCache>,
    gas_profile: RuntimeGasProfile,
    barrier: IdentityBarrier,
}

impl LiveExecutionIdentitySource {
    pub fn new(
        publisher: SnapshotPublisher,
        fee_contexts: Arc<BlockFeeContextCache>,
        gas_profile: RuntimeGasProfile,
    ) -> Self {
        let barrier = publisher.identity_barrier();
        Self {
            publisher,
            fee_contexts,
            gas_profile,
            barrier,
        }
    }

    /// Publish a fee context in the same exclusive domain as snapshot changes.
    pub async fn publish_fee_context(&self, context: BlockFeeContext) -> Result<(), IdentityError> {
        let _transition = self.barrier.begin_transition().await;
        self.fee_contexts
            .publish(context)
            .map_err(|error| IdentityError::StaleFeeContext(error.to_string()))
    }

    /// Invalidate measured route/profile state behind the lease barrier.
    pub async fn invalidate_route(&self, route: &RouteKey) -> Result<(), IdentityError> {
        let _transition = self.barrier.begin_transition().await;
        self.gas_profile
            .invalidate(route)
            .map_err(|error| IdentityError::InvalidRouteProfile(error.to_string()))
    }
}

impl ExecutionIdentitySource for LiveExecutionIdentitySource {
    async fn validate(&self, identity: &ExecutionIdentity) -> Result<(), IdentityError> {
        let ready = self
            .publisher
            .ready_snapshot()
            .await
            .ok_or(IdentityError::PublisherNotReady)?;
        if ready.id != identity.snapshot_id || ready.header != identity.header {
            return Err(IdentityError::StaleHeader);
        }
        if ready.coverage.pool_universe_fingerprint != Some(identity.pool_universe_fingerprint) {
            return Err(IdentityError::StaleTopology);
        }
        self.fee_contexts
            .matching(&identity.fee_context)
            .map_err(|error| IdentityError::StaleFeeContext(error.to_string()))?;
        let quote = self
            .gas_profile
            .quote(&identity.route)
            .map_err(|error| IdentityError::InvalidRouteProfile(error.to_string()))?;
        if quote.profile_identity != identity.gas_profile_identity {
            return Err(IdentityError::InvalidRouteProfile(
                "profile identity changed".into(),
            ));
        }
        Ok(())
    }

    async fn acquire_send_lease(
        &self,
        identity: &ExecutionIdentity,
    ) -> Result<ExecutionIdentityLease, IdentityError> {
        // Validate before and after entering the barrier. A queued transition is
        // fair-ordered ahead of this lease, so the second validation observes it.
        self.validate(identity).await?;
        let lease = self.barrier.acquire_lease().await;
        self.validate(identity).await?;
        Ok(ExecutionIdentityLease::new(identity.clone(), lease))
    }
}

/// Canonical Execute ordering. Slots are explicit so WHI-521/WHI-524 can attach
/// policy without reordering the identity checks.
pub const EXECUTE_SEND_ORDER: &[&str] = &[
    "build",
    "validate",
    "preflight",
    "validate",
    "begin_send",
    "acquire_lease",
    "final_validate",
    "sign",
    "durable_hook",
    "record_submission",
    "rpc_handoff",
    "release",
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::{BlockFeeContext, ProtocolKind};
    use alloy::primitives::B256;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct TestIdentitySource {
        valid: AtomicBool,
        barrier: IdentityBarrier,
    }

    impl ExecutionIdentitySource for TestIdentitySource {
        async fn validate(&self, _identity: &ExecutionIdentity) -> Result<(), IdentityError> {
            if self.valid.load(Ordering::SeqCst) {
                Ok(())
            } else {
                Err(IdentityError::StaleTopology)
            }
        }

        async fn acquire_send_lease(
            &self,
            identity: &ExecutionIdentity,
        ) -> Result<ExecutionIdentityLease, IdentityError> {
            self.validate(identity).await?;
            let lease = self.barrier.acquire_lease().await;
            self.validate(identity).await?;
            Ok(ExecutionIdentityLease::new(identity.clone(), lease))
        }
    }

    fn identity() -> ExecutionIdentity {
        ExecutionIdentity {
            snapshot_id: SnapshotId::new(5000, 1, B256::repeat_byte(1)),
            header: BlockHeaderContext::new(B256::ZERO, 1_700_000_000),
            pool_universe_fingerprint: B256::repeat_byte(2),
            route: RouteKey::new(vec![ProtocolKind::V2, ProtocolKind::V2]).unwrap(),
            fee_context: BlockFeeContext {
                block_number: 1,
                block_hash: B256::repeat_byte(1),
                base_fee_per_gas: 1,
                block_gas_limit: 1_000_000,
            },
            gas_profile_identity: "fixture".into(),
        }
    }

    #[tokio::test]
    async fn test_source_fails_closed_before_lease_and_binds_identity() {
        let source = TestIdentitySource {
            valid: AtomicBool::new(false),
            barrier: IdentityBarrier::default(),
        };
        assert_eq!(
            source.acquire_send_lease(&identity()).await.unwrap_err(),
            IdentityError::StaleTopology
        );
        source.valid.store(true, Ordering::SeqCst);
        let lease = source.acquire_send_lease(&identity()).await.unwrap();
        assert_eq!(lease.identity(), &identity());
    }
}
