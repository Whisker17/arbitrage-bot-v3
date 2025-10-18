use super::{
    amm::{AutomatedMarketMaker, AMM},
    error::AMMError,
    factory::{AutomatedMarketMakerFactory, DiscoverySync},
    get_token_decimals, Token,
};
use crate::amms::{GetMoeLBPairSlot0BatchRequest, GetMoeLBPairBinDataBatchRequest};
use alloy::{
    eips::BlockId,
    network::Network,
    primitives::{Address, B256, U256},
    providers::Provider,
    rpc::types::{Filter, FilterSet, Log},
    sol,
    sol_types::{SolEvent, SolValue},
};
use futures::{stream::FuturesUnordered, StreamExt};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use thiserror::Error;
use tracing::info;
use rayon::iter::{ParallelDrainRange, ParallelIterator};
use std::collections::HashMap;

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
        event LBPairCreated(
            address indexed tokenX,
            address indexed tokenY,
            uint16 indexed binStep,
            address lbPair
        );
    }
}

// ========= Core Type =========

/// Bin data for a specific bin ID
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BinReserve {
    pub reserve_x: u128,
    pub reserve_y: u128,
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
    pub protocol_share_bps: u16,
    pub max_volatility_acc: u32,
    /// Map of bin_id -> bin reserves
    /// This stores the detailed reserves for each bin to enable accurate swap simulation
    pub bins: HashMap<u32, BinReserve>,
}

impl MoeLbPair {
    pub fn new(address: Address) -> Self {
        Self { address, ..Default::default() }
    }

    pub fn address(&self) -> Address { 
        self.address 
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
        
        let AMM::MoeLbPair(mut pool) = pool_vec.remove(0) else {
            unreachable!()
        };
        
        // Initialize bins with active bin
        pool.bins.clear();
        
        Ok(pool)
    }

    /// Update bin reserves when DepositedToBins or WithdrawnFromBins event occurs
    pub fn update_bins(&mut self, ids: Vec<U256>, amounts: Vec<[u8; 32]>, is_deposit: bool) -> Result<(), AMMError> {
        for (id, amount_bytes) in ids.iter().zip(amounts.iter()) {
            let bin_id = id.to::<u32>();
            
            // Decode packed amounts (bytes32 contains both X and Y amounts)
            // Lower 128 bits = amountX, Upper 128 bits = amountY
            let amount_x = u128::from_le_bytes(amount_bytes[0..16].try_into().unwrap_or([0u8; 16]));
            let amount_y = u128::from_le_bytes(amount_bytes[16..32].try_into().unwrap_or([0u8; 16]));
            
            let bin = self.bins.entry(bin_id).or_insert(BinReserve::default());
            
            if is_deposit {
                bin.reserve_x = bin.reserve_x.saturating_add(amount_x);
                bin.reserve_y = bin.reserve_y.saturating_add(amount_y);
                self.reserve_x = self.reserve_x.saturating_add(amount_x);
                self.reserve_y = self.reserve_y.saturating_add(amount_y);
            } else {
                bin.reserve_x = bin.reserve_x.saturating_sub(amount_x);
                bin.reserve_y = bin.reserve_y.saturating_sub(amount_y);
                self.reserve_x = self.reserve_x.saturating_sub(amount_x);
                self.reserve_y = self.reserve_y.saturating_sub(amount_y);
                
                // Remove bin if both reserves are zero
                if bin.reserve_x == 0 && bin.reserve_y == 0 {
                    self.bins.remove(&bin_id);
                }
            }
        }
        Ok(())
    }

    /// Get price from bin ID
    /// Price = (1 + binStep / 10000) ^ (id - 2^23)
    pub fn get_price_from_id(&self, id: u32) -> f64 {
        const SCALE: i64 = 1 << 23; // 2^23 = 8388608
        let step = self.bin_step as f64 / 10000.0;
        let exponent = (id as i64 - SCALE) as f64;
        (1.0 + step).powf(exponent)
    }

    /// Simulate swap across multiple bins (more accurate than simple x*y=k)
    pub fn simulate_swap_across_bins(
        &self,
        swap_for_y: bool, // true = X->Y, false = Y->X
        mut amount_in: U256,
    ) -> Result<U256, AMMError> {
        if amount_in.is_zero() {
            return Ok(U256::ZERO);
        }
        
        // Sanity check: if total reserves are zero or suspiciously high, return zero
        if self.reserve_x == 0 || self.reserve_y == 0 {
            return Ok(U256::ZERO);
        }
        
        // Check for overflow/invalid reserves (e.g., > 10^30)
        const MAX_REASONABLE_RESERVE: u128 = 1_000_000_000_000_000_000_000_000_000_000; // 10^30
        if self.reserve_x > MAX_REASONABLE_RESERVE || self.reserve_y > MAX_REASONABLE_RESERVE {
            return Ok(U256::ZERO);
        }

        let mut amount_out = U256::ZERO;
        let mut current_id = self.active_id;
        let mut iterations = 0;
        const MAX_ITERATIONS: usize = 100; // Prevent infinite loops

        while !amount_in.is_zero() && iterations < MAX_ITERATIONS {
            iterations += 1;

            let bin = self.bins.get(&current_id);
            if let Some(bin_data) = bin {
                // Check bin reserves for sanity
                const MAX_BIN_RESERVE: u128 = 1_000_000_000_000_000_000_000_000_000_000; // 10^30
                if bin_data.reserve_x > MAX_BIN_RESERVE || bin_data.reserve_y > MAX_BIN_RESERVE {
                    // Bin has invalid reserves, skip it
                    current_id = if swap_for_y { current_id + 1 } else { current_id.saturating_sub(1) };
                    continue;
                }
                
                let (reserve_in, reserve_out) = if swap_for_y {
                    (U256::from(bin_data.reserve_x), U256::from(bin_data.reserve_y))
                } else {
                    (U256::from(bin_data.reserve_y), U256::from(bin_data.reserve_x))
                };

                if reserve_out.is_zero() {
                    // Move to next bin
                    current_id = if swap_for_y { current_id + 1 } else { current_id.saturating_sub(1) };
                    continue;
                }

                // Calculate with fees
                // bin_step is the base fee in basis points (e.g., 1 = 0.01%, 25 = 0.25%)
                let fee_bps = self.bin_step as u128;
                let protocol_fee_bps = self.protocol_share_bps as u128;
                let total_fee_bps = fee_bps.saturating_add(protocol_fee_bps);
                
                let max_amount_in = reserve_in;
                let amount_in_this_bin = amount_in.min(max_amount_in);
                
                if amount_in_this_bin.is_zero() {
                    break;
                }

                // Apply fees: amount_in_after_fee = amount_in * (10000 - total_fee_bps) / 10000
                let fee_amount = amount_in_this_bin * U256::from(total_fee_bps) / U256::from(10000);
                let amount_in_after_fee = amount_in_this_bin.saturating_sub(fee_amount);

                // Constant product formula with fees applied
                let amount_out_this_bin = if reserve_in.is_zero() {
                    U256::ZERO
                } else {
                    amount_in_after_fee * reserve_out / (reserve_in + amount_in_after_fee)
                };

                // Sanity check the output from this bin
                if amount_out_this_bin > amount_in_this_bin * U256::from(1000) {
                    // Output from this bin is suspiciously high, stop simulation
                    break;
                }
                
                amount_out += amount_out_this_bin;
                amount_in -= amount_in_this_bin;

                // Move to next bin
                if !amount_in.is_zero() {
                    current_id = if swap_for_y { 
                        current_id.checked_add(1).unwrap_or(u32::MAX)
                    } else { 
                        current_id.checked_sub(1).unwrap_or(0)
                    };
                }
            } else {
                // No bin data, move to next
                current_id = if swap_for_y { current_id + 1 } else { current_id.saturating_sub(1) };
            }

            // Safety check: prevent going too far from active bin
            if current_id.abs_diff(self.active_id) > 1000 {
                break;
            }
        }

        Ok(amount_out)
    }
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
            
            // Decode the Slot0Data struct array
            // (tokenX, tokenY, activeId, binStep, reserveX, reserveY, protocolShare, maxVolatilityAccumulator)
            let decoded = <Vec<(Address, Address, u32, u16, u128, u128, u16, u32)>>::abi_decode(&ret)?;
            
            Ok::<(&mut [AMM], Vec<(Address, Address, u32, u16, u128, u128, u16, u32)>), AMMError>(
                (chunk, decoded),
            )
        });
    }

    while let Some(res) = futures.next().await {
        let (group, data) = res?;
        for (slot, amm) in data.into_iter().zip(group.iter_mut()) {
            if let AMM::MoeLbPair(p) = amm {
                p.token_x = Token::from(slot.0);
                p.token_y = Token::from(slot.1);
                p.active_id = slot.2;
                p.bin_step = slot.3;
                p.reserve_x = slot.4;
                p.reserve_y = slot.5;
                p.protocol_share_bps = slot.6;
                p.max_volatility_acc = slot.7;
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
                    
                    for (offset, (reserve_x, reserve_y)) in bin_data.iter().enumerate() {
                        if *reserve_x > 0 || *reserve_y > 0 {
                            let bin_id = start_id + offset as u32;
                            
                            // Sanity check bin reserves
                            const MAX_BIN_RESERVE: u128 = 1_000_000_000_000_000_000_000_000_000_000; // 10^30
                            if *reserve_x > MAX_BIN_RESERVE || *reserve_y > MAX_BIN_RESERVE {
                                tracing::warn!(
                                    target: "moe.bins.sync",
                                    address = %pair.address,
                                    bin_id,
                                    reserve_x = *reserve_x,
                                    reserve_y = *reserve_y,
                                    "Skipping bin with unreasonably large reserves during sync"
                                );
                                continue;
                            }
                            
                            pair.bins.insert(
                                bin_id,
                                BinReserve {
                                    reserve_x: *reserve_x,
                                    reserve_y: *reserve_y,
                                },
                            );
                        }
                    }
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
            self.active_id = ev.id.to::<u32>();
            
            // Decode packed amounts from bytes32
            // In Moe LB, bytes32 packs two uint128 values using big-endian encoding:
            // - Bytes 0-15:  first uint128 (amountX)
            // - Bytes 16-31: second uint128 (amountY)
            let amounts_in_bytes = ev.amountsIn.as_slice();
            let amount_in_x = u128::from_be_bytes(amounts_in_bytes[0..16].try_into().unwrap_or([0u8; 16]));
            let amount_in_y = u128::from_be_bytes(amounts_in_bytes[16..32].try_into().unwrap_or([0u8; 16]));
            
            let amounts_out_bytes = ev.amountsOut.as_slice();
            let amount_out_x = u128::from_be_bytes(amounts_out_bytes[0..16].try_into().unwrap_or([0u8; 16]));
            let amount_out_y = u128::from_be_bytes(amounts_out_bytes[16..32].try_into().unwrap_or([0u8; 16]));
            
            // Update total reserves based on swap
            self.reserve_x = self.reserve_x.saturating_add(amount_in_x).saturating_sub(amount_out_x);
            self.reserve_y = self.reserve_y.saturating_add(amount_in_y).saturating_sub(amount_out_y);
            
            // Sanity check: if reserves become unreasonably large, log warning
            const MAX_RESERVE: u128 = 1_000_000_000_000_000_000_000_000_000_000; // 10^30
            if self.reserve_x > MAX_RESERVE || self.reserve_y > MAX_RESERVE {
                tracing::warn!(
                    target: "moe.sync",
                    address = %self.address,
                    reserve_x = self.reserve_x,
                    reserve_y = self.reserve_y,
                    "Pool reserves became unreasonably large after swap event - possible data corruption"
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
        if amount_in.is_zero() {
            return Ok(U256::ZERO);
        }
        
        // If we have bin data, use accurate bin-based simulation
        if !self.bins.is_empty() {
            let swap_for_y = base_token == self.token_x.address;
            return self.simulate_swap_across_bins(swap_for_y, amount_in);
        }
        
        // Fallback to simple constant product formula if no bin data available
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
        
        // Apply fees in fallback mode (same as bins-based calculation)
        let fee_bps = self.bin_step as u128;
        let protocol_fee_bps = self.protocol_share_bps as u128;
        let total_fee_bps = fee_bps.saturating_add(protocol_fee_bps);
        
        // Deduct fees from input amount
        let fee_amount = amount_in * U256::from(total_fee_bps) / U256::from(10000);
        let amount_in_after_fee = amount_in.saturating_sub(fee_amount);
        
        // Constant product formula with fees applied
        let numerator = amount_in_after_fee * reserve_out;
        let denominator = reserve_in + amount_in_after_fee;
        let amount_out = numerator / denominator;
        
        // Sanity check: output should not exceed reserves or be suspiciously high
        if amount_out > reserve_out || amount_out > amount_in * U256::from(1000) {
            return Ok(U256::ZERO);
        }
        
        Ok(amount_out)
    }

    fn simulate_swap_mut(
        &mut self,
        base_token: Address,
        _quote_token: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        let amount_out = self.simulate_swap(base_token, _quote_token, amount_in)?;
        
        // Update total reserves
        if base_token == self.token_x.address {
            self.reserve_x = self.reserve_x.saturating_add(amount_in.to::<u128>());
            self.reserve_y = self.reserve_y.saturating_sub(amount_out.to::<u128>());
        } else {
            self.reserve_y = self.reserve_y.saturating_add(amount_in.to::<u128>());
            self.reserve_x = self.reserve_x.saturating_sub(amount_out.to::<u128>());
        }
        
        // TODO: Update individual bin reserves if needed for more accurate multi-hop simulations
        // For now, we just update the total reserves which is sufficient for most use cases
        
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
        let to_block_num = match to_block {
            BlockId::Number(num) => {
                match num {
                    alloy::eips::BlockNumberOrTag::Number(n) => n,
                    _ => provider.get_block_number().await?,
                }
            }
            _ => provider.get_block_number().await?,
        };
        
        let filter = Filter::new()
            .event_signature(FilterSet::from(vec![self.pool_creation_event()]))
            .address(vec![self.address()])
            .from_block(self.creation_block)
            .to_block(to_block_num);
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

use std::future::Future;

impl AutomatedMarketMakerFactory for MoeFactory {
    type PoolVariant = MoeLbPair;
    fn address(&self) -> Address { self.address }
    fn create_pool(&self, log: Log) -> Result<AMM, AMMError> {
        let ev = IMoeFactory::LBPairCreated::decode_log(&log.inner)?;
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
        let price_at_active = pair.get_price_from_id(8388608);
        assert!((price_at_active - 1.0).abs() < 1e-9, "Price at active_id should be ~1.0");
        
        // One bin above active_id
        let price_above = pair.get_price_from_id(8388609);
        let expected_above = 1.002; // (1 + 0.002)^1
        assert!(
            (price_above - expected_above).abs() < 1e-6,
            "Price one bin above should be ~1.002, got {}",
            price_above
        );
        
        // One bin below active_id
        let price_below = pair.get_price_from_id(8388607);
        let expected_below = 0.998; // (1 + 0.002)^(-1) ≈ 0.998
        assert!(
            (price_below - expected_below).abs() < 1e-6,
            "Price one bin below should be ~0.998, got {}",
            price_below
        );
    }

    #[test]
    fn test_update_bins_deposit() {
        let mut pair = create_mock_pair();
        
        // Create deposit data
        let bin_id = 8388608u32;
        let ids = vec![U256::from(bin_id)];
        
        // Create packed amounts: lower 128 bits = X, upper 128 bits = Y
        let amount_x = 1_000_000u128;
        let amount_y = 2_000_000u128;
        let mut packed = [0u8; 32];
        packed[0..16].copy_from_slice(&amount_x.to_le_bytes());
        packed[16..32].copy_from_slice(&amount_y.to_le_bytes());
        let amounts = vec![packed];
        
        let initial_reserve_x = pair.reserve_x;
        let initial_reserve_y = pair.reserve_y;
        
        // Execute deposit
        pair.update_bins(ids.clone(), amounts.clone(), true).unwrap();
        
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
        packed[0..16].copy_from_slice(&amount_x.to_le_bytes());
        packed[16..32].copy_from_slice(&amount_y.to_le_bytes());
        
        let ids = vec![U256::from(bin_id)];
        let amounts = vec![packed];
        
        pair.update_bins(ids.clone(), amounts.clone(), true).unwrap();
        
        // Now withdraw half
        let withdraw_x = 1_000_000u128;
        let withdraw_y = 1_500_000u128;
        let mut withdraw_packed = [0u8; 32];
        withdraw_packed[0..16].copy_from_slice(&withdraw_x.to_le_bytes());
        withdraw_packed[16..32].copy_from_slice(&withdraw_y.to_le_bytes());
        let withdraw_amounts = vec![withdraw_packed];
        
        let reserve_x_before = pair.reserve_x;
        let reserve_y_before = pair.reserve_y;
        
        pair.update_bins(ids.clone(), withdraw_amounts, false).unwrap();
        
        // Verify bin reserves decreased
        let bin = pair.bins.get(&bin_id).unwrap();
        assert_eq!(bin.reserve_x, amount_x - withdraw_x);
        assert_eq!(bin.reserve_y, amount_y - withdraw_y);
        
        // Verify total reserves decreased
        assert_eq!(pair.reserve_x, reserve_x_before - withdraw_x);
        assert_eq!(pair.reserve_y, reserve_y_before - withdraw_y);
    }

    #[test]
    fn test_update_bins_full_withdraw_removes_bin() {
        let mut pair = create_mock_pair();
        let bin_id = 8388608u32;
        
        // Deposit liquidity
        let amount_x = 1_000_000u128;
        let amount_y = 2_000_000u128;
        let mut packed = [0u8; 32];
        packed[0..16].copy_from_slice(&amount_x.to_le_bytes());
        packed[16..32].copy_from_slice(&amount_y.to_le_bytes());
        
        let ids = vec![U256::from(bin_id)];
        let amounts = vec![packed];
        
        pair.update_bins(ids.clone(), amounts.clone(), true).unwrap();
        assert!(pair.bins.contains_key(&bin_id));
        
        // Withdraw all
        pair.update_bins(ids, amounts, false).unwrap();
        
        // Bin should be removed
        assert!(!pair.bins.contains_key(&bin_id));
    }

    #[test]
    fn test_simulate_swap_simple() {
        let mut pair = create_mock_pair();
        
        // Add some bins with liquidity
        for i in 0..5 {
            let bin_id = pair.active_id + i;
            pair.bins.insert(
                bin_id,
                BinReserve {
                    reserve_x: 1_000_000_000,
                    reserve_y: 1_000_000,
                },
            );
        }
        
        // Simulate swap X -> Y
        let amount_in = U256::from(100_000_000u128);
        let amount_out = pair
            .simulate_swap(pair.token_x.address, pair.token_y.address, amount_in)
            .unwrap();
        
        assert!(amount_out > U256::ZERO, "Should receive some output");
        assert!(amount_out < U256::from(100_000u128), "Output should be reasonable");
    }

    #[test]
    fn test_simulate_swap_across_bins() {
        let pair = create_mock_pair();
        
        // Manually build bin structure
        let mut test_pair = pair.clone();
        
        // Add liquidity to active bin and adjacent bins
        test_pair.bins.insert(
            test_pair.active_id,
            BinReserve {
                reserve_x: 10_000_000_000,
                reserve_y: 10_000_000,
            },
        );
        test_pair.bins.insert(
            test_pair.active_id + 1,
            BinReserve {
                reserve_x: 5_000_000_000,
                reserve_y: 5_000_000,
            },
        );
        
        // Swap for Y (X -> Y)
        let amount_in = U256::from(1_000_000_000u128);
        let amount_out = test_pair.simulate_swap_across_bins(true, amount_in).unwrap();
        
        assert!(amount_out > U256::ZERO);
    }

    #[test]
    fn test_simulate_swap_mut() {
        let mut pair = create_mock_pair();
        
        // Add bins
        pair.bins.insert(
            pair.active_id,
            BinReserve {
                reserve_x: 10_000_000_000,
                reserve_y: 10_000_000,
            },
        );
        
        let initial_reserve_x = pair.reserve_x;
        let initial_reserve_y = pair.reserve_y;
        
        // Simulate swap X -> Y
        let amount_in = U256::from(100_000_000u128);
        let amount_out = pair
            .simulate_swap_mut(pair.token_x.address, pair.token_y.address, amount_in)
            .unwrap();
        
        // Reserves should have changed
        assert!(pair.reserve_x > initial_reserve_x);
        assert!(pair.reserve_y < initial_reserve_y);
        assert!(amount_out > U256::ZERO);
    }

    #[test]
    fn test_calculate_price() {
        let pair = create_mock_pair();
        
        // Calculate price of X in terms of Y
        let price_x_in_y = pair.calculate_price(pair.token_x.address, pair.token_y.address).unwrap();
        
        // With 10^18 X and 10^6 Y (and decimals 18 vs 6), price should be around 1.0
        assert!(price_x_in_y > 0.0);
        
        // Calculate price of Y in terms of X
        let price_y_in_x = pair.calculate_price(pair.token_y.address, pair.token_x.address).unwrap();
        
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
        let factory_address = address!("0x5bEf015CA9424A7C07B68490616a4C1F094BEdEc");
        let creation_block = 12345678u64;
        
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
            packed[0..16].copy_from_slice(&amount_x.to_le_bytes());
            packed[16..32].copy_from_slice(&amount_y.to_le_bytes());
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
        let pair = create_mock_pair();
        
        let amount_out = pair
            .simulate_swap(pair.token_x.address, pair.token_y.address, U256::ZERO)
            .unwrap();
        
        assert_eq!(amount_out, U256::ZERO);
    }

    #[test]
    fn test_swap_with_no_bins_fallback() {
        let mut pair = create_mock_pair();
        
        // Ensure bins are empty, but reserves are set
        pair.bins.clear();
        pair.reserve_x = 1_000_000_000_000_000_000;
        pair.reserve_y = 1_000_000;
        
        // Should fallback to simple constant product formula
        let amount_in = U256::from(100_000_000_000_000_000u128); // 0.1 token
        let amount_out = pair
            .simulate_swap(pair.token_x.address, pair.token_y.address, amount_in)
            .unwrap();
        
        assert!(amount_out > U256::ZERO, "Fallback swap should work");
    }

    #[test]
    fn test_bin_step_variations() {
        let mut pair = create_mock_pair();
        
        // Test with different bin steps
        let test_steps = vec![1, 10, 20, 50, 100]; // Various bin steps
        
        for step in test_steps {
            pair.bin_step = step;
            
            let price_at_active = pair.get_price_from_id(8388608);
            assert!((price_at_active - 1.0).abs() < 1e-9);
            
            let price_above = pair.get_price_from_id(8388609);
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
        assert_eq!(events[2], IMoeLBPairEvents::WithdrawnFromBins::SIGNATURE_HASH);
    }
}
