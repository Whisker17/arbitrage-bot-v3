use super::{
    amm::{AutomatedMarketMaker, AMM},
    error::AMMError,
    factory::{AutomatedMarketMakerFactory, DiscoverySync},
    float::u256_to_float,
    get_token_decimals, Token,
};
use crate::amms::moe::math::{
    bin_helper,
    constants::{BASIS_POINT_MAX_U128, SCALE_OFFSET},
    packed_uint128_math, pair_parameter_helper,
};
use crate::amms::{GetMoeLBPairBinDataBatchRequest, GetMoeLBPairSlot0BatchRequest};
use alloy::{
    eips::BlockId,
    network::Network,
    primitives::{Address, B256, U256},
    providers::Provider,
    rpc::types::Log,
    sol,
    sol_types::{SolEvent, SolValue},
};
use futures::{stream::FuturesUnordered, StreamExt};
use rayon::iter::{ParallelDrainRange, ParallelIterator};
use serde::{Deserialize, Serialize};
use std::{cmp::Ordering, collections::HashMap};
use thiserror::Error;
use tracing::info;

pub mod math;
pub mod pool_list;
pub mod snapshot;
pub mod sync;

use snapshot::MoeSlot0BatchResponse;
pub use snapshot::{MoeBinRange, MoeSnapshot, MoeSnapshotContext, MoeSnapshotSyncConfig};
pub use sync::sync_moe_snapshots_batch;

pub use pool_list::{
    default_moe_pool_list_meta_path, default_moe_pool_list_path, discover_moe_pool_list,
    fetch_chunked_factory_logs, MoePoolList, MoePoolListEntry, MoePoolListError, MoePoolListMeta,
    CANONICAL_MOE_FACTORY, CANONICAL_MOE_FACTORY_CREATION_BLOCK,
    COMMITTED_MOE_POOL_LIST_SNAPSHOT_BLOCK, DEFAULT_MOE_POOL_LIST_META_REL,
    DEFAULT_MOE_POOL_LIST_REL,
};

const MAX_ITERATIONS: usize = 512;
const BPS_SCALE: u128 = BASIS_POINT_MAX_U128;

// ========= Errors =========

#[derive(Debug, Error)]
pub enum MoeError {
    #[error("Moe slot0 data unavailable")]
    MissingSlot0,
    #[error("Unsupported token address for Moe AMM")]
    UnsupportedToken,
    #[error("Arithmetic overflow while updating Moe reserves")]
    Arithmetic,
    #[error("Insufficient liquidity in bins")]
    InsufficientLiquidity,
    #[error("Invalid bin id")]
    InvalidBinId,
    #[error("Moe state is incomplete for an exact quote")]
    IncompleteState,
    #[error("Moe quote timestamp {quote_timestamp} does not match snapshot timestamp {snapshot_timestamp}")]
    SnapshotTimestampMismatch {
        quote_timestamp: u64,
        snapshot_timestamp: u64,
    },
    #[error("Moe batch response returned {actual} entries for {expected} requests")]
    MalformedBatchResponse { expected: usize, actual: usize },
    #[error("Moe snapshot violates a protocol invariant")]
    InvalidSnapshot,
    #[error(transparent)]
    PoolList(#[from] pool_list::MoePoolListError),
}

// ========= Events / Minimal Interfaces =========

sol! {
    #[derive(Debug, PartialEq, Eq)]
    #[sol(rpc)]
    contract IMoeLBPairEvents {
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

    #[derive(Debug, PartialEq, Eq)]
    #[sol(rpc)]
    contract IMoeLBPair {
        function getFactory() external view returns (address);
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

    #[derive(Debug)]
    #[sol(rpc)]
    contract IMoeFactory {
        // Matches ILBFactory.LBPairCreated on Merchant Moe (Mantle).
        event LBPairCreated(
            address indexed tokenX,
            address indexed tokenY,
            uint256 indexed binStep,
            address LBPair,
            uint256 pid
        );

        function getNumberOfLBPairs() external view returns (uint256);
        function getLBPairAtIndex(uint256 id) external view returns (address);
    }
}

// ========= Core Type =========

/// Bin data for a specific bin ID
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BinReserve {
    pub reserve_x: u128,
    pub reserve_y: u128,
}

/// Slot0-like data for Moe LB pairs (includes timestamp and other key parameters)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MoeSlot0 {
    pub active_id: u32,
    pub bin_step: u16,
    pub reserve_x: u128,
    pub reserve_y: u128,
    pub volatility_accumulator: u32,
    pub volatility_reference: u32,
    pub id_reference: u32,
    pub timestamp: U256,
    pub base_factor: u16,
    pub filter_period: u16,
    pub decay_period: u16,
    pub reduction_factor: u16,
    pub variable_fee_control: u32,
    pub protocol_share_bps: u16,
    pub max_volatility_acc: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MoeLbPair {
    pub address: Address,
    pub token_x: Token,
    pub token_y: Token,
    pub bin_step: u16,
    pub active_id: u32,
    pub reserve_x: u128,
    pub reserve_y: u128,
    pub base_factor: u16,
    pub filter_period: u16,
    pub decay_period: u16,
    pub reduction_factor: u16,
    pub variable_fee_control: u32,
    pub protocol_share_bps: u16,
    pub max_volatility_acc: u32,
    pub volatility_accumulator: u32,
    pub volatility_reference: u32,
    pub id_reference: u32,
    pub time_of_last_update: u64,
    /// Map of bin_id -> bin reserves
    /// This stores the detailed reserves for each bin to enable accurate swap simulation
    pub bins: HashMap<u32, BinReserve>,
    /// Complete, block-bound quote state. Legacy fields above are invalid until this is present.
    #[serde(default)]
    pub snapshot: Option<MoeSnapshot>,
}

impl MoeLbPair {
    pub fn new(address: Address) -> Self {
        Self {
            address,
            ..Default::default()
        }
    }

    pub fn address(&self) -> Address {
        self.address
    }

    /// Get current slot0-like data from the chain
    pub async fn get_slot0<N, P>(&self, provider: P) -> Result<MoeSlot0, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let pair = IMoeLBPair::new(self.address, provider.clone());

        let active_id = pair.getActiveId().call().await?;
        let bin_step = pair.getBinStep().call().await?;
        let reserves = pair.getReserves().call().await?;
        let var_params = pair.getVariableFeeParameters().call().await?;

        Ok(MoeSlot0 {
            active_id: active_id.to::<u32>(),
            bin_step,
            reserve_x: reserves.reserveX,
            reserve_y: reserves.reserveY,
            volatility_accumulator: var_params.volatilityAccumulator.to::<u32>(),
            volatility_reference: var_params.volatilityReference.to::<u32>(),
            id_reference: var_params.idReference.to::<u32>(),
            timestamp: U256::from(var_params.timeOfLastUpdate),
            base_factor: 0,
            filter_period: 0,
            decay_period: 0,
            reduction_factor: 0,
            variable_fee_control: 0,
            protocol_share_bps: 0,
            max_volatility_acc: 0,
        })
    }

    /// Initialize basic pool data similar to AgniPool::init_basic
    pub async fn init_basic<N, P>(
        mut self,
        block_number: BlockId,
        provider: P,
    ) -> Result<Self, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let pair = IMoeLBPair::new(self.address, provider.clone());

        self.token_x = Token::from(pair.getTokenX().call().block(block_number).await?);
        self.token_y = Token::from(pair.getTokenY().call().block(block_number).await?);
        self.bin_step = pair.getBinStep().call().block(block_number).await?;

        let mut pool_vec = vec![AMM::MoeLbPair(self)];
        sync_slot0_batch::<N, _>(&mut pool_vec, block_number, provider.clone()).await?;
        sync_token_decimals::<N, _>(&mut pool_vec, provider.clone()).await?;

        let AMM::MoeLbPair(pool) = pool_vec.remove(0) else {
            unreachable!()
        };

        Ok(pool)
    }

    /// Update bin reserves when DepositedToBins or WithdrawnFromBins event occurs
    pub fn update_bins(
        &mut self,
        ids: Vec<U256>,
        amounts: Vec<[u8; 32]>,
        is_deposit: bool,
    ) -> Result<(), AMMError> {
        let mut updated = self.clone();
        updated.snapshot = None;
        for (id, amount_bytes) in ids.iter().zip(amounts.iter()) {
            let bin_id = id.to::<u32>();

            // Decode packed amounts (bytes32 contains both X and Y amounts)
            // In Moe LB, bytes32 uses big-endian encoding:
            // - Bytes 0-15:  amountX (first uint128)
            // - Bytes 16-31: amountY (second uint128)
            let amount_x = u128::from_be_bytes(amount_bytes[0..16].try_into().unwrap_or([0u8; 16]));
            let amount_y =
                u128::from_be_bytes(amount_bytes[16..32].try_into().unwrap_or([0u8; 16]));

            let bin = updated.bins.entry(bin_id).or_insert(BinReserve::default());

            if is_deposit {
                bin.reserve_x = bin
                    .reserve_x
                    .checked_add(amount_x)
                    .ok_or(MoeError::Arithmetic)?;
                bin.reserve_y = bin
                    .reserve_y
                    .checked_add(amount_y)
                    .ok_or(MoeError::Arithmetic)?;
                updated.reserve_x = updated
                    .reserve_x
                    .checked_add(amount_x)
                    .ok_or(MoeError::Arithmetic)?;
                updated.reserve_y = updated
                    .reserve_y
                    .checked_add(amount_y)
                    .ok_or(MoeError::Arithmetic)?;
            } else {
                bin.reserve_x = bin
                    .reserve_x
                    .checked_sub(amount_x)
                    .ok_or(MoeError::Arithmetic)?;
                bin.reserve_y = bin
                    .reserve_y
                    .checked_sub(amount_y)
                    .ok_or(MoeError::Arithmetic)?;
                updated.reserve_x = updated
                    .reserve_x
                    .checked_sub(amount_x)
                    .ok_or(MoeError::Arithmetic)?;
                updated.reserve_y = updated
                    .reserve_y
                    .checked_sub(amount_y)
                    .ok_or(MoeError::Arithmetic)?;

                // Remove bin if both reserves are zero
                if bin.reserve_x == 0 && bin.reserve_y == 0 {
                    updated.bins.remove(&bin_id);
                }
            }
        }
        *self = updated;
        Ok(())
    }

    /// Get price from bin ID
    /// Price = (1 + binStep / 10000) ^ (id - 2^23)
    pub fn get_price_from_id(&self, id: u32) -> Result<f64, AMMError> {
        price_from_id_to_f64(id, self.bin_step)
    }

    /// Simulate swap across multiple bins (more accurate than simple x*y=k)
    pub fn simulate_swap_across_bins(
        &mut self,
        swap_for_y: bool, // true = X->Y, false = Y->X
        amount_in: U256,
        timestamp: u64,
    ) -> Result<U256, AMMError> {
        self.simulate_swap_precise(swap_for_y, amount_in, timestamp)
    }

    pub fn simulate_swap_precise(
        &mut self,
        swap_for_y: bool,
        amount_left: U256,
        timestamp: u64,
    ) -> Result<U256, AMMError> {
        let mut quote = self.snapshot_quote(timestamp)?;
        let amount_out = simulate_swap_precise_inner(&mut quote, swap_for_y, amount_left)?;
        quote.snapshot = None;
        *self = quote;
        Ok(amount_out)
    }

    pub fn snapshot_slot0(&self) -> MoeSlot0 {
        MoeSlot0 {
            active_id: self.active_id,
            bin_step: self.bin_step,
            reserve_x: self.reserve_x,
            reserve_y: self.reserve_y,
            volatility_accumulator: self.volatility_accumulator,
            volatility_reference: self.volatility_reference,
            id_reference: self.id_reference,
            timestamp: U256::from(self.time_of_last_update),
            base_factor: self.base_factor,
            filter_period: self.filter_period,
            decay_period: self.decay_period,
            reduction_factor: self.reduction_factor,
            variable_fee_control: self.variable_fee_control,
            protocol_share_bps: self.protocol_share_bps,
            max_volatility_acc: self.max_volatility_acc,
        }
    }

    pub fn install_snapshot(&mut self, snapshot: MoeSnapshot) -> Result<(), AMMError> {
        snapshot.validate()?;
        self.apply_snapshot(snapshot);
        Ok(())
    }

    fn snapshot_quote(&self, timestamp: u64) -> Result<Self, AMMError> {
        let snapshot = self.snapshot.as_ref().ok_or(MoeError::IncompleteState)?;
        if timestamp != snapshot.block_timestamp {
            return Err(MoeError::SnapshotTimestampMismatch {
                quote_timestamp: timestamp,
                snapshot_timestamp: snapshot.block_timestamp,
            }
            .into());
        }

        let mut quote = self.clone();
        quote.apply_snapshot(snapshot.clone());
        Ok(quote)
    }

    pub(crate) fn apply_snapshot(&mut self, snapshot: MoeSnapshot) {
        let slot0 = &snapshot.slot0;
        self.active_id = slot0.active_id;
        self.bin_step = slot0.bin_step;
        self.reserve_x = slot0.reserve_x;
        self.reserve_y = slot0.reserve_y;
        self.base_factor = slot0.base_factor;
        self.filter_period = slot0.filter_period;
        self.decay_period = slot0.decay_period;
        self.reduction_factor = slot0.reduction_factor;
        self.variable_fee_control = slot0.variable_fee_control;
        self.protocol_share_bps = slot0.protocol_share_bps;
        self.max_volatility_acc = slot0.max_volatility_acc;
        self.volatility_accumulator = slot0.volatility_accumulator;
        self.volatility_reference = slot0.volatility_reference;
        self.id_reference = slot0.id_reference;
        self.time_of_last_update = slot0.timestamp.to::<u64>();
        self.bins = snapshot.bins.clone();
        self.snapshot = Some(snapshot);
    }
}

fn price_from_id_to_f64(id: u32, bin_step: u16) -> Result<f64, AMMError> {
    let price =
        math::price_helper::get_price_from_id(id, bin_step).map_err(|_| MoeError::Arithmetic)?;
    let mut price = u256_to_float(price)?;
    price /= u256_to_float(U256::from(1u8) << SCALE_OFFSET)?;
    Ok(price.to_f64())
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

            let decoded = MoeSlot0BatchResponse::decode_batch(&ret)?;
            Ok::<(&mut [AMM], Vec<MoeSlot0BatchResponse>), AMMError>((chunk, decoded))
        });
    }

    while let Some(res) = futures.next().await {
        let (group, data) = res?;
        for (slot, amm) in data.into_iter().zip(group.iter_mut()) {
            if let AMM::MoeLbPair(p) = amm {
                p.token_x = Token::from(slot.token_x);
                p.token_y = Token::from(slot.token_y);
                p.active_id = slot.slot0.active_id;
                p.bin_step = slot.slot0.bin_step;
                p.reserve_x = slot.slot0.reserve_x;
                p.reserve_y = slot.slot0.reserve_y;
                p.base_factor = slot.slot0.base_factor;
                p.filter_period = slot.slot0.filter_period;
                p.decay_period = slot.slot0.decay_period;
                p.reduction_factor = slot.slot0.reduction_factor;
                p.variable_fee_control = slot.slot0.variable_fee_control;
                p.protocol_share_bps = slot.slot0.protocol_share_bps;
                p.max_volatility_acc = slot.slot0.max_volatility_acc;
                p.volatility_accumulator = slot.slot0.volatility_accumulator;
                p.volatility_reference = slot.slot0.volatility_reference;
                p.id_reference = slot.slot0.id_reference;
                p.time_of_last_update = slot.slot0.timestamp.to::<u64>();
                p.snapshot = None;
            }
        }
    }
    Ok(())
}

/// Sync bin data for active bins around the current active_id
/// This populates the bins HashMap with reserve data for accurate swap simulation
pub async fn sync_active_bins_batch<N, P>(
    pairs: &mut [AMM],
    block: BlockId,
    provider: P,
    bins_radius: u32,
) -> Result<(), AMMError>
where
    N: Network,
    P: Provider<N> + Clone,
{
    // Process in smaller chunks to avoid "max code size exceeded" error
    // Each chunk can handle ~5-10 pools depending on bins_radius
    let chunk_size = 5;
    let mut futures = FuturesUnordered::new();

    for chunk in pairs.chunks_mut(chunk_size) {
        // Build batch requests for this chunk
        let batch_requests: Vec<GetMoeLBPairBinDataBatchRequest::BinDataRequest> = chunk
            .iter()
            .filter_map(|amm| {
                if let AMM::MoeLbPair(pair) = amm {
                    let active_id = pair.active_id;
                    let start_id = active_id.saturating_sub(bins_radius);
                    let end_id = active_id.saturating_add(bins_radius);

                    // Create array of bin IDs to query
                    // Convert u32 to Uint<24, 1> (uint24 in Solidity)
                    let ids: Vec<u32> = (start_id..=end_id).collect();

                    Some(GetMoeLBPairBinDataBatchRequest::BinDataRequest {
                        pair: pair.address,
                        ids: ids.into_iter().map(U256::from).map(|v| v.to()).collect(),
                    })
                } else {
                    None
                }
            })
            .collect();

        if batch_requests.is_empty() {
            continue;
        }

        let prov = provider.clone();
        futures.push(async move {
            // Execute batch request
            let ret = GetMoeLBPairBinDataBatchRequest::deploy_builder(prov, batch_requests)
                .call_raw()
                .block(block)
                .await?;

            // Decode response: Vec<Vec<(u128, u128)>>
            let all_bin_data: Vec<Vec<(u128, u128)>> = Vec::abi_decode(&ret)?;

            Ok::<(&mut [AMM], Vec<Vec<(u128, u128)>>), AMMError>((chunk, all_bin_data))
        });
    }

    // Process results
    while let Some(res) = futures.next().await {
        let (chunk, all_bin_data) = res?;

        let mut pair_idx = 0;
        for amm in chunk.iter_mut() {
            if let AMM::MoeLbPair(pair) = amm {
                if pair_idx < all_bin_data.len() {
                    let bin_data = &all_bin_data[pair_idx];
                    let active_id = pair.active_id;
                    let start_id = active_id.saturating_sub(bins_radius);
                    let end_id = active_id.saturating_add(bins_radius);
                    let expected = (u64::from(end_id) - u64::from(start_id) + 1) as usize;
                    if bin_data.len() != expected {
                        return Err(MoeError::MalformedBatchResponse {
                            expected,
                            actual: bin_data.len(),
                        }
                        .into());
                    }
                    pair.bins
                        .retain(|bin_id, _| *bin_id < start_id || *bin_id > end_id);

                    for (offset, (reserve_x, reserve_y)) in bin_data.iter().enumerate() {
                        if *reserve_x > 0 || *reserve_y > 0 {
                            let bin_id = start_id + offset as u32;
                            pair.bins.insert(
                                bin_id,
                                BinReserve {
                                    reserve_x: *reserve_x,
                                    reserve_y: *reserve_y,
                                },
                            );
                        }
                    }
                    pair.snapshot = None;
                    pair_idx += 1;
                }
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
            if let Some(&dx) = decs.get(&p.token_x.address) {
                p.token_x.decimals = dx;
            }
            if let Some(&dy) = decs.get(&p.token_y.address) {
                p.token_y.decimals = dy;
            }
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

            // IMPORTANT: Only update active_id, NOT reserves!
            //
            // Each Swap event represents changes to a SINGLE BIN, not the entire pool.
            // A user swap may cross multiple bins and trigger multiple Swap events.
            // The amountsIn/amountsOut in each event are for that specific bin only.
            //
            // Total pool reserves (reserve_x, reserve_y) should be:
            // 1. Re-synced from chain when needed (via getReserves() call)
            // 2. Or calculated by summing all individual bin reserves
            //
            // We do NOT update total reserves here because:
            // - Swap events are per-bin, not per-pool
            // - We would need to track all bins to accurately maintain total reserves
            // - For arbitrage monitoring, active_id changes are sufficient to detect opportunities
            self.active_id = ev.id.to::<u32>();
            self.snapshot = None;

            // Optionally decode amounts for logging/debugging
            if tracing::enabled!(tracing::Level::TRACE) {
                let amounts_in_bytes = ev.amountsIn.as_slice();
                let amount_in_x =
                    u128::from_be_bytes(amounts_in_bytes[0..16].try_into().unwrap_or([0u8; 16]));
                let amount_in_y =
                    u128::from_be_bytes(amounts_in_bytes[16..32].try_into().unwrap_or([0u8; 16]));

                let amounts_out_bytes = ev.amountsOut.as_slice();
                let amount_out_x =
                    u128::from_be_bytes(amounts_out_bytes[0..16].try_into().unwrap_or([0u8; 16]));
                let amount_out_y =
                    u128::from_be_bytes(amounts_out_bytes[16..32].try_into().unwrap_or([0u8; 16]));

                tracing::trace!(
                    target: "moe.sync.swap",
                    address = %self.address,
                    bin_id = %ev.id,
                    amount_in_x,
                    amount_in_y,
                    amount_out_x,
                    amount_out_y,
                    "Swap event in bin"
                );
            }

            Ok(())
        } else if sig == IMoeLBPairEvents::DepositedToBins::SIGNATURE_HASH {
            let ev = IMoeLBPairEvents::DepositedToBins::decode_log(log.as_ref())?;

            // Clone to avoid move errors
            let ids: Vec<U256> = ev.ids.clone();

            // Convert Vec<alloy::primitives::FixedBytes<32>> to Vec<[u8; 32]>
            let amounts: Vec<[u8; 32]> = ev.amounts.iter().map(|fb| fb.0).collect();

            self.update_bins(ids, amounts, true)?;
            Ok(())
        } else if sig == IMoeLBPairEvents::WithdrawnFromBins::SIGNATURE_HASH {
            let ev = IMoeLBPairEvents::WithdrawnFromBins::decode_log(log.as_ref())?;

            // Clone to avoid move errors
            let ids: Vec<U256> = ev.ids.clone();
            let amounts: Vec<[u8; 32]> = ev.amounts.iter().map(|fb| fb.0).collect();

            self.update_bins(ids, amounts, false)?;
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
        let timestamp = self
            .snapshot
            .as_ref()
            .ok_or(MoeError::IncompleteState)?
            .block_timestamp;
        self.simulate_swap_with_timestamp(base_token, _quote_token, amount_in, timestamp)
    }

    fn simulate_swap_with_timestamp(
        &self,
        base_token: Address,
        _quote_token: Address,
        amount_in: U256,
        timestamp: u64,
    ) -> Result<U256, AMMError> {
        let swap_for_y = if base_token == self.token_x.address {
            true
        } else if base_token == self.token_y.address {
            false
        } else {
            return Err(MoeError::UnsupportedToken.into());
        };
        let mut quote = self.snapshot_quote(timestamp)?;
        if amount_in.is_zero() {
            return Ok(U256::ZERO);
        }
        simulate_swap_precise_inner(&mut quote, swap_for_y, amount_in)
    }

    fn simulate_swap_mut(
        &mut self,
        base_token: Address,
        _quote_token: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        let timestamp = self
            .snapshot
            .as_ref()
            .ok_or(MoeError::IncompleteState)?
            .block_timestamp;
        let swap_for_y = if base_token == self.token_x.address {
            true
        } else if base_token == self.token_y.address {
            false
        } else {
            return Err(MoeError::UnsupportedToken.into());
        };
        self.simulate_swap_precise(swap_for_y, amount_in, timestamp)
    }

    fn tokens(&self) -> Vec<Address> {
        vec![self.token_x.address, self.token_y.address]
    }

    fn calculate_price(&self, base_token: Address, _quote_token: Address) -> Result<f64, AMMError> {
        let snapshot = self.snapshot.as_ref().ok_or(MoeError::IncompleteState)?;
        let dx = self.token_x.decimals as i16;
        let dy = self.token_y.decimals as i16;
        let shift = dx - dy;
        let ratio = price_from_id_to_f64(snapshot.slot0.active_id, snapshot.slot0.bin_step)?;
        let price_x_in_y = match shift.cmp(&0) {
            Ordering::Less => ratio / 10f64.powi(i32::from(-shift)),
            Ordering::Greater => ratio * 10f64.powi(i32::from(shift)),
            Ordering::Equal => ratio,
        };
        if base_token == self.token_x.address {
            Ok(price_x_in_y)
        } else if base_token == self.token_y.address {
            Ok(1.0 / price_x_in_y)
        } else {
            Err(MoeError::UnsupportedToken.into())
        }
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
        if let Some(AMM::MoeLbPair(updated)) = v.into_iter().next() {
            Ok(updated)
        } else {
            Ok(self)
        }
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
        Self {
            address,
            creation_block,
        }
    }

    pub async fn get_all_pools<N, P>(
        &self,
        to_block: BlockId,
        provider: P,
    ) -> Result<Vec<AMM>, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let to_block_num = match to_block {
            BlockId::Number(num) => match num {
                alloy::eips::BlockNumberOrTag::Number(n) => n,
                _ => provider.get_block_number().await?,
            },
            _ => provider.get_block_number().await?,
        };

        let logs = pool_list::fetch_chunked_factory_logs(
            provider,
            self.address(),
            self.pool_creation_event(),
            self.creation_block,
            to_block_num,
        )
        .await
        .map_err(MoeError::from)?;
        let mut pools = Vec::with_capacity(logs.len());
        for log in logs {
            pools.push(self.create_pool(log)?);
        }
        Ok(pools)
    }

    pub async fn sync_all_pools<N, P>(
        &self,
        mut pools: Vec<AMM>,
        block_number: BlockId,
        provider: P,
    ) -> Result<Vec<AMM>, AMMError>
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
                    x.reserve_x > 0
                        && x.reserve_y > 0
                        && x.token_x.decimals > 0
                        && x.token_y.decimals > 0
                }
                _ => true,
            })
            .collect();
        Ok(pools)
    }
}

use std::future::Future;

impl AutomatedMarketMakerFactory for MoeFactory {
    type PoolVariant = MoeLbPair;
    fn address(&self) -> Address {
        self.address
    }
    fn create_pool(&self, log: Log) -> Result<AMM, AMMError> {
        let entry =
            pool_list::entry_from_creation_log(log, self.address).map_err(MoeError::from)?;
        Ok(AMM::MoeLbPair(MoeLbPair {
            address: entry.pool,
            token_x: entry.token_x.into(),
            token_y: entry.token_y.into(),
            bin_step: entry.bin_step,
            ..Default::default()
        }))
    }
    fn creation_block(&self) -> u64 {
        self.creation_block
    }
    fn pool_creation_event(&self) -> B256 {
        IMoeFactory::LBPairCreated::SIGNATURE_HASH
    }
}

impl DiscoverySync for MoeFactory {
    fn discover<N, P>(
        &self,
        to_block: BlockId,
        provider: P,
    ) -> impl Future<Output = Result<Vec<AMM>, AMMError>>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        info!(target = "amms::moe::discover", address = ?self.address, "Discovering Moe pools");
        self.get_all_pools::<N, _>(to_block, provider)
    }
    fn sync<N, P>(
        &self,
        amms: Vec<AMM>,
        to_block: BlockId,
        provider: P,
    ) -> impl Future<Output = Result<Vec<AMM>, AMMError>>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        info!(target = "amms::moe::sync", address = ?self.address, "Syncing Moe pools");
        self.sync_all_pools::<N, _>(amms, to_block, provider)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    /// Create a mock MoeLbPair for testing
    fn create_mock_pair() -> MoeLbPair {
        let address = address!("0x1234567890123456789012345678901234567890");
        let mut pair = MoeLbPair::new(address);

        pair.token_x = Token {
            address: address!("0xdEAddEaDdeAddEAddeadDEadDEADDEAddead0000"),
            decimals: 18,
        };
        pair.token_y = Token {
            address: address!("0x0d500B1d8E8eF31E21C99d1Db9A6444d3ADf1270"),
            decimals: 6,
        };
        pair.bin_step = 20; // 0.2% bin step
        pair.active_id = 8388608; // 2^23, neutral bin
        pair.reserve_x = 1_000_000_000_000_000_000; // 1.0 X token
        pair.reserve_y = 1_000_000; // 1.0 Y token (6 decimals)
        pair.protocol_share_bps = 100; // 1%
        pair.max_volatility_acc = 250000;

        pair
    }

    fn install_mock_snapshot(pair: &mut MoeLbPair, radius: u32, timestamp: u64) {
        let range = MoeBinRange::new(
            pair.active_id.saturating_sub(radius),
            pair.active_id.saturating_add(radius),
        );
        let snapshot = MoeSnapshot::new(
            pair.snapshot_slot0(),
            pair.bins.clone(),
            vec![range],
            MoeSnapshotContext::new(B256::repeat_byte(1), timestamp),
        )
        .unwrap();
        pair.install_snapshot(snapshot).unwrap();
    }

    #[test]
    fn test_new_pair() {
        let address = address!("0x1234567890123456789012345678901234567890");
        let pair = MoeLbPair::new(address);

        assert_eq!(pair.address, address);
        assert_eq!(pair.active_id, 0);
        assert_eq!(pair.reserve_x, 0);
        assert_eq!(pair.reserve_y, 0);
        assert!(pair.bins.is_empty());
    }

    #[test]
    fn test_get_price_from_id() {
        let pair = create_mock_pair();

        // At active_id (2^23), price should be 1.0
        let price_at_active = pair.get_price_from_id(8388608).unwrap();
        assert!(
            (price_at_active - 1.0).abs() < 1e-9,
            "Price at active_id should be ~1.0"
        );

        // One bin above active_id: (1 + 0.002)^1
        let price_above = pair.get_price_from_id(8388609).unwrap();
        let expected_above = 1.002;
        assert!(
            (price_above - expected_above).abs() < 1e-6,
            "Price one bin above should be ~1.002, got {}",
            price_above
        );

        // One bin below active_id: 1/1.002 ≈ 0.99800399
        let price_below = pair.get_price_from_id(8388607).unwrap();
        let expected_below = 1.0 / 1.002;
        assert!(
            (price_below - expected_below).abs() < 1e-9,
            "Price one bin below should be ~{expected_below}, got {price_below}"
        );
    }

    #[test]
    fn test_update_bins_deposit() {
        let mut pair = create_mock_pair();

        // Create deposit data
        let bin_id = 8388608u32;
        let ids = vec![U256::from(bin_id)];

        // Create packed amounts using big-endian encoding (EVM standard)
        let amount_x = 1_000_000u128;
        let amount_y = 2_000_000u128;
        let mut packed = [0u8; 32];
        packed[0..16].copy_from_slice(&amount_x.to_be_bytes());
        packed[16..32].copy_from_slice(&amount_y.to_be_bytes());
        let amounts = vec![packed];

        let initial_reserve_x = pair.reserve_x;
        let initial_reserve_y = pair.reserve_y;

        // Execute deposit
        pair.update_bins(ids.clone(), amounts.clone(), true)
            .unwrap();

        // Verify bin was created
        assert!(pair.bins.contains_key(&bin_id));
        let bin = pair.bins.get(&bin_id).unwrap();
        assert_eq!(bin.reserve_x, amount_x);
        assert_eq!(bin.reserve_y, amount_y);

        // Verify total reserves updated
        assert_eq!(pair.reserve_x, initial_reserve_x + amount_x);
        assert_eq!(pair.reserve_y, initial_reserve_y + amount_y);

        // Add more to the same bin
        pair.update_bins(ids, amounts, true).unwrap();
        let bin = pair.bins.get(&bin_id).unwrap();
        assert_eq!(bin.reserve_x, amount_x * 2);
        assert_eq!(bin.reserve_y, amount_y * 2);
    }

    #[test]
    fn test_update_bins_withdraw() {
        let mut pair = create_mock_pair();
        let bin_id = 8388608u32;

        // First deposit some liquidity
        let amount_x = 2_000_000u128;
        let amount_y = 3_000_000u128;
        let mut packed = [0u8; 32];
        packed[0..16].copy_from_slice(&amount_x.to_be_bytes());
        packed[16..32].copy_from_slice(&amount_y.to_be_bytes());

        let ids = vec![U256::from(bin_id)];
        let amounts = vec![packed];

        pair.update_bins(ids.clone(), amounts.clone(), true)
            .unwrap();

        // Now withdraw half
        let withdraw_x = 1_000_000u128;
        let withdraw_y = 1_500_000u128;
        let mut withdraw_packed = [0u8; 32];
        withdraw_packed[0..16].copy_from_slice(&withdraw_x.to_be_bytes());
        withdraw_packed[16..32].copy_from_slice(&withdraw_y.to_be_bytes());
        let withdraw_amounts = vec![withdraw_packed];

        let reserve_x_before = pair.reserve_x;
        let reserve_y_before = pair.reserve_y;

        pair.update_bins(ids.clone(), withdraw_amounts, false)
            .unwrap();

        // Verify bin reserves decreased
        let bin = pair.bins.get(&bin_id).unwrap();
        assert_eq!(bin.reserve_x, amount_x - withdraw_x);
        assert_eq!(bin.reserve_y, amount_y - withdraw_y);

        // Verify total reserves decreased
        assert_eq!(pair.reserve_x, reserve_x_before - withdraw_x);
        assert_eq!(pair.reserve_y, reserve_y_before - withdraw_y);
    }

    #[test]
    fn test_update_bins_overflow_does_not_commit_partial_state() {
        let mut pair = create_mock_pair();
        let before_reserve_x = pair.reserve_x;
        let before_reserve_y = pair.reserve_y;
        let mut amount = [0u8; 32];
        amount[..16].copy_from_slice(&u128::MAX.to_be_bytes());

        let result = pair.update_bins(vec![U256::from(pair.active_id)], vec![amount], true);

        assert!(matches!(
            result,
            Err(AMMError::MoeError(MoeError::Arithmetic))
        ));
        assert_eq!(pair.reserve_x, before_reserve_x);
        assert_eq!(pair.reserve_y, before_reserve_y);
        assert!(pair.bins.is_empty());
    }

    #[test]
    fn test_update_bins_full_withdraw_removes_bin() {
        let mut pair = create_mock_pair();
        let bin_id = 8388608u32;

        // Deposit liquidity
        let amount_x = 1_000_000u128;
        let amount_y = 2_000_000u128;
        let mut packed = [0u8; 32];
        packed[0..16].copy_from_slice(&amount_x.to_be_bytes());
        packed[16..32].copy_from_slice(&amount_y.to_be_bytes());

        let ids = vec![U256::from(bin_id)];
        let amounts = vec![packed];

        pair.update_bins(ids.clone(), amounts.clone(), true)
            .unwrap();
        assert!(pair.bins.contains_key(&bin_id));

        // Withdraw all
        pair.update_bins(ids, amounts, false).unwrap();

        // Bin should be removed
        assert!(!pair.bins.contains_key(&bin_id));
    }

    #[test]
    fn test_simulate_swap_simple() {
        let mut pair = create_mock_pair();
        pair.time_of_last_update = 1_700_000_000;
        pair.reserve_x = 0;
        pair.reserve_y = 0;

        // Add bins with liquidity and keep pair totals consistent.
        for i in 0..5 {
            let bin_id = pair.active_id - i;
            let rx = 1_000_000_000u128;
            let ry = 1_000_000u128;
            pair.bins.insert(
                bin_id,
                BinReserve {
                    reserve_x: rx,
                    reserve_y: ry,
                },
            );
            pair.reserve_x += rx;
            pair.reserve_y += ry;
        }
        install_mock_snapshot(&mut pair, 4, 1_700_000_000);

        let amount_in = U256::from(100_000u128);
        let amount_out = pair
            .simulate_swap_precise(true, amount_in, u64::from(pair.time_of_last_update))
            .unwrap();

        assert!(amount_out > U256::ZERO, "Should receive some output");
    }

    #[test]
    fn test_simulate_swap_across_bins() {
        let mut test_pair = create_mock_pair();
        let timestamp: u64 = 1_700_000_000;
        test_pair.time_of_last_update = timestamp;
        test_pair.reserve_x = 15_000_000_000;
        test_pair.reserve_y = 15_000_000;

        test_pair.bins.insert(
            test_pair.active_id,
            BinReserve {
                reserve_x: 10_000_000_000,
                reserve_y: 10_000_000,
            },
        );
        install_mock_snapshot(&mut test_pair, 1, timestamp);
        // X->Y walks toward lower ids.
        test_pair.bins.insert(
            test_pair.active_id - 1,
            BinReserve {
                reserve_x: 5_000_000_000,
                reserve_y: 5_000_000,
            },
        );
        install_mock_snapshot(&mut test_pair, 1, timestamp);

        let amount_in = U256::from(1_000_000u128);
        let amount_out = test_pair
            .simulate_swap_precise(true, amount_in, timestamp)
            .unwrap();

        assert!(amount_out > U256::ZERO);
    }

    #[test]
    fn test_simulate_swap_across_bins_reverse() {
        let mut test_pair = create_mock_pair();
        let timestamp: u64 = 1_700_000_000;
        test_pair.time_of_last_update = timestamp;
        test_pair.reserve_x = 15_000_000_000;
        test_pair.reserve_y = 15_000_000;

        test_pair.bins.insert(
            test_pair.active_id,
            BinReserve {
                reserve_x: 10_000_000_000,
                reserve_y: 10_000_000,
            },
        );
        // Y->X walks toward higher ids.
        test_pair.bins.insert(
            test_pair.active_id + 1,
            BinReserve {
                reserve_x: 5_000_000_000,
                reserve_y: 5_000_000,
            },
        );
        install_mock_snapshot(&mut test_pair, 1, timestamp);

        let amount_in = U256::from(1_000_000u128);
        let amount_out = test_pair
            .simulate_swap_precise(false, amount_in, timestamp)
            .unwrap();

        assert!(amount_out > U256::ZERO);
    }

    #[test]
    fn test_simulate_swap_accepts_one_sided_output_bin() {
        let mut pair = create_mock_pair();
        let timestamp: u64 = 1_700_000_000;
        pair.reserve_x = 0;
        pair.reserve_y = 10_000_000;
        pair.bins.insert(
            pair.active_id,
            BinReserve {
                reserve_x: 0,
                reserve_y: 10_000_000,
            },
        );
        install_mock_snapshot(&mut pair, 0, timestamp);

        let amount_out = pair
            .simulate_swap_precise(true, U256::from(1_000u64), timestamp)
            .unwrap();

        assert!(amount_out > U256::ZERO);
    }

    #[test]
    fn test_simulate_swap_mut() {
        let mut pair = create_mock_pair();
        pair.time_of_last_update = 1_700_000_000;
        pair.reserve_x = 10_000_000_000;
        pair.reserve_y = 10_000_000;
        pair.bins.insert(
            pair.active_id,
            BinReserve {
                reserve_x: 10_000_000_000,
                reserve_y: 10_000_000,
            },
        );
        install_mock_snapshot(&mut pair, 0, 1_700_000_000);

        let initial_reserve_x = pair.reserve_x;
        let initial_reserve_y = pair.reserve_y;

        let amount_in = U256::from(100_000u128);
        let amount_out = pair
            .simulate_swap_precise(true, amount_in, u64::from(pair.time_of_last_update))
            .unwrap();

        assert!(pair.reserve_x > initial_reserve_x);
        assert!(pair.reserve_y < initial_reserve_y);
        assert!(amount_out > U256::ZERO);
    }

    #[test]
    fn test_calculate_price() {
        let mut pair = create_mock_pair();
        install_mock_snapshot(&mut pair, 0, 1_700_000_000);

        // Calculate price of X in terms of Y
        let price_x_in_y = pair
            .calculate_price(pair.token_x.address, pair.token_y.address)
            .unwrap();

        assert!((price_x_in_y - 1e12).abs() / 1e12 < 1e-12);

        // Calculate price of Y in terms of X
        let price_y_in_x = pair
            .calculate_price(pair.token_y.address, pair.token_x.address)
            .unwrap();
        assert!((price_y_in_x - 1e-12).abs() / 1e-12 < 1e-12);

        // Product should be ~1.0 (reciprocal relationship)
        let product = price_x_in_y * price_y_in_x;
        assert!(
            (product - 1.0).abs() < 1e-6,
            "Price product should be ~1.0, got {}",
            product
        );
    }

    #[test]
    fn test_tokens() {
        let pair = create_mock_pair();
        let tokens = pair.tokens();

        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0], pair.token_x.address);
        assert_eq!(tokens[1], pair.token_y.address);
    }

    #[test]
    fn test_factory_creation() {
        let factory_address = CANONICAL_MOE_FACTORY;
        let creation_block = CANONICAL_MOE_FACTORY_CREATION_BLOCK;

        let factory = MoeFactory::new(factory_address, creation_block);

        assert_eq!(factory.address(), factory_address);
        assert_eq!(factory.creation_block(), creation_block);
    }

    #[test]
    fn test_multiple_bins() {
        let mut pair = create_mock_pair();

        // Add liquidity to multiple bins
        let bin_ids: Vec<U256> = (0..10).map(|i| U256::from(pair.active_id + i)).collect();
        let mut amounts = Vec::new();

        for i in 0..10 {
            let amount_x = (i + 1) * 1_000_000u128;
            let amount_y = (i + 1) * 500_000u128;
            let mut packed = [0u8; 32];
            packed[0..16].copy_from_slice(&amount_x.to_be_bytes());
            packed[16..32].copy_from_slice(&amount_y.to_be_bytes());
            amounts.push(packed);
        }

        pair.update_bins(bin_ids.clone(), amounts, true).unwrap();

        // Verify all bins were created
        assert_eq!(pair.bins.len(), 10);

        // Verify each bin has correct reserves
        for (i, bin_id) in bin_ids.iter().enumerate() {
            let bin = pair.bins.get(&bin_id.to::<u32>()).unwrap();
            assert_eq!(bin.reserve_x, (i as u128 + 1) * 1_000_000);
            assert_eq!(bin.reserve_y, (i as u128 + 1) * 500_000);
        }
    }

    #[test]
    fn test_swap_with_zero_amount() {
        let mut pair = create_mock_pair();
        install_mock_snapshot(&mut pair, 0, 1_700_000_000);

        let amount_out = pair
            .simulate_swap(pair.token_x.address, pair.token_y.address, U256::ZERO)
            .unwrap();

        assert_eq!(amount_out, U256::ZERO);
    }

    #[test]
    fn zero_amount_quote_requires_snapshot() {
        let pair = create_mock_pair();
        let result = pair.simulate_swap_with_timestamp(
            pair.token_x.address,
            pair.token_y.address,
            U256::ZERO,
            1_700_000_000,
        );

        assert!(matches!(
            result,
            Err(AMMError::MoeError(MoeError::IncompleteState))
        ));
    }

    #[test]
    fn quote_rejects_unsynced_bins() {
        let mut pair = create_mock_pair();

        // Ensure bins are empty, but reserves are set
        pair.bins.clear();
        pair.reserve_x = 1_000_000_000_000_000_000;
        pair.reserve_y = 1_000_000;

        let amount_in = U256::from(100_000_000_000_000_000u128); // 0.1 token
        let result = pair.simulate_swap(pair.token_x.address, pair.token_y.address, amount_in);

        assert!(matches!(
            result,
            Err(AMMError::MoeError(MoeError::IncompleteState))
        ));
    }

    #[test]
    fn quote_rejects_liquidity_past_queried_coverage() {
        let mut pair = create_mock_pair();
        pair.reserve_x = 10;
        pair.reserve_y = 1;
        pair.bins.insert(
            pair.active_id,
            BinReserve {
                reserve_x: 10,
                reserve_y: 1,
            },
        );
        install_mock_snapshot(&mut pair, 0, 1_700_000_000);

        let result =
            pair.simulate_swap(pair.token_x.address, pair.token_y.address, U256::from(2u8));

        assert!(matches!(
            result,
            Err(AMMError::MoeError(MoeError::IncompleteState))
        ));
    }

    #[test]
    fn quote_uses_its_snapshot_header_timestamp() {
        let mut pair = create_mock_pair();
        pair.time_of_last_update = 1;
        pair.reserve_x = 1_000_000;
        pair.reserve_y = 1_000_000;
        pair.bins.insert(
            pair.active_id,
            BinReserve {
                reserve_x: 1_000_000,
                reserve_y: 1_000_000,
            },
        );
        install_mock_snapshot(&mut pair, 0, 1_700_000_000);

        let quote = pair.simulate_swap(pair.token_x.address, pair.token_y.address, U256::from(1u8));
        let mismatched = pair.simulate_swap_with_timestamp(
            pair.token_x.address,
            pair.token_y.address,
            U256::from(1u8),
            1_700_000_001,
        );

        assert!(quote.is_ok());
        assert!(matches!(
            mismatched,
            Err(AMMError::MoeError(
                MoeError::SnapshotTimestampMismatch { .. }
            ))
        ));
    }

    #[test]
    fn quote_timestamp_changes_time_dependent_fee_state() {
        fn pair_at(timestamp: u64) -> MoeLbPair {
            let mut pair = create_mock_pair();
            pair.time_of_last_update = 1;
            pair.filter_period = 100;
            pair.decay_period = 200;
            pair.reduction_factor = 5_000;
            pair.variable_fee_control = 100_000;
            pair.volatility_accumulator = 1_000;
            pair.id_reference = pair.active_id - 5;
            pair.reserve_x = 1_000_000_000;
            pair.reserve_y = 1_000_000_000;
            pair.bins.insert(
                pair.active_id,
                BinReserve {
                    reserve_x: 1_000_000_000,
                    reserve_y: 1_000_000_000,
                },
            );
            install_mock_snapshot(&mut pair, 0, timestamp);
            pair
        }

        let early_pair = pair_at(50);
        let early = early_pair
            .simulate_swap(
                early_pair.token_x.address,
                early_pair.token_y.address,
                U256::from(1_000_000u64),
            )
            .unwrap();
        let late_pair = pair_at(150);
        let late = late_pair
            .simulate_swap(
                late_pair.token_x.address,
                late_pair.token_y.address,
                U256::from(1_000_000u64),
            )
            .unwrap();

        assert_ne!(early, late);
    }

    #[test]
    fn large_protocol_valid_reserves_are_quoted() {
        let mut pair = create_mock_pair();
        let reserve = 10u128.pow(31);
        pair.reserve_x = reserve;
        pair.reserve_y = reserve;
        pair.bins.insert(
            pair.active_id,
            BinReserve {
                reserve_x: reserve,
                reserve_y: reserve,
            },
        );
        install_mock_snapshot(&mut pair, 0, 1_700_000_000);

        let quote = pair
            .simulate_swap(
                pair.token_x.address,
                pair.token_y.address,
                U256::from(1_000_000u64),
            )
            .unwrap();

        assert!(quote > U256::ZERO);
    }

    #[test]
    fn price_uses_the_snapshot_active_id() {
        let mut pair = create_mock_pair();
        pair.token_x.decimals = 0;
        pair.token_y.decimals = 0;
        install_mock_snapshot(&mut pair, 0, 1_700_000_000);
        pair.snapshot.as_mut().unwrap().slot0.active_id += 1;

        let price = pair
            .calculate_price(pair.token_x.address, pair.token_y.address)
            .unwrap();

        assert!((price - 1.002).abs() < 1e-12);
    }

    #[test]
    fn test_bin_step_variations() {
        let mut pair = create_mock_pair();

        // Test with different bin steps
        let test_steps = vec![1, 10, 20, 50, 100]; // Various bin steps

        for step in test_steps {
            pair.bin_step = step;

            let price_at_active = pair.get_price_from_id(8388608).unwrap();
            assert!((price_at_active - 1.0).abs() < 1e-9);

            let price_above = pair.get_price_from_id(8388609).unwrap();
            let expected = 1.0 + (step as f64 / 10000.0);
            assert!(
                (price_above - expected).abs() < 1e-6,
                "Step {}: expected {}, got {}",
                step,
                expected,
                price_above
            );
        }
    }

    #[test]
    fn test_address_method() {
        let address = address!("0x1234567890123456789012345678901234567890");
        let pair = MoeLbPair::new(address);

        assert_eq!(pair.address(), address);
    }

    #[test]
    fn test_sync_events() {
        let pair = create_mock_pair();
        let events = pair.sync_events();

        assert_eq!(events.len(), 3);
        assert_eq!(events[0], IMoeLBPairEvents::Swap::SIGNATURE_HASH);
        assert_eq!(events[1], IMoeLBPairEvents::DepositedToBins::SIGNATURE_HASH);
        assert_eq!(
            events[2],
            IMoeLBPairEvents::WithdrawnFromBins::SIGNATURE_HASH
        );
    }
}

fn simulate_swap_precise_inner(
    pair: &mut MoeLbPair,
    swap_for_y: bool,
    mut amount_left: U256,
) -> Result<U256, AMMError> {
    let queried_ranges = pair
        .snapshot
        .as_ref()
        .ok_or(MoeError::IncompleteState)?
        .queried_ranges
        .clone();

    let mut parameters = MoeParameters::from_pair(pair);
    let timestamp = pair
        .snapshot
        .as_ref()
        .ok_or(MoeError::IncompleteState)?
        .block_timestamp;
    parameters.update_references(timestamp);

    let mut amount_out = U256::ZERO;
    let mut current_id = pair.active_id;
    let mut loops = 0usize;

    let mut visited_bins = std::collections::HashSet::new();

    while !amount_left.is_zero() {
        if loops == MAX_ITERATIONS
            || !queried_ranges
                .iter()
                .any(|range| range.contains(current_id))
        {
            return Err(MoeError::IncompleteState.into());
        }
        loops += 1;

        if !pair.bins.contains_key(&current_id) {
            let next_bin_id = next_id(current_id, swap_for_y);
            if next_bin_id == current_id || !visited_bins.insert(next_bin_id) {
                return Err(MoeError::IncompleteState.into());
            }
            current_id = next_bin_id;
            continue;
        }

        parameters.update_volatility_accumulator(current_id);

        let result =
            simulate_single_bin(pair, &mut parameters, current_id, swap_for_y, amount_left)?;

        if !result.amount_out.is_zero() {
            amount_out = amount_out
                .checked_add(result.amount_out)
                .ok_or(MoeError::Arithmetic)?;
        }

        if !result.amount_in_with_fee.is_zero() {
            if result.amount_in_with_fee >= amount_left {
                parameters.write_back(pair);
                return Ok(amount_out);
            }
            amount_left = amount_left
                .checked_sub(result.amount_in_with_fee)
                .ok_or(MoeError::Arithmetic)?;
        }

        if result.bin_exhausted || result.amount_out.is_zero() {
            let next_bin_id = next_id(current_id, swap_for_y);
            if next_bin_id == current_id || !visited_bins.insert(next_bin_id) {
                return Err(MoeError::IncompleteState.into());
            }
            current_id = next_bin_id;
            continue;
        }

        return Err(MoeError::IncompleteState.into());
    }

    parameters.write_back(pair);
    Ok(amount_out)
}

fn next_id(id: u32, swap_for_y: bool) -> u32 {
    // In Moe LB:
    // - Higher bin ID = higher price (more X, less Y)
    // - Lower bin ID = lower price (less X, more Y)
    //
    // When swapping X for Y (swap_for_y=true):
    //   - We're selling X and buying Y
    //   - We need bins with Y liquidity (lower price bins)
    //   - So we move DOWN (id - 1)
    //
    // When swapping Y for X (swap_for_y=false):
    //   - We're selling Y and buying X
    //   - We need bins with X liquidity (higher price bins)
    //   - So we move UP (id + 1)
    if swap_for_y {
        id.saturating_sub(1) // Move to lower price bins
    } else {
        id.saturating_add(1) // Move to higher price bins
    }
}

fn simulate_single_bin(
    pair: &mut MoeLbPair,
    parameters: &mut MoeParameters,
    bin_id: u32,
    swap_for_y: bool,
    amount_in: U256,
) -> Result<BinSwapResult, AMMError> {
    let (amount_in_with_fee, amount_out, fee_paid, bin_exhausted) =
        compute_bin_swap(pair, parameters, bin_id, swap_for_y, amount_in)?;

    if amount_out.is_zero() {
        return Ok(BinSwapResult {
            amount_in_with_fee: U256::ZERO,
            amount_out: U256::ZERO,
            fee_paid: U256::ZERO,
            protocol_fee: U256::ZERO,
            bin_exhausted,
        });
    }

    let mut bin = pair.bins.get(&bin_id).cloned().unwrap_or_default();

    let amount_out_u128 = amount_out.try_into().map_err(|_| MoeError::Arithmetic)?;
    let amount_in_with_fee_u128: u128 = amount_in_with_fee
        .try_into()
        .map_err(|_| MoeError::Arithmetic)?;
    let protocol_fee_u256 = parameters.protocol_fee_amount_u256(fee_paid);
    let protocol_fee_u128: u128 = protocol_fee_u256
        .try_into()
        .map_err(|_| MoeError::Arithmetic)?;

    let amount_after_protocol = amount_in_with_fee_u128
        .checked_sub(protocol_fee_u128)
        .ok_or(MoeError::Arithmetic)?;

    if swap_for_y {
        bin.reserve_x = bin
            .reserve_x
            .checked_add(amount_after_protocol)
            .ok_or(MoeError::Arithmetic)?;
        bin.reserve_y = bin
            .reserve_y
            .checked_sub(amount_out_u128)
            .ok_or(MoeError::InsufficientLiquidity)?;
        pair.reserve_x = pair
            .reserve_x
            .checked_add(amount_after_protocol)
            .ok_or(MoeError::Arithmetic)?;
        pair.reserve_y = pair
            .reserve_y
            .checked_sub(amount_out_u128)
            .ok_or(MoeError::InsufficientLiquidity)?;
    } else {
        bin.reserve_y = bin
            .reserve_y
            .checked_add(amount_after_protocol)
            .ok_or(MoeError::Arithmetic)?;
        bin.reserve_x = bin
            .reserve_x
            .checked_sub(amount_out_u128)
            .ok_or(MoeError::InsufficientLiquidity)?;
        pair.reserve_y = pair
            .reserve_y
            .checked_add(amount_after_protocol)
            .ok_or(MoeError::Arithmetic)?;
        pair.reserve_x = pair
            .reserve_x
            .checked_sub(amount_out_u128)
            .ok_or(MoeError::InsufficientLiquidity)?;
    }

    if bin.reserve_x == 0 && bin.reserve_y == 0 {
        pair.bins.remove(&bin_id);
    } else {
        pair.bins.insert(bin_id, bin);
    }

    Ok(BinSwapResult {
        amount_in_with_fee,
        amount_out,
        fee_paid,
        protocol_fee: protocol_fee_u256,
        bin_exhausted,
    })
}

struct BinSwapResult {
    amount_in_with_fee: U256,
    amount_out: U256,
    fee_paid: U256,
    protocol_fee: U256,
    bin_exhausted: bool,
}

fn compute_bin_swap(
    pair: &MoeLbPair,
    params: &MoeParameters,
    bin_id: u32,
    swap_for_y: bool,
    amount_left: U256,
) -> Result<(U256, U256, U256, bool), AMMError> {
    // Get the bin, return zero if it doesn't exist
    let bin = match pair.bins.get(&bin_id) {
        Some(b) => b.clone(),
        None => {
            // Bin doesn't exist, return zero output
            return Ok((U256::ZERO, U256::ZERO, U256::ZERO, true));
        }
    };

    if (swap_for_y && bin.reserve_y == 0) || (!swap_for_y && bin.reserve_x == 0) {
        return Ok((U256::ZERO, U256::ZERO, U256::ZERO, true));
    }

    let mut parameters_u256 = pair_parameter_helper::set_static_fee_parameters(
        U256::ZERO,
        params.base_factor,
        params.filter_period,
        params.decay_period,
        params.reduction_factor,
        params.variable_fee_control,
        params.protocol_share,
        params.max_volatility_accumulator,
    )
    .map_err(|_| MoeError::Arithmetic)?;
    parameters_u256 = pair_parameter_helper::set_active_id(parameters_u256, params.active_id);
    parameters_u256 = pair_parameter_helper::set_volatility_accumulator(
        parameters_u256,
        params.volatility_accumulator,
    )
    .map_err(|_| MoeError::Arithmetic)?;
    parameters_u256 = pair_parameter_helper::set_volatility_reference(
        parameters_u256,
        params.volatility_reference,
    )
    .map_err(|_| MoeError::Arithmetic)?;
    parameters_u256 = pair_parameter_helper::set_id_reference(parameters_u256, params.id_reference);

    let packed_reserves = packed_uint128_math::encode(bin.reserve_x, bin.reserve_y);
    let input_encoded = packed_uint128_math::encode(
        if swap_for_y {
            amount_left.try_into().map_err(|_| MoeError::Arithmetic)?
        } else {
            0u128
        },
        if swap_for_y {
            0u128
        } else {
            amount_left.try_into().map_err(|_| MoeError::Arithmetic)?
        },
    );

    let (amounts_in_with_fee, amounts_out, total_fees) = bin_helper::get_amounts(
        packed_reserves,
        parameters_u256,
        pair.bin_step,
        swap_for_y,
        bin_id, // Use current bin_id, not pair.active_id!
        input_encoded,
    )
    .map_err(|_| MoeError::Arithmetic)?;

    let amount_in_with_fee = if swap_for_y {
        packed_uint128_math::decode_x(amounts_in_with_fee)
    } else {
        packed_uint128_math::decode_y(amounts_in_with_fee)
    };
    let amount_out = if swap_for_y {
        packed_uint128_math::decode_y(amounts_out)
    } else {
        packed_uint128_math::decode_x(amounts_out)
    };
    let fee_paid = if swap_for_y {
        packed_uint128_math::decode_x(total_fees)
    } else {
        packed_uint128_math::decode_y(total_fees)
    };
    let bin_exhausted = amount_out
        == if swap_for_y {
            bin.reserve_y
        } else {
            bin.reserve_x
        };

    Ok((
        U256::from(amount_in_with_fee),
        U256::from(amount_out),
        U256::from(fee_paid),
        bin_exhausted,
    ))
}

mod pair_parameters {
    use alloy::primitives::U256;

    use super::BPS_SCALE;
    use crate::amms::moe::math::constants::PRECISION_U128;

    #[derive(Clone, Copy, Debug, Default)]
    pub struct Parameters {
        pub base_factor: u16,
        pub filter_period: u16,
        pub decay_period: u16,
        pub reduction_factor: u16,
        pub variable_fee_control: u32,
        pub protocol_share: u16,
        pub max_volatility_accumulator: u32,
        pub volatility_accumulator: u32,
        pub volatility_reference: u32,
        pub id_reference: u32,
        pub time_of_last_update: u64,
        pub active_id: u32,
        pub bin_step: u16,
    }

    impl Parameters {
        pub fn from_pair(pair: &crate::amms::moe::MoeLbPair) -> Self {
            Self {
                base_factor: pair.base_factor,
                filter_period: pair.filter_period,
                decay_period: pair.decay_period,
                reduction_factor: pair.reduction_factor,
                variable_fee_control: pair.variable_fee_control,
                protocol_share: pair.protocol_share_bps,
                max_volatility_accumulator: pair.max_volatility_acc,
                volatility_accumulator: pair.volatility_accumulator,
                volatility_reference: pair.volatility_reference,
                id_reference: pair.id_reference,
                time_of_last_update: pair.time_of_last_update,
                active_id: pair.active_id,
                bin_step: pair.bin_step,
            }
        }

        pub fn total_fee(&self) -> u128 {
            let base = (self.base_factor as u128) * (self.bin_step as u128) * 10_000_000_000;
            let variable = if self.variable_fee_control == 0 {
                0
            } else {
                let prod = (self.volatility_accumulator as u128) * (self.bin_step as u128);
                (prod * prod * (self.variable_fee_control as u128) + 99) / 100
            };
            base.saturating_add(variable).min(PRECISION_U128)
        }

        pub fn protocol_fee_amount(&self, fee_amount: u128) -> u128 {
            fee_amount * (self.protocol_share as u128) / BPS_SCALE
        }

        pub fn protocol_fee_amount_u256(&self, fee_amount: U256) -> U256 {
            if self.protocol_share == 0 {
                return U256::ZERO;
            }
            U256::from(self.protocol_share) * fee_amount / U256::from(BPS_SCALE)
        }

        pub fn needs_reference_update(&self, timestamp: u64) -> bool {
            let dt = timestamp.saturating_sub(self.time_of_last_update as u64);
            dt >= self.filter_period as u64
        }

        pub fn update_references(&mut self, timestamp: u64) {
            let last_update = self.time_of_last_update as u64;
            let dt = timestamp.saturating_sub(last_update);

            if dt >= self.filter_period as u64 {
                self.id_reference = self.active_id;
                if dt < self.decay_period as u64 {
                    let reduction = (self.reduction_factor as u128)
                        .saturating_mul(self.volatility_accumulator as u128)
                        / (BPS_SCALE as u128);
                    self.volatility_reference = reduction.min(u32::MAX as u128) as u32;
                } else {
                    self.volatility_reference = 0;
                }
            }

            self.time_of_last_update = timestamp.min((1u64 << 40) - 1);
        }

        pub fn update_volatility_accumulator(&mut self, new_active_id: u32) {
            let delta_id = if new_active_id > self.id_reference {
                new_active_id - self.id_reference
            } else {
                self.id_reference - new_active_id
            } as u128;

            let new_acc = (self.volatility_reference as u128)
                .saturating_add(delta_id.saturating_mul(BPS_SCALE as u128))
                .min(self.max_volatility_accumulator as u128);

            self.volatility_accumulator = new_acc as u32;
            self.active_id = new_active_id;
        }

        pub fn write_back(self, pair: &mut crate::amms::moe::MoeLbPair) {
            pair.base_factor = self.base_factor;
            pair.filter_period = self.filter_period;
            pair.decay_period = self.decay_period;
            pair.reduction_factor = self.reduction_factor;
            pair.variable_fee_control = self.variable_fee_control;
            pair.protocol_share_bps = self.protocol_share;
            pair.max_volatility_acc = self.max_volatility_accumulator;
            pair.volatility_accumulator = self.volatility_accumulator;
            pair.volatility_reference = self.volatility_reference;
            pair.id_reference = self.id_reference;
            pair.time_of_last_update = self.time_of_last_update;
            pair.active_id = self.active_id;
        }
    }
}

use pair_parameters::Parameters as MoeParameters;
