use crate::amms::{
    amm::{AMM, AutomatedMarketMaker},
    error::AMMError,
    get_token_decimals,
    Token,
    GetMoeLBPairSlot0BatchRequest,
};
use alloy::{
    eips::BlockId,
    network::Network,
    primitives::{Address, B256, U256},
    providers::Provider,
    rpc::types::{Filter, FilterSet, Log},
    sol,
    sol_types::SolValue,
};
use futures::{stream::FuturesUnordered, StreamExt};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use thiserror::Error;
use tracing::info;
use rayon::iter::{IntoParallelRefIterator, ParallelDrainRange, ParallelIterator};

// ========= Errors =========

#[derive(Debug, Error)]
pub enum MoeError {
    #[error("Moe slot0 data unavailable")]
    MissingSlot0,
    #[error("Unsupported token address for Moe AMM")]
    UnsupportedToken,
    #[error("Arithmetic overflow while updating Moe reserves")]
    Arithmetic,
}

// ========= Events / Minimal Interfaces =========

sol! {
    #[derive(Debug, PartialEq, Eq)]
    #[sol(rpc)]
    pub contract IMoeLBPairEvents {
        event Swap(
            address indexed sender,
            address indexed to,
            uint24 id,
            bytes32 amountsIn,
            bytes32 amountsOut,
            uint24 volatilityAccumulator,
            bytes32 totalFees,
            bytes32 protocolFees
        );

        event DepositedToBins(
            address indexed sender,
            address indexed to,
            uint256[] ids,
            bytes32[] amounts
        );

        event WithdrawnFromBins(
            address indexed sender,
            address indexed to,
            uint256[] ids,
            bytes32[] amounts
        );
    }
}

sol! {
    #[derive(Debug, PartialEq, Eq)]
    #[sol(rpc)]
    pub contract IMoeLBPair {
        function getTokenX() external view returns (address);
        function getTokenY() external view returns (address);
        function getReserves() external view returns (uint128 reserveX, uint128 reserveY);
        function getBinStep() external view returns (uint16);
        function getActiveId() external view returns (uint24);
        function getBin(uint24 id) external view returns (uint128 reserveX, uint128 reserveY);
        function getStaticFeeParameters()
            external
            view
            returns (
                uint16 baseFactor,
                uint16 filterPeriod,
                uint16 decayPeriod,
                uint16 reductionFactor,
                uint24 variableFeeControl,
                uint16 protocolShare,
                uint24 maxVolatilityAccumulator
            );
        function getVariableFeeParameters()
            external
            view
            returns (
                uint24 volatilityAccumulator,
                uint24 volatilityReference,
                uint24 idReference,
                uint40 timeOfLastUpdate
            );
        function getNextNonEmptyBin(bool swapForY, uint24 id) external view returns (uint24 nextId);
        function getProtocolFees() external view returns (uint128 protocolFeeX, uint128 protocolFeeY);
        function getPriceFromId(uint24 id) external view returns (uint256 price);
        function getIdFromPrice(uint256 price) external view returns (uint24 id);
    }
}

sol! {
    #[derive(Debug)]
    #[sol(rpc)]
    pub contract IMoeFactory {
        event LBPairCreated(
            address indexed tokenX,
            address indexed tokenY,
            uint16 indexed binStep,
            address lbPair
        );
    }
}

// ========= Core Type =========

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MoeLbPair {
    pub address: Address,
    pub token_x: Token,
    pub token_y: Token,
    pub bin_step: u16,
    pub active_id: u32,
    pub reserve_x: u128,
    pub reserve_y: u128,
    pub protocol_share_bps: u16,
    pub max_volatility_acc: u32,
}

impl MoeLbPair {
    pub fn new(address: Address) -> Self {
        Self { address, ..Default::default() }
    }

    pub fn address(&self) -> Address { self.address }
}

// ========= Batch Sync Helpers =========

pub async fn sync_slot0_batch<N, P>(
    pairs: &mut [AMM],
    block: BlockId,
    provider: P,
) -> Result<(), AMMError>
where
    N: Network,
    P: Provider<N> + Clone,
{
    let step = 255;
    let mut futures = FuturesUnordered::new();
    for chunk in pairs.chunks_mut(step) {
        let addrs: Vec<Address> = chunk.iter_mut().map(|p| p.address()).collect();
        let prov = provider.clone();
        futures.push(async move {
            let ret = GetMoeLBPairSlot0BatchRequest::deploy_builder(prov, addrs)
                .call_raw()
                .block(block)
                .await?;
            Ok::<(&mut [AMM], Vec<(u32, u16, u128, u128, u16, u32)>), AMMError>(
                (chunk, <Vec<(u32, u16, u128, u128, u16, u32)>>::abi_decode(&ret)?),
            )
        });
    }

    while let Some(res) = futures.next().await {
        let (group, data) = res?;
        for (slot, amm) in data.into_iter().zip(group.iter_mut()) {
            if let AMM::MoeLbPair(p) = amm {
                p.active_id = slot.0;
                p.bin_step = slot.1;
                p.reserve_x = slot.2;
                p.reserve_y = slot.3;
                p.protocol_share_bps = slot.4;
                p.max_volatility_acc = slot.5;
            }
        }
    }
    Ok(())
}

pub async fn sync_token_decimals<N, P>(pairs: &mut [AMM], provider: P) -> Result<(), AMMError>
where
    N: Network,
    P: Provider<N> + Clone,
{
    let mut tokens = Vec::new();
    for amm in pairs.iter() {
        if let AMM::MoeLbPair(p) = amm {
            tokens.push(p.token_x.address);
            tokens.push(p.token_y.address);
        }
    }
    let decs = get_token_decimals(tokens, provider).await?;
    for amm in pairs.iter_mut() {
        if let AMM::MoeLbPair(p) = amm {
            if let Some(&dx) = decs.get(&p.token_x.address) { p.token_x.decimals = dx; }
            if let Some(&dy) = decs.get(&p.token_y.address) { p.token_y.decimals = dy; }
        }
    }
    Ok(())
}

// ========= AMM Implementation =========

impl AutomatedMarketMaker for MoeLbPair {
    fn address(&self) -> Address {
        self.address
    }

    fn sync_events(&self) -> Vec<B256> {
        vec![
            IMoeLBPairEvents::Swap::SIGNATURE_HASH,
            IMoeLBPairEvents::DepositedToBins::SIGNATURE_HASH,
            IMoeLBPairEvents::WithdrawnFromBins::SIGNATURE_HASH,
        ]
    }

    fn sync(&mut self, log: &Log) -> Result<(), AMMError> {
        let sig = log.topics()[0];
        if sig == IMoeLBPairEvents::Swap::SIGNATURE_HASH {
            let ev = IMoeLBPairEvents::Swap::decode_log(log.as_ref())?;
            self.active_id = ev.id as u32;
            Ok(())
        } else if sig == IMoeLBPairEvents::DepositedToBins::SIGNATURE_HASH {
            Ok(())
        } else if sig == IMoeLBPairEvents::WithdrawnFromBins::SIGNATURE_HASH {
            Ok(())
        } else {
            Err(AMMError::UnrecognizedEventSignature(sig))
        }
    }

    fn simulate_swap(
        &self,
        base_token: Address,
        _quote_token: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        if amount_in.is_zero() {
            return Ok(U256::ZERO);
        }
        let (reserve_in, reserve_out) = if base_token == self.token_x.address {
            (U256::from(self.reserve_x), U256::from(self.reserve_y))
        } else if base_token == self.token_y.address {
            (U256::from(self.reserve_y), U256::from(self.reserve_x))
        } else {
            return Ok(U256::ZERO);
        };
        if reserve_in.is_zero() || reserve_out.is_zero() {
            return Ok(U256::ZERO);
        }
        let numerator = amount_in * reserve_out;
        let denominator = reserve_in + amount_in;
        Ok(numerator / denominator)
    }

    fn simulate_swap_mut(
        &mut self,
        base_token: Address,
        _quote_token: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        let amount_out = self.simulate_swap(base_token, _quote_token, amount_in)?;
        if base_token == self.token_x.address {
            self.reserve_x = self.reserve_x.saturating_add(amount_in.to::<u128>());
            self.reserve_y = self.reserve_y.saturating_sub(amount_out.to::<u128>());
        } else {
            self.reserve_y = self.reserve_y.saturating_add(amount_in.to::<u128>());
            self.reserve_x = self.reserve_x.saturating_sub(amount_out.to::<u128>());
        }
        Ok(amount_out)
    }

    fn tokens(&self) -> Vec<Address> {
        vec![self.token_x.address, self.token_y.address]
    }

    fn calculate_price(&self, base_token: Address, _quote_token: Address) -> Result<f64, AMMError> {
        let (rx, ry, dx, dy) = (
            self.reserve_x as f64,
            self.reserve_y as f64,
            self.token_x.decimals as i8,
            self.token_y.decimals as i8,
        );
        let shift = dx - dy;
        let ratio = rx.max(1.0) / ry.max(1.0);
        let price_x_in_y = match shift.cmp(&0) {
            Ordering::Less => ratio / 10f64.powi((-shift) as i32),
            Ordering::Greater => ratio * 10f64.powi(shift as i32),
            Ordering::Equal => ratio,
        };
        if base_token == self.token_x.address { Ok(price_x_in_y) } else { Ok(1.0 / price_x_in_y) }
    }

    async fn init<N, P>(mut self, block_number: BlockId, provider: P) -> Result<Self, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let pair = IMoeLBPair::new(self.address, provider.clone());
        let x = pair.getTokenX().call().block(block_number).await?;
        let y = pair.getTokenY().call().block(block_number).await?;
        self.token_x = Token::from(x);
        self.token_y = Token::from(y);

        let mut v = vec![AMM::from(self.clone())];
        sync_slot0_batch::<N, _>(&mut v, block_number, provider.clone()).await?;
        sync_token_decimals::<N, _>(&mut v, provider.clone()).await?;
        if let Some(AMM::MoeLbPair(updated)) = v.into_iter().next() { Ok(updated) } else { Ok(self) }
    }
}

pub use MoeLbPair as MoeLbPairExport;

// ========= Factory =========

#[derive(Debug, Clone, Serialize, Deserialize, Hash, PartialEq, Eq)]
pub struct MoeFactory {
    pub address: Address,
    pub creation_block: u64,
}

impl MoeFactory {
    pub fn new(address: Address, creation_block: u64) -> Self {
        Self { address, creation_block }
    }

    pub async fn get_all_pools<N, P>(&self, to_block: BlockId, provider: P) -> Result<Vec<AMM>, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let filter = Filter::new()
            .event_signature(FilterSet::from(vec![self.pool_creation_event()]))
            .address(vec![self.address()])
            .from_block(self.creation_block)
            .to_block(to_block);
        let logs = provider.get_logs(&filter).await?;
        let mut pools = Vec::with_capacity(logs.len());
        for log in logs {
            pools.push(self.create_pool(log)?);
        }
        Ok(pools)
    }

    pub async fn sync_all_pools<N, P>(&self, mut pools: Vec<AMM>, block_number: BlockId, provider: P) -> Result<Vec<AMM>, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        sync_slot0_batch::<N, _>(&mut pools, block_number, provider.clone()).await?;
        sync_token_decimals::<N, _>(&mut pools, provider.clone()).await?;
        pools = pools
            .par_drain(..)
            .filter(|p| match p {
                AMM::MoeLbPair(x) => {
                    x.reserve_x > 0 && x.reserve_y > 0 && x.token_x.decimals > 0 && x.token_y.decimals > 0
                }
                _ => true,
            })
            .collect();
        Ok(pools)
    }
}

use crate::amms::factory::{AutomatedMarketMakerFactory, DiscoverySync};
use std::future::Future;

impl AutomatedMarketMakerFactory for MoeFactory {
    type PoolVariant = MoeLbPair;
    fn address(&self) -> Address { self.address }
    fn create_pool(&self, log: Log) -> Result<AMM, AMMError> {
        let ev: alloy::primitives::Log<IMoeFactory::LBPairCreated> = IMoeFactory::LBPairCreated::decode_log(&log.inner)?;
        Ok(AMM::MoeLbPair(MoeLbPair {
            address: ev.lbPair,
            token_x: ev.tokenX.into(),
            token_y: ev.tokenY.into(),
            bin_step: ev.binStep,
            ..Default::default()
        }))
    }
    fn creation_block(&self) -> u64 { self.creation_block }
    fn pool_creation_event(&self) -> B256 { IMoeFactory::LBPairCreated::SIGNATURE_HASH }
}

impl DiscoverySync for MoeFactory {
    fn discover<N, P>(&self, to_block: BlockId, provider: P) -> impl Future<Output = Result<Vec<AMM>, AMMError>>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        info!(target = "amms::moe::discover", address = ?self.address, "Discovering Moe pools");
        self.get_all_pools::<N, _>(to_block, provider)
    }
    fn sync<N, P>(&self, amms: Vec<AMM>, to_block: BlockId, provider: P) -> impl Future<Output = Result<Vec<AMM>, AMMError>>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        info!(target = "amms::moe::sync", address = ?self.address, "Syncing Moe pools");
        self.sync_all_pools::<N, _>(amms, to_block, provider)
    }
}
