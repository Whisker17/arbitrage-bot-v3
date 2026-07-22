//! Registry-backed pool provenance checks.
//!
//! Pool `factory()` self-attestation is deliberately not part of this API.

use alloy::primitives::Address;
use alloy::providers::Provider;
use thiserror::Error;

use super::contract::{
    IArbitrageExecutor, IMoeLBFactoryRegistry, IUniswapV2FactoryRegistry, IUniswapV3FactoryRegistry,
};
use crate::state_space::PoolProtocol;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolProvenance {
    pub protocol: PoolProtocol,
    pub factory: Address,
    pub pool: Address,
    pub token0: Address,
    pub token1: Address,
    /// V3/Agni fee or Moe bin step; zero for V2.
    pub fee_or_bin_step: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecutorPoolRegistration {
    pub enabled: bool,
    pub pool_type: u8,
    pub token0: Address,
    pub token1: Address,
    pub fee: u32,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ProvenanceError {
    #[error("factory registry reverse lookup returned {observed}, expected {expected}")]
    RegistryMismatch {
        expected: Address,
        observed: Address,
    },
    #[error("pool is disabled or absent from executor registry")]
    ExecutorDisabled,
    #[error("executor registry pool type mismatch")]
    ExecutorPoolType,
    #[error("executor registry token identity mismatch")]
    ExecutorTokens,
    #[error("executor registry fee identity mismatch")]
    ExecutorFee,
    #[error("provenance source failed: {0}")]
    Source(String),
}

#[allow(async_fn_in_trait)]
pub trait ProvenanceSource: Send + Sync {
    /// Must call factory `getPair`, `getPool`, or `getLBPairInformation`.
    async fn registry_pool(&self, expected: &PoolProvenance) -> Result<Address, ProvenanceError>;
    async fn executor_registration(
        &self,
        pool: Address,
    ) -> Result<ExecutorPoolRegistration, ProvenanceError>;
}

#[derive(Clone)]
pub struct OnChainProvenanceSource<P> {
    provider: P,
    executor: Address,
}

impl<P> OnChainProvenanceSource<P> {
    pub fn new(provider: P, executor: Address) -> Self {
        Self { provider, executor }
    }
}

impl<P> ProvenanceSource for OnChainProvenanceSource<P>
where
    P: Provider + Clone,
{
    async fn registry_pool(&self, expected: &PoolProvenance) -> Result<Address, ProvenanceError> {
        let result = match expected.protocol {
            PoolProtocol::UniswapV2 => {
                IUniswapV2FactoryRegistry::new(expected.factory, self.provider.clone())
                    .getPair(expected.token0, expected.token1)
                    .call()
                    .await
                    .map_err(|error| ProvenanceError::Source(error.to_string()))?
            }
            PoolProtocol::UniswapV3 | PoolProtocol::Agni => {
                IUniswapV3FactoryRegistry::new(expected.factory, self.provider.clone())
                    .getPool(
                        expected.token0,
                        expected.token1,
                        alloy::primitives::aliases::U24::from(expected.fee_or_bin_step),
                    )
                    .call()
                    .await
                    .map_err(|error| ProvenanceError::Source(error.to_string()))?
            }
            PoolProtocol::MoeLb => {
                let information =
                    IMoeLBFactoryRegistry::new(expected.factory, self.provider.clone())
                        .getLBPairInformation(
                            expected.token0,
                            expected.token1,
                            alloy::primitives::U256::from(expected.fee_or_bin_step),
                        )
                        .call()
                        .await
                        .map_err(|error| ProvenanceError::Source(error.to_string()))?;
                information.LBPair
            }
        };
        Ok(result)
    }

    async fn executor_registration(
        &self,
        pool: Address,
    ) -> Result<ExecutorPoolRegistration, ProvenanceError> {
        let registration = IArbitrageExecutor::new(self.executor, self.provider.clone())
            .registeredPools(pool)
            .call()
            .await
            .map_err(|error| ProvenanceError::Source(error.to_string()))?;
        Ok(ExecutorPoolRegistration {
            enabled: registration.enabled,
            pool_type: registration.poolType,
            token0: registration.token0,
            token1: registration.token1,
            fee: registration.fee.to::<u32>(),
        })
    }
}

pub async fn verify_pool_provenance(
    source: &impl ProvenanceSource,
    expected: &PoolProvenance,
) -> Result<(), ProvenanceError> {
    let observed = source.registry_pool(expected).await?;
    if observed != expected.pool {
        return Err(ProvenanceError::RegistryMismatch {
            expected: expected.pool,
            observed,
        });
    }
    let registration = source.executor_registration(expected.pool).await?;
    if !registration.enabled {
        return Err(ProvenanceError::ExecutorDisabled);
    }
    let expected_pool_type = match expected.protocol {
        PoolProtocol::UniswapV2 => 0,
        PoolProtocol::UniswapV3 | PoolProtocol::Agni => 1,
        PoolProtocol::MoeLb => 2,
    };
    if registration.pool_type != expected_pool_type {
        return Err(ProvenanceError::ExecutorPoolType);
    }
    if (registration.token0, registration.token1) != (expected.token0, expected.token1) {
        return Err(ProvenanceError::ExecutorTokens);
    }
    let expected_fee = match expected.protocol {
        PoolProtocol::UniswapV3 | PoolProtocol::Agni => expected.fee_or_bin_step,
        PoolProtocol::UniswapV2 | PoolProtocol::MoeLb => 0,
    };
    if registration.fee != expected_fee {
        return Err(ProvenanceError::ExecutorFee);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    struct FakeSource {
        registry_result: Address,
    }

    impl ProvenanceSource for FakeSource {
        async fn registry_pool(
            &self,
            _expected: &PoolProvenance,
        ) -> Result<Address, ProvenanceError> {
            Ok(self.registry_result)
        }

        async fn executor_registration(
            &self,
            _pool: Address,
        ) -> Result<ExecutorPoolRegistration, ProvenanceError> {
            Ok(ExecutorPoolRegistration {
                enabled: true,
                pool_type: 0,
                token0: address!("0000000000000000000000000000000000000001"),
                token1: address!("0000000000000000000000000000000000000002"),
                fee: 0,
            })
        }
    }

    #[tokio::test]
    async fn rejects_self_attesting_fake_pool_absent_from_factory_registry() {
        let fake_pool = address!("00000000000000000000000000000000000000aa");
        let expected = PoolProvenance {
            protocol: PoolProtocol::UniswapV2,
            factory: address!("00000000000000000000000000000000000000f0"),
            pool: fake_pool,
            token0: address!("0000000000000000000000000000000000000001"),
            token1: address!("0000000000000000000000000000000000000002"),
            fee_or_bin_step: 0,
        };
        let error = verify_pool_provenance(
            &FakeSource {
                // A fake pool could report expected.factory itself, but the
                // authoritative factory registry returns no matching pair.
                registry_result: Address::ZERO,
            },
            &expected,
        )
        .await
        .unwrap_err();
        assert!(matches!(error, ProvenanceError::RegistryMismatch { .. }));
    }
}
