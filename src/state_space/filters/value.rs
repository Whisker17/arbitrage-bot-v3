use std::{collections::HashMap, marker::PhantomData};

use super::{AMMFilter, FilterStage};
use crate::amms::{
    amm::{AutomatedMarketMaker, AMM},
    error::AMMError,
};
use alloy::{
    network::Network,
    primitives::{Address, U256},
    providers::Provider,
    sol,
    sol_types::SolValue,
};
use async_trait::async_trait;
use WmntValueInPools::{PoolInfo, PoolInfoReturn};

sol! {
    #[sol(rpc)]
    WmntValueInPoolsBatchRequest,
    "src/amms/abi/WmntValueInPoolsBatchRequest.json"
}

pub struct ValueFilter<const CHUNK_SIZE: usize, N, P>
where
    N: Network,
    P: Provider<N> + Clone,
{
    pub uniswap_v2_factory: Address,
    pub uniswap_v3_factory: Address,
    // If you later want Agni separated, add agni_factory here and adjust poolType mapping
    pub wmnt: Address,
    pub min_wmnt_threshold: U256,
    pub provider: P,
    phantom: PhantomData<N>,
}

impl<const CHUNK_SIZE: usize, N, P> ValueFilter<CHUNK_SIZE, N, P>
where
    N: Network,
    P: Provider<N> + Clone,
{
    pub fn new(
        uniswap_v2_factory: Address,
        uniswap_v3_factory: Address,
        wmnt: Address,
        min_wmnt_threshold: U256,
        provider: P,
    ) -> Self {
        Self {
            uniswap_v2_factory,
            uniswap_v3_factory,
            wmnt,
            min_wmnt_threshold,
            provider,
            phantom: PhantomData,
        }
    }

    pub async fn get_wmnt_value_in_pools(
        &self,
        pools: Vec<PoolInfo>,
    ) -> Result<HashMap<Address, PoolInfoReturn>, AMMError> {
        let deployer = WmntValueInPoolsBatchRequest::deploy_builder(
            self.provider.clone(),
            self.uniswap_v2_factory,
            self.uniswap_v3_factory,
            self.wmnt,
            pools,
        );

        let res = deployer.call_raw().await?;
        let return_data = <Vec<PoolInfoReturn> as SolValue>::abi_decode(&res)?;

        Ok(return_data
            .into_iter()
            .map(|pool_info| (pool_info.poolAddress, pool_info))
            .collect())
    }
}

#[async_trait]
impl<const CHUNK_SIZE: usize, N, P> AMMFilter for ValueFilter<CHUNK_SIZE, N, P>
where
    N: Network,
    P: Provider<N> + Clone,
{
    async fn filter(&self, amms: Vec<AMM>) -> Result<Vec<AMM>, AMMError> {
        let pool_infos = amms
            .iter()
            .cloned()
            .map(|amm| {
                let pool_address = amm.address();
                let pool_type = match amm {
                    AMM::UniswapV2Pool(_) => 1,
                    AMM::UniswapV3Pool(_) => 2,
                    AMM::AgniPool(_) => 2, // Treat Agni as UniV3-type
                };

                PoolInfo {
                    poolType: pool_type,
                    poolAddress: pool_address,
                }
            })
            .collect::<Vec<_>>();

        let mut pool_info_returns = HashMap::new();
        let futs = pool_infos
            .chunks(CHUNK_SIZE)
            .map(|chunk| async { self.get_wmnt_value_in_pools(chunk.to_vec()).await })
            .collect::<Vec<_>>();

        let results = futures::future::join_all(futs).await;
        for result in results {
            pool_info_returns.extend(result?);
        }

        let filtered_amms = amms
            .into_iter()
            .filter(|amm| {
                let pool_address = amm.address();
                pool_info_returns
                    .get(&pool_address)
                    .is_some_and(|pool_info_return| pool_info_return.wmntValue > self.min_wmnt_threshold)
            })
            .collect::<Vec<_>>();
        Ok(filtered_amms)
    }

    fn stage(&self) -> FilterStage {
        FilterStage::Sync
    }
}
