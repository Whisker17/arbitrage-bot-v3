//! Shared WHI-519 intent-state-machine helpers for active monitor services.
//!
//! Production broadcast remains fail-closed (WHI-526). With the gate closed,
//! services still route candidates through a process-lifetime SM singleton and
//! exercise reserve → begin_prepare → abort/release using real candidate
//! SnapshotIds. Local signing is covered by unit tests / measured Executor
//! paths; services do not invent fake Measured Executor state here.

use alloy::primitives::aliases::U112;
use alloy::primitives::{Address, B256, U256};
use amms::amms::amm::{AutomatedMarketMaker, AMM};
use amms::execution::{
    BlockFeeContextCache, CandidateRef, ChainNonceView, ExecutionContext, ExecutionContextView,
    ExecutionIdentity, ExecutionIdentityLease, ExecutionIdentitySource, ExecutionParams, Executor,
    ExecutorConfig, FeePolicy, FinalRequestParams, HeadOutcome, IdentityError, IntentPolicy,
    IntentStateMachine, LatestWinsSlot, NoopPreflight, ProtocolKind, RouteKey, RuntimeGasProfile,
    RuntimeProfileConfig, VerifiedCrossingBuckets,
};
use amms::state_space::{BlockHeaderContext, SnapshotId, SnapshotStatus};
#[cfg(test)]
use amms::state_space::{MarketSnapshot, ProtocolCoverage};
use eyre::{eyre, Result};
#[cfg(test)]
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

/// Reject queued work unless the live tip is still Ready at the candidate SnapshotId.
pub fn require_matching_ready_tip(
    tip: Option<SnapshotStatus>,
    candidate_id: SnapshotId,
) -> Result<SnapshotStatus> {
    match tip {
        Some(SnapshotStatus::Ready(snapshot)) if snapshot.id == candidate_id => {
            Ok(SnapshotStatus::Ready(snapshot))
        }
        Some(SnapshotStatus::Ready(snapshot)) => Err(eyre!(
            "stale queued opportunity: candidate {:?} != live tip {:?}",
            candidate_id,
            snapshot.id
        )),
        Some(_) => Err(eyre!("execution gate has no live Ready snapshot tip")),
        None => Err(eyre!("execution gate has no live Ready snapshot tip")),
    }
}

pub fn production_send_allowed() -> bool {
    // WHI-519 keeps production sends disabled; WHI-526 owns enablement.
    false
}

pub fn intent_policy_from_env_or_defaults() -> Result<IntentPolicy> {
    match IntentPolicy::from_env() {
        Ok(policy) => Ok(policy),
        Err(_) => {
            // Local/dev defaults for dry-run services when caps are unset.
            // Live operators must set MAX_FEE_CAP_WEI / CANCEL_FEE_CAP_WEI.
            Ok(IntentPolicy::with_caps(
                200_000_000_000, // 200 gwei
                400_000_000_000, // 400 gwei
            ))
        }
    }
}

/// Process-lifetime SM singleton for one service process / signer account.
pub fn process_intent_sm(signer: Address) -> Result<Arc<IntentStateMachine>> {
    static SM: OnceLock<Result<Arc<IntentStateMachine>, String>> = OnceLock::new();
    // OnceLock stores Result so construction errors surface once.
    // Note: signer is fixed to the first caller in this process (services use one account).
    let cell = SM.get_or_init(|| {
        let policy = intent_policy_from_env_or_defaults().map_err(|e| e.to_string())?;
        IntentStateMachine::new(
            signer,
            ChainNonceView {
                latest_nonce: 0,
                pending_nonce: 0,
            },
            policy,
            production_send_allowed(),
        )
        .map(Arc::new)
        .map_err(|e| e.to_string())
    });
    match cell {
        Ok(sm) => {
            if sm.signer_address() != signer {
                return Err(eyre!(
                    "intent SM singleton already bound to signer {}, refused {}",
                    sm.signer_address(),
                    signer
                ));
            }
            Ok(Arc::clone(sm))
        }
        Err(e) => Err(eyre!("intent SM init failed: {e}")),
    }
}

/// Back-compat alias used by older call sites.
pub fn build_intent_sm(signer: Address) -> Result<Arc<IntentStateMachine>> {
    process_intent_sm(signer)
}

pub fn candidate_ref(
    snapshot_id: SnapshotId,
    header: BlockHeaderContext,
    pool_universe_fingerprint: B256,
    route_key: RouteKey,
    amount_in: U256,
) -> Result<CandidateRef> {
    Ok(CandidateRef {
        snapshot_id,
        header,
        pool_universe_fingerprint,
        route_key,
        amount_in,
    })
}

/// Ready tip matching the candidate's full SnapshotId (service dry-run gate).
#[cfg(test)]
pub fn ready_status_for_candidate(candidate: &CandidateRef) -> SnapshotStatus {
    let mut coverage = ProtocolCoverage::default();
    coverage.pool_universe_fingerprint = Some(candidate.pool_universe_fingerprint);
    SnapshotStatus::Ready(Arc::new(MarketSnapshot::new(
        candidate.snapshot_id,
        candidate.header,
        HashMap::new(),
        coverage,
    )))
}

/// Fee context bound to the candidate block identity for permit minting.
pub fn fee_context_for_candidate(
    candidate: &CandidateRef,
    base_fee_per_gas: u128,
    block_gas_limit: u64,
) -> amms::execution::BlockFeeContext {
    amms::execution::BlockFeeContext {
        block_number: candidate.snapshot_id.block_number,
        block_hash: candidate.snapshot_id.block_hash,
        base_fee_per_gas,
        block_gas_limit,
    }
}

/// Non-blocking pre-broadcast exercise of the process SM while production send is gated.
///
/// Uses the candidate's real SnapshotId and a Ready gate. Does not sign/broadcast.
pub fn exercise_sm_prebroadcast(
    sm: &IntentStateMachine,
    candidate: CandidateRef,
    status: &SnapshotStatus,
) -> Result<()> {
    sm.observe_snapshot(status)?;
    let fee_ctx = fee_context_for_candidate(&candidate, 50_000_000_000, 60_000_000);
    let (nonce, _permit) = sm.reserve(candidate, status, fee_ctx)?;
    sm.begin_prepare(nonce)?;
    // Gate closed: never sign/broadcast. Abort prepare and release trailing reserved.
    sm.abort_prepare(nonce)?;
    let _ = sm.reconcile(ChainNonceView {
        latest_nonce: 0,
        pending_nonce: 0,
    })?;
    Ok(())
}

/// Shared helper: route a resized candidate through the process SM prebroadcast path.
pub fn route_candidate_through_sm(
    signer: Address,
    status: &SnapshotStatus,
    header: BlockHeaderContext,
    pool_universe_fingerprint: B256,
    route_key: RouteKey,
    amount_in: U256,
) -> Result<()> {
    let SnapshotStatus::Ready(snapshot) = status else {
        return Err(eyre!("execution gate requires a Ready market snapshot"));
    };
    if snapshot.header != header {
        return Err(eyre!(
            "candidate header does not match the Ready market snapshot"
        ));
    }
    if snapshot.coverage.pool_universe_fingerprint != Some(pool_universe_fingerprint) {
        return Err(eyre!(
            "candidate pool-universe fingerprint does not match the Ready market snapshot"
        ));
    }

    let candidate = candidate_ref(
        snapshot.id,
        header,
        pool_universe_fingerprint,
        route_key,
        amount_in,
    )?;
    let sm = process_intent_sm(signer)?;
    exercise_sm_prebroadcast(&sm, candidate, status)
}

/// Minimal stand-in [`ExecutionIdentitySource`] for the four monitor services (WHI-553).
///
/// Validates structurally against the caller-supplied [`SnapshotStatus`] for one
/// candidate, mirroring `LiveExecutionIdentitySource::validate`'s checks, without a live
/// `SnapshotPublisher` — these services do not run one yet. Real production wiring
/// (a `SnapshotPublisher`-backed `LiveExecutionIdentitySource` and per-block-synchronized
/// gas-profile refresh) is tracked as deferred follow-up; see docs/DEFERRED_ISSUES.md.
pub struct StatusBoundIdentitySource {
    status: SnapshotStatus,
}

impl StatusBoundIdentitySource {
    pub fn new(status: SnapshotStatus) -> Self {
        Self { status }
    }
}

impl ExecutionIdentitySource for StatusBoundIdentitySource {
    async fn validate(&self, identity: &ExecutionIdentity) -> Result<(), IdentityError> {
        let SnapshotStatus::Ready(snapshot) = &self.status else {
            return Err(IdentityError::PublisherNotReady);
        };
        if snapshot.id != identity.snapshot_id || snapshot.header != identity.header {
            return Err(IdentityError::StaleHeader);
        }
        if snapshot.coverage.pool_universe_fingerprint != Some(identity.pool_universe_fingerprint)
        {
            return Err(IdentityError::StaleTopology);
        }
        Ok(())
    }

    async fn acquire_send_lease(
        &self,
        _identity: &ExecutionIdentity,
    ) -> Result<ExecutionIdentityLease, IdentityError> {
        unreachable!(
            "StatusBoundIdentitySource never acquires a send lease: run_pipeline_head_closed \
             stops before pause/lease/sign/broadcast"
        )
    }
}

/// Loads the canonical Mantle-mainnet gas-profile artifact and builds a real [`Executor`]
/// from it (WHI-553), so all four monitor services can exercise `run_pipeline_head_closed`
/// with a real request builder while the production send gate stays closed
/// (`production_send_allowed() == false`).
pub async fn build_execution_runtime<P: alloy::providers::Provider + Clone + 'static>(
    provider: P,
    executor_contract: Address,
    wmnt_address: Address,
    executor_config: ExecutorConfig,
) -> Result<Executor> {
    let profile_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("config/gas_profiles/mantle_mainnet_v1.json");
    let gas_profile = RuntimeGasProfile::load(
        &profile_path,
        RuntimeProfileConfig::mantle_mainnet(Vec::new()),
    )
    .map_err(|e| eyre!("failed to load gas profile runtime artifact: {e}"))?;
    let block_fee_contexts = Arc::new(BlockFeeContextCache::default());
    let context = ExecutionContext::from_provider(
        provider,
        executor_contract,
        wmnt_address,
        gas_profile,
        block_fee_contexts,
    )
    .await?;
    Ok(Executor::new(context, executor_config))
}

/// Derives an attestation-only [`VerifiedCrossingBuckets`] directly from a route key's
/// own already-measured crossing fields (populated by each protocol's
/// `simulate_swap_with_crossing_evidence`), so the fold `ExecutionParams::new` performs
/// internally is idempotent and the candidate's route_key stays byte-identical to the
/// one used to mint the permit. Returns `None` for a V2-only route.
pub fn verified_crossing_buckets_from_route(route_key: &RouteKey) -> Option<VerifiedCrossingBuckets> {
    let has_v3_or_moe = route_key
        .protocols
        .iter()
        .any(|p| matches!(p, ProtocolKind::V3 | ProtocolKind::Moe));
    if !has_v3_or_moe {
        return None;
    }
    Some(VerifiedCrossingBuckets::new(
        route_key.v3_tick_crossings,
        route_key.moe_bin_crossings,
    ))
}

/// Derives the on-chain-registration fields [`ExecutionParams`] needs
/// (`pool_types`/`pool_tokens`/`expected_reserves_u112`) from each service's already
/// block-synced local `AMM` state, rather than issuing fresh on-chain reads (WHI-553).
///
/// `crate::execution::params::ParamsBuilder::build` is the crate-private production
/// equivalent (async, live `detect_pool_meta`/`getReserves` reads) and is unreachable
/// from `examples/` code. Since the production send gate stays closed here, sourcing
/// these fields from local state (which may lag on-chain by up to one block) instead of
/// a fresh read is an accepted, documented difference — see docs/DEFERRED_ISSUES.md.
/// Byte codes mirror `pool_type_byte`: V2=0, V3/Agni=1, MoeLB=2. V3/Agni/Moe hops carry
/// `U112::ZERO` placeholder reserves, matching the production builder.
pub fn execution_params_inputs_from_pools(
    pools: &[AMM],
    token_path: Vec<Address>,
    step_amounts_out: Vec<U256>,
    min_amount_out: U256,
    expected_net_profit_mnt_wei: U256,
) -> Result<ExecutionParamsInputs> {
    let mut pool_addresses = Vec::with_capacity(pools.len());
    let mut pool_types = Vec::with_capacity(pools.len());
    let mut pool_tokens = Vec::with_capacity(pools.len());
    let mut expected_reserves_u112 = Vec::with_capacity(pools.len() * 2);

    for pool in pools {
        pool_addresses.push(pool.address());
        let (pool_type, token0, token1, reserve0, reserve1) = match pool {
            AMM::UniswapV2Pool(p) => (
                0u8,
                p.token_a.address,
                p.token_b.address,
                U112::try_from(p.reserve_0)
                    .map_err(|e| eyre!("pool {} reserve_0 exceeds uint112: {e}", p.address))?,
                U112::try_from(p.reserve_1)
                    .map_err(|e| eyre!("pool {} reserve_1 exceeds uint112: {e}", p.address))?,
            ),
            AMM::UniswapV3Pool(p) => {
                (1u8, p.token_a.address, p.token_b.address, U112::ZERO, U112::ZERO)
            }
            AMM::AgniPool(p) => {
                (1u8, p.token_a.address, p.token_b.address, U112::ZERO, U112::ZERO)
            }
            AMM::MoeLbPair(p) => {
                (2u8, p.token_x.address, p.token_y.address, U112::ZERO, U112::ZERO)
            }
        };
        pool_types.push(pool_type);
        pool_tokens.push((token0, token1));
        expected_reserves_u112.push(reserve0);
        expected_reserves_u112.push(reserve1);
    }

    Ok(ExecutionParamsInputs {
        token_path,
        pool_addresses,
        pool_types,
        pool_tokens,
        expected_reserves_u112,
        step_amounts_out,
        min_amount_out,
        expected_net_profit_mnt_wei,
    })
}

/// Protocol-specific inputs for [`ExecutionParams::new`], bundled to keep
/// [`run_candidate_through_pipeline_head`]'s signature manageable.
pub struct ExecutionParamsInputs {
    pub token_path: Vec<Address>,
    pub pool_addresses: Vec<Address>,
    pub pool_types: Vec<u8>,
    pub pool_tokens: Vec<(Address, Address)>,
    pub expected_reserves_u112: Vec<U112>,
    pub step_amounts_out: Vec<U256>,
    pub min_amount_out: U256,
    pub expected_net_profit_mnt_wei: U256,
}

/// Shared WHI-553 helper: build a real `ExecutionParams`/`FinalRequestParams` for one
/// candidate and run it through `run_pipeline_head_closed`. Wallet-free — never signs or
/// broadcasts. Centralizes all four services' request-building so no service keeps its
/// own copy.
#[allow(clippy::too_many_arguments)]
pub async fn run_candidate_through_pipeline_head(
    signer: Address,
    executor: &Executor,
    status: &SnapshotStatus,
    header: BlockHeaderContext,
    pool_universe_fingerprint: B256,
    route_key: RouteKey,
    amount_in: U256,
    inputs: ExecutionParamsInputs,
    deadline_secs: u64,
    base_fee_per_gas: u128,
    block_gas_limit: u64,
) -> Result<HeadOutcome> {
    let SnapshotStatus::Ready(snapshot) = status else {
        return Err(eyre!("execution gate requires a Ready market snapshot"));
    };
    if snapshot.header != header {
        return Err(eyre!(
            "candidate header does not match the Ready market snapshot"
        ));
    }
    if snapshot.coverage.pool_universe_fingerprint != Some(pool_universe_fingerprint) {
        return Err(eyre!(
            "candidate pool-universe fingerprint does not match the Ready market snapshot"
        ));
    }

    let crossing_buckets = verified_crossing_buckets_from_route(&route_key);
    let candidate = candidate_ref(
        snapshot.id,
        header,
        pool_universe_fingerprint,
        route_key.clone(),
        amount_in,
    )?;

    let params = ExecutionParams::new(
        amount_in,
        route_key,
        inputs.token_path,
        inputs.pool_addresses,
        inputs.pool_types,
        inputs.pool_tokens,
        inputs.expected_reserves_u112,
        inputs.step_amounts_out,
        inputs.min_amount_out,
        inputs.expected_net_profit_mnt_wei,
        crossing_buckets,
    )
    .map_err(|e| eyre!("failed to build execution params: {e}"))?;

    let fee_ctx = fee_context_for_candidate(&candidate, base_fee_per_gas, block_gas_limit);
    executor
        .context
        .block_fee_contexts()
        .publish(fee_ctx.clone())
        .map_err(|e| eyre!("failed to publish block fee context: {e}"))?;

    let quote = executor
        .context
        .gas_profile()
        .quote(&params.route_key)
        .map_err(|e| {
            eyre!(
                "gas profile quote failed for route {}: {e}",
                params.route_key.key_string()
            )
        })?;

    let fee_plan = FeePolicy::new(
        executor.config.default_priority_fee_wei,
        executor.config.block_gas_limit_reserve,
    )
    .build(&quote, &fee_ctx)
    .map_err(|e| eyre!("failed to build fee plan: {e}"))?;

    let deadline = finite_deadline(&header, deadline_secs)?;
    let final_request_params = FinalRequestParams {
        params,
        candidate: candidate.clone(),
        fee_plan,
        deadline,
    };

    let sm = process_intent_sm(signer)?;
    let identity_source = StatusBoundIdentitySource::new(status.clone());
    let chain = ChainNonceView {
        latest_nonce: 0,
        pending_nonce: 0,
    };

    amms::execution::run_pipeline_head_closed(
        &sm,
        candidate,
        status,
        fee_ctx,
        executor,
        &identity_source,
        &NoopPreflight,
        final_request_params,
        chain,
    )
    .await
    .map_err(|e| eyre!("pipeline head failed: {e}"))
}

/// Mirrors `crate::execution::params::ParamsBuilder::build`'s private `min_amount_out`
/// derivation (`src/execution/params.rs`), which stays crate-private and unreachable
/// from `examples/`. Not reused via any shared code path — kept in sync by hand.
pub fn min_amount_out_from_plan(
    amount_in: U256,
    simulated_output: U256,
    config: &ExecutorConfig,
) -> U256 {
    let expected_profit = simulated_output.checked_sub(amount_in).unwrap_or(U256::ZERO);
    let slippage_allowance = mul_fraction(expected_profit, config.slippage_tolerance);
    let mut min_amount_out = simulated_output.saturating_sub(slippage_allowance);
    if (config.include_gas_cost_in_min_out || config.enforce_non_loss)
        && min_amount_out < amount_in
    {
        min_amount_out = amount_in;
    }
    min_amount_out
}

fn mul_fraction(value: U256, fraction: f64) -> U256 {
    if fraction <= 0.0 {
        return U256::ZERO;
    }
    let scale = 1_000_000u128;
    let fraction_scaled = ((fraction * scale as f64) as u128).min(scale);
    value * U256::from(fraction_scaled) / U256::from(scale)
}

pub type JobSlot<T> = Arc<LatestWinsSlot<T>>;

pub fn new_job_slot<T>() -> JobSlot<T> {
    Arc::new(LatestWinsSlot::new())
}

pub fn finite_deadline(header: &BlockHeaderContext, deadline_secs: u64) -> Result<U256> {
    amms::execution::deadline_from_header_timestamp(header.block_timestamp, deadline_secs)
        .map_err(|_| eyre!("deadline overflow"))
}

/// Shared WMNT balance read at a hash-pinned snapshot (WHI-524).
pub async fn executor_balance_at_snapshot<P: alloy::providers::Provider + Clone>(
    provider: &P,
    wmnt: Address,
    executor: Address,
    snapshot_id: SnapshotId,
) -> Result<amms::state_space::SnapshotBoundBalance> {
    use amms::execution::IERC20;
    use amms::state_space::{hash_pinned_state_block_id, SnapshotBoundBalance};
    let wmnt_contract = IERC20::new(wmnt, provider.clone());
    let amount = wmnt_contract
        .balanceOf(executor)
        .call()
        .block(hash_pinned_state_block_id(snapshot_id.block_hash))
        .await?;
    Ok(SnapshotBoundBalance::new(snapshot_id, amount))
}

/// Three-way per-tx cap: `min(balance, configured_quote_max, max_input_per_tx)`.
pub fn capped_max_input_for_snapshot(
    snapshot_id: SnapshotId,
    balance: amms::state_space::SnapshotBoundBalance,
    configured_quote_max: U256,
    max_input_per_tx_wmnt_wei: U256,
) -> Result<U256> {
    let configured = configured_quote_max.min(max_input_per_tx_wmnt_wei);
    amms::state_space::max_input_bound_for_snapshot(snapshot_id, balance, configured)
        .map_err(|e| eyre!("{e}"))
}

/// Inventory over-cap check. Returns Err when balance exceeds the mandatory cap.
pub fn check_inventory_cap(
    balance: U256,
    max_total_inventory_wmnt_wei: U256,
) -> Result<()> {
    if balance > max_total_inventory_wmnt_wei {
        return Err(eyre!(
            "executor inventory {balance} exceeds MAX_TOTAL_INVENTORY_WMNT_WEI {max_total_inventory_wmnt_wei}"
        ));
    }
    Ok(())
}
