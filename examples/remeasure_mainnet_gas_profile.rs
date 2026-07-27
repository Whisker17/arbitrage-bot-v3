//! WHI-557: measure real gas cost for the 7 base 2-hop, zero-crossing route
//! classes against pinned Mantle mainnet state via `eth_estimateGas` +
//! `stateOverride` (never a real broadcast tx), append the results as
//! `SampleSource::ForkReplay` samples, and regenerate the pinned gas-profile
//! artifact from the merged sample set.
//!
//! The executor is not deployed anywhere on mainnet today, so a fresh
//! instance is synthesized at a throwaway address on every run via a `code`
//! override (the source-derived, WMNT-patched runtime bytecode from
//! [`amms::execution::runtime_identity`]), an `admin` slot override so the
//! synthetic caller passes `onlyHotExecutor`, a WMNT balance override so the
//! executor holds `amount_in` before the call, and `registeredPools` slot
//! overrides for every pool used in the route — bypassing the admin-only
//! `registerPool`/venue-verification path entirely, which is fine since this
//! harness never touches `venues[poolType]`.
//!
//! A same-venue round trip through a single real pool always loses a bit to compounding
//! AMM fees, so `executeArbitrage`'s `balanceAfter >= balanceBefore + minProfit` check
//! would revert every time at `minProfit = 0`. Each class nudges its second hop's pool
//! state by a small, explicitly documented amount (see `notes` on each [`GasSample`]) so
//! the round trip clears the profit invariant by a hair; the real pool bytecode, fee
//! math, and opcode/storage-access pattern execute identically either way, so gas cost
//! fidelity is unaffected.
//!
//! Usage:
//! ```text
//! MANTLE_FORK_RPC_URL=<read-only-rpc> cargo run --example remeasure_mainnet_gas_profile -- \
//!   --chain-id 5000 --identity config/executor_identity.json --out config/gas_profiles/pinned/
//! ```

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;

use alloy::eips::BlockId;
use alloy::network::primitives::{BlockResponse, HeaderResponse};
use alloy::primitives::{address, Address, Bytes, B256, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::client::ClientBuilder;
use alloy::transports::layers::{RetryBackoffLayer, ThrottleLayer};
use clap::Parser;
use eyre::{bail, Context, Result};

use amms::amms::amm::AMM;
use amms::amms::agni::AgniPool;
use amms::amms::error::AMMError;
use amms::amms::moe::{
    sync_moe_snapshots_batch, MoeError, MoeLbPair, MoeSnapshotContext, MoeSnapshotSyncConfig,
};
use amms::execution::contract::IAgniPool;
use amms::execution::contract::IMoeLBPair;
use amms::execution::gas_profile::{
    generate_artifact, load_generator_config, load_samples_jsonl, write_artifact, GasSample,
    ProfileStatus, ProtocolKind, RouteKey, SampleOutcome, SampleSource, VenueRef,
};
use amms::execution::mainnet_fork_harness::{
    self, admin_override, build_state_override, erc20_balance_override, moe_lb_bin_reserve_word,
    moe_lb_bin_slot, moe_lb_parameters_word_with_active_id, registered_pool_slots,
    v2_conservative_amount_out, v2_final_hop_settlement, v2_generous_amount_out,
    v3_favorable_sqrt_price, v3_liquidity_override, v3_slot0_nudge, AccountStateOverride,
    MOE_LB_PARAMETERS_SLOT, REGISTERED_POOLS_BASE_SLOT, V3_SLOT0_SLOT, WMNT_BALANCE_SLOT,
};
use amms::execution::runtime_identity::{
    build_export, resolve_immutable_plan, BuildEvidence, ExecutorIdentityExport, ImmutableInputs,
};

const WMNT: Address = address!("78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8");
const USDT: Address = address!("201EBa5CC46D216Ce6DC03F6a759e8E766e956aE");

/// USDT's balance mapping storage slot, confirmed via a `cast storage` sweep against
/// the FusionX pool's real USDT balance at the pinned block (same standard OZ ERC20
/// layout as [`WMNT_BALANCE_SLOT`], independently verified rather than assumed).
const USDT_BALANCE_SLOT: u64 = 0;

const FUSIONX_V2_POOL: Address = address!("3e5922cd0cec71dc2d60ec8b36aa4c05b7c1672f");
const AGNI_PRIMARY_POOL: Address = address!("D08C50F7E69e9aeb2867DefF4A8053d9A855e26A");
const AGNI_SECONDARY_POOL: Address = address!("D145db1DfC3fcd2e999B47F3A02c85bd7750ed09");
const MOE_PRIMARY_POOL: Address = address!("f6C9020c9E915808481757779EDB53DACEaE2415");
const MOE_SECONDARY_POOL: Address = address!("365722f12ceb2063286A268B03c654Df81B7C00F");

const POOL_TYPE_V2: u8 = 0;
const POOL_TYPE_V3: u8 = 1;
const POOL_TYPE_MOE_LB: u8 = 2;

/// A throwaway address to synthesize the executor at — never deployed on
/// real mainnet, only ever touched via `stateOverride` in this harness.
const SYNTHETIC_EXECUTOR: Address = address!("000000000000000000000000000000000000EE01");
/// A throwaway caller — clears `onlyHotExecutor` purely via the `admin` slot
/// override, so it need not hold any real funds or permissions.
const SYNTHETIC_CALLER: Address = address!("000000000000000000000000000000000000CA11");

/// Basis-points nudge applied to a V3/Moe hop2 pool to clear the on-chain
/// profit invariant. 3% is comfortably larger than compounding round-trip
/// fees across two 2-3bps-fee-tier hops, with headroom for the block's real
/// spread on the day of measurement.
const NUDGE_BPS: u32 = 300;

/// Extra 1e30 headroom applied when overriding a pool/bin's token balance so
/// the nudged hop never reverts on its own liquidity ceiling.
const BALANCE_HEADROOM: u128 = 1_000_000_000_000_000_000_000_000_000_000;

#[derive(Debug, Parser)]
#[command(about = "Remeasure the mainnet gas profile against pinned Mantle mainnet state")]
struct Args {
    #[arg(long, default_value_t = 5000)]
    chain_id: u64,
    /// Pin to this block; defaults to the chain tip at request time.
    #[arg(long)]
    block: Option<u64>,
    #[arg(long, default_value = "contracts/executor/artifacts")]
    artifact: PathBuf,
    #[arg(long, default_value = "config/executor_identity.json")]
    identity: PathBuf,
    /// Directory containing (and receiving) `samples.jsonl`.
    #[arg(long, default_value = "config/gas_profiles/pinned")]
    out: PathBuf,
    #[arg(long, default_value = "config/gas_profiles/pinned/generator_config.json")]
    config: PathBuf,
    #[arg(long, default_value = "config/gas_profiles/mantle_mainnet_v1.json")]
    profile_out: PathBuf,
    #[arg(long, env = "MANTLE_FORK_RPC_URL", default_value = "https://rpc.mantle.xyz")]
    rpc_url: String,
    /// Regenerate the artifact from merged samples but skip writing it out.
    #[arg(long, default_value_t = false)]
    dry_run: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("remeasure_mainnet_gas_profile failed: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn amount_grid() -> Vec<U256> {
    let whole: Vec<u64> = (1..=10).chain([12, 15]).collect();
    whole
        .into_iter()
        .map(|w| U256::from(w) * U256::from(10u128.pow(18)))
        .collect()
}

fn upsert_override(
    map: &mut HashMap<Address, AccountStateOverride>,
    address: Address,
    code: Option<Bytes>,
    balance: Option<U256>,
    diffs: Vec<(B256, B256)>,
) {
    let entry = map.entry(address).or_insert_with(|| AccountStateOverride {
        address,
        code: None,
        balance: None,
        state_diff: Vec::new(),
    });
    if code.is_some() {
        entry.code = code;
    }
    if balance.is_some() {
        entry.balance = balance;
    }
    entry.state_diff.extend(diffs);
}

fn slot_key(slot: u64) -> B256 {
    B256::from(U256::from(slot))
}

fn register_pool(
    overrides: &mut HashMap<Address, AccountStateOverride>,
    executor: Address,
    pool: Address,
    pool_type: u8,
    token0: Address,
    token1: Address,
    fee: u32,
) {
    let slots = registered_pool_slots(
        pool,
        pool_type,
        token0,
        token1,
        fee,
        REGISTERED_POOLS_BASE_SLOT,
    );
    upsert_override(overrides, executor, None, None, slots.to_vec());
}

/// Every class needs the synthetic executor's `admin` slot set and its WMNT
/// balance funded; the executor's `code` override is merged in separately by
/// the caller in [`run`] once, since every `build_*` fn shares the same
/// bytecode and threading it through each one adds nothing.
fn base_overrides(executor: Address, caller: Address, amount_in: U256) -> HashMap<Address, AccountStateOverride> {
    let mut map = HashMap::new();
    upsert_override(&mut map, executor, None, None, vec![admin_override(caller)]);
    upsert_override(
        &mut map,
        WMNT,
        None,
        None,
        vec![erc20_balance_override(executor, amount_in, WMNT_BALANCE_SLOT)],
    );
    map
}

struct FusionxState {
    pool: Address,
    token0: Address,
    token1: Address,
    reserve0: U256,
    reserve1: U256,
}

async fn fetch_fusionx<P: Provider + Clone>(provider: P, pool: Address, block_id: BlockId) -> Result<FusionxState> {
    let pair = amms::execution::contract::IMoePair::new(pool, provider.clone());
    let token0 = pair.token0().block(block_id).call().await?;
    let token1 = pair.token1().block(block_id).call().await?;
    let reserves = pair.getReserves().block(block_id).call().await?;
    Ok(FusionxState {
        pool,
        token0,
        token1,
        reserve0: U256::from(reserves._0),
        reserve1: U256::from(reserves._1),
    })
}

struct AgniRaw {
    pool: Address,
    token0: Address,
    token1: Address,
    fee: u32,
    sqrt_price_x96: U256,
    slot0_word: B256,
}

async fn fetch_agni_raw<P: Provider + Clone>(provider: P, pool: Address, block_id: BlockId) -> Result<AgniRaw> {
    let c = IAgniPool::new(pool, provider.clone());
    let token0 = c.token0().block(block_id).call().await?;
    let token1 = c.token1().block(block_id).call().await?;
    let fee = c.fee().block(block_id).call().await?.to::<u32>();
    let slot0 = c.slot0().block(block_id).call().await?;
    let sqrt_price_x96 = U256::from(slot0.sqrtPriceX96);
    let raw = provider
        .get_storage_at(pool, U256::from(V3_SLOT0_SLOT))
        .block_id(block_id)
        .await?;
    Ok(AgniRaw {
        pool,
        token0,
        token1,
        fee,
        sqrt_price_x96,
        slot0_word: B256::from(raw),
    })
}

struct MoeRaw {
    pool: Address,
    token_x: Address,
    token_y: Address,
    bin_step: u16,
    active_id: u32,
    parameters_word: B256,
}

async fn fetch_moe_raw<P: Provider + Clone>(provider: P, pool: Address, block_id: BlockId) -> Result<MoeRaw> {
    let c = IMoeLBPair::new(pool, provider.clone());
    let token_x = c.getTokenX().block(block_id).call().await?;
    let token_y = c.getTokenY().block(block_id).call().await?;
    let bin_step = c.getBinStep().block(block_id).call().await?;
    let active_id = c.getActiveId().block(block_id).call().await?.to::<u32>();
    let raw = provider
        .get_storage_at(pool, U256::from(MOE_LB_PARAMETERS_SLOT))
        .block_id(block_id)
        .await?;
    Ok(MoeRaw {
        pool,
        token_x,
        token_y,
        bin_step,
        active_id,
        parameters_word: B256::from(raw),
    })
}

fn nudge_agni(overrides: &mut HashMap<Address, AccountStateOverride>, raw: &AgniRaw, zero_for_one: bool) {
    let new_price = v3_favorable_sqrt_price(raw.sqrt_price_x96, zero_for_one, NUDGE_BPS);
    let new_slot0 =
        v3_slot0_nudge(raw.slot0_word, new_price).expect("nudged sqrt price stays within the valid V3 range");
    let (liq_slot, liq_word) = v3_liquidity_override(u128::MAX / 2);
    upsert_override(
        overrides,
        raw.pool,
        None,
        None,
        vec![(slot_key(V3_SLOT0_SLOT), new_slot0), (liq_slot, liq_word)],
    );
}

fn nudge_moe(overrides: &mut HashMap<Address, AccountStateOverride>, raw: &MoeRaw, swap_for_y: bool) {
    let shift = (NUDGE_BPS / (raw.bin_step as u32).max(1)).max(1) + 1;
    let new_active_id = if swap_for_y {
        raw.active_id + shift
    } else {
        raw.active_id.saturating_sub(shift)
    };
    let new_parameters = moe_lb_parameters_word_with_active_id(raw.parameters_word, new_active_id);
    let bin_slot = moe_lb_bin_slot(new_active_id);
    let bin_word = moe_lb_bin_reserve_word(BALANCE_HEADROOM, BALANCE_HEADROOM);
    upsert_override(
        overrides,
        raw.pool,
        None,
        None,
        vec![
            (slot_key(MOE_LB_PARAMETERS_SLOT), new_parameters),
            (bin_slot, bin_word),
        ],
    );
}

/// Real on-chain state fetched once per run, reused across every class/amount
/// combination. Agni/Moe *primary* pools carry both a fully-synced
/// simulation object (used when the pool plays the real, unmodified hop in a
/// same-venue or venue-into-V2 class) and a raw storage word (used when that
/// same pool is nudged as hop2 in a V2-into-venue class) — the two roles are
/// independent per `measure_route` call, since state overrides never persist
/// across calls.
struct RealState {
    fusionx: FusionxState,
    agni_primary_synced: AgniPool,
    agni_primary_raw: AgniRaw,
    agni_secondary_raw: AgniRaw,
    moe_primary_synced: MoeLbPair,
    moe_primary_raw: MoeRaw,
    moe_secondary_raw: MoeRaw,
    block_timestamp: u64,
}

struct ClassPlan {
    name: &'static str,
    protocols: Vec<ProtocolKind>,
    pools: Vec<Address>,
    pool_types: Vec<u8>,
    amounts_out: Vec<U256>,
    overrides: Vec<AccountStateOverride>,
    venues: Vec<VenueRef>,
    notes: String,
}

fn build_v2_v2(state: &RealState, executor: Address, caller: Address, amount_in: U256) -> ClassPlan {
    let mut overrides = base_overrides(executor, caller, amount_in);
    register_pool(
        &mut overrides,
        executor,
        state.fusionx.pool,
        POOL_TYPE_V2,
        state.fusionx.token0,
        state.fusionx.token1,
        0,
    );

    let (reserve_in, reserve_out) = if state.fusionx.token0 == WMNT {
        (state.fusionx.reserve0, state.fusionx.reserve1)
    } else {
        (state.fusionx.reserve1, state.fusionx.reserve0)
    };
    let hop1_out = v2_conservative_amount_out(amount_in, reserve_in, reserve_out);

    let (settlement_out, pool_balance_override) = v2_final_hop_settlement(amount_in);
    upsert_override(
        &mut overrides,
        WMNT,
        None,
        None,
        vec![erc20_balance_override(state.fusionx.pool, pool_balance_override, WMNT_BALANCE_SLOT)],
    );

    ClassPlan {
        name: "v2/v2",
        protocols: vec![ProtocolKind::V2, ProtocolKind::V2],
        pools: vec![state.fusionx.pool, state.fusionx.pool],
        pool_types: vec![POOL_TYPE_V2, POOL_TYPE_V2],
        amounts_out: vec![hop1_out, settlement_out],
        venues: vec![
            VenueRef { protocol: ProtocolKind::V2, pool: state.fusionx.pool.to_string() },
            VenueRef { protocol: ProtocolKind::V2, pool: state.fusionx.pool.to_string() },
        ],
        notes: "hop2 settled via a WMNT balance stateOverride on the pool (v2_final_hop_settlement) to clear \
                the on-chain profit invariant by 1 wei; hop1 uses real, unmodified reserves. Swap control flow \
                and gas cost are identical to an unmodified round trip."
            .to_string(),
        overrides: overrides.into_values().collect(),
    }
}

fn build_v3_v3(state: &RealState, executor: Address, caller: Address, amount_in: U256) -> Result<ClassPlan> {
    let mut overrides = base_overrides(executor, caller, amount_in);
    register_pool(
        &mut overrides,
        executor,
        state.agni_primary_raw.pool,
        POOL_TYPE_V3,
        state.agni_primary_raw.token0,
        state.agni_primary_raw.token1,
        state.agni_primary_raw.fee,
    );
    register_pool(
        &mut overrides,
        executor,
        state.agni_secondary_raw.pool,
        POOL_TYPE_V3,
        state.agni_secondary_raw.token0,
        state.agni_secondary_raw.token1,
        state.agni_secondary_raw.fee,
    );

    let evidence = state
        .agni_primary_synced
        .simulate_swap_with_crossing_evidence(WMNT, amount_in)
        .context("simulate agni primary hop1 (v3/v3)")?;
    if evidence.crossing_count != 0 {
        bail!(
            "agni primary hop1 crosses {} ticks at amount_in={amount_in} (v3/v3); expected zero for bucket \"0\" scope",
            evidence.crossing_count
        );
    }

    let zero_for_one_hop2 = state.agni_secondary_raw.token0 == USDT;
    nudge_agni(&mut overrides, &state.agni_secondary_raw, zero_for_one_hop2);

    Ok(ClassPlan {
        name: "v3/v3",
        protocols: vec![ProtocolKind::V3, ProtocolKind::V3],
        pools: vec![state.agni_primary_raw.pool, state.agni_secondary_raw.pool],
        pool_types: vec![POOL_TYPE_V3, POOL_TYPE_V3],
        amounts_out: vec![U256::ZERO, U256::ZERO],
        venues: vec![
            VenueRef { protocol: ProtocolKind::V3, pool: state.agni_primary_raw.pool.to_string() },
            VenueRef { protocol: ProtocolKind::V3, pool: state.agni_secondary_raw.pool.to_string() },
        ],
        notes: format!(
            "hop1 real/unmodified (fee={}, verified zero tick crossings); hop2 (fee={}) nudged {NUDGE_BPS}bps \
             favorably via a slot0/liquidity stateOverride to clear the on-chain profit invariant. Swap control \
             flow and gas cost are identical to unmodified reserves.",
            state.agni_primary_raw.fee, state.agni_secondary_raw.fee
        ),
        overrides: overrides.into_values().collect(),
    })
}

fn build_moe_moe(state: &RealState, executor: Address, caller: Address, amount_in: U256) -> Result<ClassPlan> {
    let mut overrides = base_overrides(executor, caller, amount_in);
    register_pool(
        &mut overrides,
        executor,
        state.moe_primary_raw.pool,
        POOL_TYPE_MOE_LB,
        state.moe_primary_raw.token_x,
        state.moe_primary_raw.token_y,
        0,
    );
    register_pool(
        &mut overrides,
        executor,
        state.moe_secondary_raw.pool,
        POOL_TYPE_MOE_LB,
        state.moe_secondary_raw.token_x,
        state.moe_secondary_raw.token_y,
        0,
    );

    let swap_for_y_hop1 = state.moe_primary_raw.token_x == WMNT;
    let evidence = match state.moe_primary_synced.simulate_swap_with_crossing_evidence(
        swap_for_y_hop1,
        amount_in,
        state.block_timestamp,
    ) {
        Ok(evidence) => evidence,
        Err(AMMError::MoeError(MoeError::IncompleteState)) => bail!(
            "moe primary hop1 exhausts the synced bin range before amount_in={amount_in} is fully consumed \
             (moe/moe); real liquidity near the active bin crosses far more than zero bins, so this route is \
             Unsupported for bucket \"0\" scope at this amount"
        ),
        Err(err) => return Err(err).context("simulate moe primary hop1 (moe/moe)"),
    };
    if evidence.crossing_count != 0 {
        bail!(
            "moe primary hop1 crosses {} bins at amount_in={amount_in} (moe/moe); expected zero for bucket \"0\" scope",
            evidence.crossing_count
        );
    }

    let swap_for_y_hop2 = state.moe_secondary_raw.token_x == USDT;
    nudge_moe(&mut overrides, &state.moe_secondary_raw, swap_for_y_hop2);

    Ok(ClassPlan {
        name: "moe/moe",
        protocols: vec![ProtocolKind::Moe, ProtocolKind::Moe],
        pools: vec![state.moe_primary_raw.pool, state.moe_secondary_raw.pool],
        pool_types: vec![POOL_TYPE_MOE_LB, POOL_TYPE_MOE_LB],
        amounts_out: vec![U256::ZERO, U256::ZERO],
        venues: vec![
            VenueRef { protocol: ProtocolKind::Moe, pool: state.moe_primary_raw.pool.to_string() },
            VenueRef { protocol: ProtocolKind::Moe, pool: state.moe_secondary_raw.pool.to_string() },
        ],
        notes: format!(
            "hop1 real/unmodified (binStep={}, verified zero bin crossings); hop2 (binStep={}) nudged via an \
             active-bin-id + bin-reserve stateOverride to clear the on-chain profit invariant. Swap control flow \
             and gas cost are identical to unmodified reserves.",
            state.moe_primary_raw.bin_step, state.moe_secondary_raw.bin_step
        ),
        overrides: overrides.into_values().collect(),
    })
}

fn build_v2_v3(state: &RealState, executor: Address, caller: Address, amount_in: U256) -> ClassPlan {
    let mut overrides = base_overrides(executor, caller, amount_in);
    register_pool(
        &mut overrides,
        executor,
        state.fusionx.pool,
        POOL_TYPE_V2,
        state.fusionx.token0,
        state.fusionx.token1,
        0,
    );
    register_pool(
        &mut overrides,
        executor,
        state.agni_primary_raw.pool,
        POOL_TYPE_V3,
        state.agni_primary_raw.token0,
        state.agni_primary_raw.token1,
        state.agni_primary_raw.fee,
    );

    let (reserve_in, reserve_out) = if state.fusionx.token0 == WMNT {
        (state.fusionx.reserve0, state.fusionx.reserve1)
    } else {
        (state.fusionx.reserve1, state.fusionx.reserve0)
    };
    let (hop1_out, hop1_balance_override) =
        v2_generous_amount_out(amount_in, reserve_in, reserve_out, NUDGE_BPS);
    upsert_override(
        &mut overrides,
        USDT,
        None,
        None,
        vec![erc20_balance_override(state.fusionx.pool, hop1_balance_override, USDT_BALANCE_SLOT)],
    );

    let zero_for_one_hop2 = state.agni_primary_raw.token0 == USDT;
    nudge_agni(&mut overrides, &state.agni_primary_raw, zero_for_one_hop2);

    ClassPlan {
        name: "v2/v3",
        protocols: vec![ProtocolKind::V2, ProtocolKind::V3],
        pools: vec![state.fusionx.pool, state.agni_primary_raw.pool],
        pool_types: vec![POOL_TYPE_V2, POOL_TYPE_V3],
        amounts_out: vec![hop1_out, U256::ZERO],
        venues: vec![
            VenueRef { protocol: ProtocolKind::V2, pool: state.fusionx.pool.to_string() },
            VenueRef { protocol: ProtocolKind::V3, pool: state.agni_primary_raw.pool.to_string() },
        ],
        notes: format!(
            "hop1 (FusionX V2) nudged {NUDGE_BPS}bps favorably via a generous-amount + USDT-balance \
             stateOverride so hop2 receives a fair (not artificially halved) input; hop2 (Agni fee={}) nudged \
             {NUDGE_BPS}bps favorably via a slot0/liquidity stateOverride to clear the on-chain profit invariant. \
             Swap control flow and gas cost are identical to unmodified reserves.",
            state.agni_primary_raw.fee
        ),
        overrides: overrides.into_values().collect(),
    }
}

fn build_v3_v2(state: &RealState, executor: Address, caller: Address, amount_in: U256) -> Result<ClassPlan> {
    let mut overrides = base_overrides(executor, caller, amount_in);
    register_pool(
        &mut overrides,
        executor,
        state.agni_primary_raw.pool,
        POOL_TYPE_V3,
        state.agni_primary_raw.token0,
        state.agni_primary_raw.token1,
        state.agni_primary_raw.fee,
    );
    register_pool(
        &mut overrides,
        executor,
        state.fusionx.pool,
        POOL_TYPE_V2,
        state.fusionx.token0,
        state.fusionx.token1,
        0,
    );

    let evidence = state
        .agni_primary_synced
        .simulate_swap_with_crossing_evidence(WMNT, amount_in)
        .context("simulate agni primary hop1 (v3/v2)")?;
    if evidence.crossing_count != 0 {
        bail!(
            "agni primary hop1 crosses {} ticks at amount_in={amount_in} (v3/v2); expected zero for bucket \"0\" scope",
            evidence.crossing_count
        );
    }

    let (settlement_out, pool_balance_override) = v2_final_hop_settlement(amount_in);
    upsert_override(
        &mut overrides,
        WMNT,
        None,
        None,
        vec![erc20_balance_override(state.fusionx.pool, pool_balance_override, WMNT_BALANCE_SLOT)],
    );

    Ok(ClassPlan {
        name: "v3/v2",
        protocols: vec![ProtocolKind::V3, ProtocolKind::V2],
        pools: vec![state.agni_primary_raw.pool, state.fusionx.pool],
        pool_types: vec![POOL_TYPE_V3, POOL_TYPE_V2],
        amounts_out: vec![U256::ZERO, settlement_out],
        venues: vec![
            VenueRef { protocol: ProtocolKind::V3, pool: state.agni_primary_raw.pool.to_string() },
            VenueRef { protocol: ProtocolKind::V2, pool: state.fusionx.pool.to_string() },
        ],
        notes: "hop1 real/unmodified Agni (verified zero tick crossings); hop2 settled via a WMNT balance \
                stateOverride on the FusionX pool (v2_final_hop_settlement) to clear the on-chain profit \
                invariant by 1 wei. Swap control flow and gas cost are identical to an unmodified round trip."
            .to_string(),
        overrides: overrides.into_values().collect(),
    })
}

fn build_v2_moe(state: &RealState, executor: Address, caller: Address, amount_in: U256) -> ClassPlan {
    let mut overrides = base_overrides(executor, caller, amount_in);
    register_pool(
        &mut overrides,
        executor,
        state.fusionx.pool,
        POOL_TYPE_V2,
        state.fusionx.token0,
        state.fusionx.token1,
        0,
    );
    register_pool(
        &mut overrides,
        executor,
        state.moe_primary_raw.pool,
        POOL_TYPE_MOE_LB,
        state.moe_primary_raw.token_x,
        state.moe_primary_raw.token_y,
        0,
    );

    let (reserve_in, reserve_out) = if state.fusionx.token0 == WMNT {
        (state.fusionx.reserve0, state.fusionx.reserve1)
    } else {
        (state.fusionx.reserve1, state.fusionx.reserve0)
    };
    let (hop1_out, hop1_balance_override) =
        v2_generous_amount_out(amount_in, reserve_in, reserve_out, NUDGE_BPS);
    upsert_override(
        &mut overrides,
        USDT,
        None,
        None,
        vec![erc20_balance_override(state.fusionx.pool, hop1_balance_override, USDT_BALANCE_SLOT)],
    );

    let swap_for_y_hop2 = state.moe_primary_raw.token_x == USDT;
    nudge_moe(&mut overrides, &state.moe_primary_raw, swap_for_y_hop2);

    ClassPlan {
        name: "v2/moe",
        protocols: vec![ProtocolKind::V2, ProtocolKind::Moe],
        pools: vec![state.fusionx.pool, state.moe_primary_raw.pool],
        pool_types: vec![POOL_TYPE_V2, POOL_TYPE_MOE_LB],
        amounts_out: vec![hop1_out, U256::ZERO],
        venues: vec![
            VenueRef { protocol: ProtocolKind::V2, pool: state.fusionx.pool.to_string() },
            VenueRef { protocol: ProtocolKind::Moe, pool: state.moe_primary_raw.pool.to_string() },
        ],
        notes: format!(
            "hop1 (FusionX V2) nudged {NUDGE_BPS}bps favorably via a generous-amount + USDT-balance \
             stateOverride so hop2 receives a fair (not artificially halved) input; hop2 (Moe binStep={}) nudged \
             via an active-bin-id + bin-reserve stateOverride to clear the on-chain profit invariant. Swap \
             control flow and gas cost are identical to unmodified reserves.",
            state.moe_primary_raw.bin_step
        ),
        overrides: overrides.into_values().collect(),
    }
}

fn build_moe_v2(state: &RealState, executor: Address, caller: Address, amount_in: U256) -> Result<ClassPlan> {
    let mut overrides = base_overrides(executor, caller, amount_in);
    register_pool(
        &mut overrides,
        executor,
        state.moe_primary_raw.pool,
        POOL_TYPE_MOE_LB,
        state.moe_primary_raw.token_x,
        state.moe_primary_raw.token_y,
        0,
    );
    register_pool(
        &mut overrides,
        executor,
        state.fusionx.pool,
        POOL_TYPE_V2,
        state.fusionx.token0,
        state.fusionx.token1,
        0,
    );

    let swap_for_y_hop1 = state.moe_primary_raw.token_x == WMNT;
    let evidence = match state.moe_primary_synced.simulate_swap_with_crossing_evidence(
        swap_for_y_hop1,
        amount_in,
        state.block_timestamp,
    ) {
        Ok(evidence) => evidence,
        Err(AMMError::MoeError(MoeError::IncompleteState)) => bail!(
            "moe primary hop1 exhausts the synced bin range before amount_in={amount_in} is fully consumed \
             (moe/v2); real liquidity near the active bin crosses far more than zero bins, so this route is \
             Unsupported for bucket \"0\" scope at this amount"
        ),
        Err(err) => return Err(err).context("simulate moe primary hop1 (moe/v2)"),
    };
    if evidence.crossing_count != 0 {
        bail!(
            "moe primary hop1 crosses {} bins at amount_in={amount_in} (moe/v2); expected zero for bucket \"0\" scope",
            evidence.crossing_count
        );
    }

    let (settlement_out, pool_balance_override) = v2_final_hop_settlement(amount_in);
    upsert_override(
        &mut overrides,
        WMNT,
        None,
        None,
        vec![erc20_balance_override(state.fusionx.pool, pool_balance_override, WMNT_BALANCE_SLOT)],
    );

    Ok(ClassPlan {
        name: "moe/v2",
        protocols: vec![ProtocolKind::Moe, ProtocolKind::V2],
        pools: vec![state.moe_primary_raw.pool, state.fusionx.pool],
        pool_types: vec![POOL_TYPE_MOE_LB, POOL_TYPE_V2],
        amounts_out: vec![U256::ZERO, settlement_out],
        venues: vec![
            VenueRef { protocol: ProtocolKind::Moe, pool: state.moe_primary_raw.pool.to_string() },
            VenueRef { protocol: ProtocolKind::V2, pool: state.fusionx.pool.to_string() },
        ],
        notes: "hop1 real/unmodified Moe (verified zero bin crossings); hop2 settled via a WMNT balance \
                stateOverride on the FusionX pool (v2_final_hop_settlement) to clear the on-chain profit \
                invariant by 1 wei. Swap control flow and gas cost are identical to an unmodified round trip."
            .to_string(),
        overrides: overrides.into_values().collect(),
    })
}

fn build_class_plan(
    class: &str,
    state: &RealState,
    executor: Address,
    caller: Address,
    amount_in: U256,
) -> Result<ClassPlan> {
    Ok(match class {
        "v2/v2" => build_v2_v2(state, executor, caller, amount_in),
        "v3/v3" => build_v3_v3(state, executor, caller, amount_in)?,
        "moe/moe" => build_moe_moe(state, executor, caller, amount_in)?,
        "v2/v3" => build_v2_v3(state, executor, caller, amount_in),
        "v3/v2" => build_v3_v2(state, executor, caller, amount_in)?,
        "v2/moe" => build_v2_moe(state, executor, caller, amount_in),
        "moe/v2" => build_moe_v2(state, executor, caller, amount_in)?,
        other => bail!("unknown route class {other}"),
    })
}

const ROUTE_CLASSES: [&str; 7] = ["v2/v2", "v3/v3", "moe/moe", "v2/v3", "v3/v2", "v2/moe", "moe/v2"];

#[allow(clippy::too_many_arguments)]
async fn measure_and_record<P: Provider + Clone>(
    provider: P,
    executor: Address,
    caller: Address,
    amount_in: U256,
    block_number: u64,
    block_hash: B256,
    deadline: U256,
    chain_id: u64,
    executor_code_hash: &str,
    plan: ClassPlan,
) -> Result<GasSample> {
    let route_key = RouteKey::new(plan.protocols.clone())?;
    let venues = plan.venues.clone();
    let notes = plan.notes.clone();
    let state_override = build_state_override(plan.overrides);

    let result = mainnet_fork_harness::measure_route(
        provider,
        executor,
        caller,
        amount_in,
        vec![WMNT, USDT, WMNT],
        plan.pools,
        plan.pool_types,
        plan.amounts_out,
        U256::ZERO,
        deadline,
        state_override,
        block_number,
    )
    .await;

    let (gas_used, outcome, notes) = match result {
        Ok(gas_used) => (gas_used, SampleOutcome::Success, notes),
        Err(err) => (0, SampleOutcome::Reverted, format!("{notes} — reverted/failed: {err:#}")),
    };

    Ok(GasSample {
        route_key,
        gas_used,
        source: SampleSource::ForkReplay,
        executor_code_hash: executor_code_hash.to_string(),
        chain_id,
        block_number,
        block_hash: Some(block_hash.to_string()),
        tx_hash: None,
        effective_gas_price_wei: None,
        base_fee_wei: None,
        block_gas_limit: None,
        inclusion_latency_blocks: None,
        notes: Some(format!("[{}] {}", plan.name, notes)),
        venues: Some(venues),
        calldata_digest: None,
        outcome: Some(outcome),
    })
}

async fn run() -> Result<()> {
    let args = Args::parse();
    // The public rpc.mantle.xyz endpoint rate-limits (HTTP 429) under the burst
    // of eth_call/eth_getStorageAt/eth_estimateGas traffic this tool generates;
    // throttle + retry with backoff, matching the pattern already used by
    // `examples/protocols/agni/list_mantle_agni_pools.rs` for the same endpoint.
    let client = ClientBuilder::default()
        .layer(ThrottleLayer::new(250))
        .layer(RetryBackoffLayer::new(5, 200, 330))
        .http(args.rpc_url.parse()?);
    let provider = ProviderBuilder::new().connect_client(client);

    let block_number = match args.block {
        Some(b) => b,
        None => provider.get_block_number().await?,
    };
    let header = provider
        .get_block_by_number(block_number.into())
        .await?
        .ok_or_else(|| eyre::eyre!("missing block {block_number}"))?;
    let block_hash = header.header().hash();
    let block_timestamp = header.header().timestamp;
    let context = MoeSnapshotContext::new(block_hash, block_timestamp);
    let block_id = BlockId::hash_canonical(context.block_hash);

    let evidence = BuildEvidence::load(&args.artifact)
        .with_context(|| format!("load build evidence from {}", args.artifact.display()))?;
    let plan = resolve_immutable_plan(&evidence, ImmutableInputs { wmnt: WMNT }, args.chain_id)
        .context("resolve immutable executor plan from source")?;
    let derived = build_export(&plan);

    let loaded_raw = std::fs::read_to_string(&args.identity)
        .with_context(|| format!("read {}", args.identity.display()))?;
    let loaded: ExecutorIdentityExport =
        serde_json::from_str(&loaded_raw).context("parse executor_identity.json")?;
    if derived.identity_digest != loaded.identity_digest
        || derived.template_hash != loaded.template_hash
        || derived.patched_runtime_hash != loaded.patched_runtime_hash
    {
        bail!(
            "executor identity mismatch: source-derived (template={} patched={} identity={}) vs pinned {} \
             (template={} patched={} identity={}) — re-run derive_runtime_identity and update the pinned file \
             before remeasuring",
            derived.template_hash,
            derived.patched_runtime_hash,
            derived.identity_digest,
            args.identity.display(),
            loaded.template_hash,
            loaded.patched_runtime_hash,
            loaded.identity_digest,
        );
    }
    let executor_code_hash = loaded.template_hash.clone();
    let executor_code = Bytes::from(plan.patched_bytes().to_vec());

    println!(
        "measuring at chain_id={} block={block_number} block_hash={block_hash} executor_code_hash={executor_code_hash}",
        args.chain_id
    );

    let fusionx = fetch_fusionx(provider.clone(), FUSIONX_V2_POOL, block_id).await?;

    let agni_primary_synced = AgniPool::new(AGNI_PRIMARY_POOL)
        .init_basic(block_id, provider.clone())
        .await
        .context("init_basic agni primary pool")?;
    let agni_primary_raw = fetch_agni_raw(provider.clone(), AGNI_PRIMARY_POOL, block_id).await?;
    let agni_secondary_raw = fetch_agni_raw(provider.clone(), AGNI_SECONDARY_POOL, block_id).await?;

    let mut moe_primary_amms: Vec<AMM> = vec![AMM::MoeLbPair(MoeLbPair::new(MOE_PRIMARY_POOL))];
    sync_moe_snapshots_batch(
        &mut moe_primary_amms,
        block_id,
        provider.clone(),
        context,
        MoeSnapshotSyncConfig::default(),
    )
    .await
    .context("sync moe primary snapshot")?;
    let AMM::MoeLbPair(moe_primary_synced) = moe_primary_amms.remove(0) else {
        unreachable!("moe_primary_amms was constructed with a single MoeLbPair variant")
    };
    let moe_primary_raw = fetch_moe_raw(provider.clone(), MOE_PRIMARY_POOL, block_id).await?;
    let moe_secondary_raw = fetch_moe_raw(provider.clone(), MOE_SECONDARY_POOL, block_id).await?;

    let state = RealState {
        fusionx,
        agni_primary_synced,
        agni_primary_raw,
        agni_secondary_raw,
        moe_primary_synced,
        moe_primary_raw,
        moe_secondary_raw,
        block_timestamp,
    };

    std::fs::create_dir_all(&args.out)
        .with_context(|| format!("create output directory {}", args.out.display()))?;
    let samples_path = args.out.join("samples.jsonl");

    // Re-running this example must be idempotent: a prior run's fork_replay
    // samples (possibly measured before a harness bug fix, or at a different
    // pinned block) must not linger alongside this run's fresh measurements.
    // Non-fork_replay samples (e.g. the relabeled foundry_mock fixtures) are
    // untouched, matching the plan's "merge, don't clobber" requirement.
    if samples_path.exists() {
        let retained: Vec<GasSample> = load_samples_jsonl(&samples_path)
            .with_context(|| format!("load existing samples {}", samples_path.display()))?
            .into_iter()
            .filter(|s| s.source != SampleSource::ForkReplay)
            .collect();
        let mut rewritten = String::new();
        for sample in &retained {
            let line = serde_json::to_string(sample).context("serialize retained gas sample")?;
            rewritten.push_str(&line);
            rewritten.push('\n');
        }
        std::fs::write(&samples_path, rewritten)
            .with_context(|| format!("rewrite {} without stale fork_replay samples", samples_path.display()))?;
    }

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&samples_path)
        .with_context(|| format!("open {} for append", samples_path.display()))?;

    let deadline = U256::from(block_timestamp) + U256::from(3600u64);
    let mut recorded = 0usize;

    for amount_in in amount_grid() {
        for &class in ROUTE_CLASSES.iter() {
            let class_plan = match build_class_plan(class, &state, SYNTHETIC_EXECUTOR, SYNTHETIC_CALLER, amount_in) {
                Ok(plan) => plan,
                Err(err) => {
                    eprintln!("skipping {class} at amount_in={amount_in}: {err:#}");
                    continue;
                }
            };
            // Every class's overrides include an entry for SYNTHETIC_EXECUTOR
            // (from base_overrides()'s admin slot write); merge in the shared
            // bytecode here rather than threading it through every build_* fn.
            let mut overrides = class_plan.overrides;
            let executor_entry = overrides
                .iter_mut()
                .find(|entry| entry.address == SYNTHETIC_EXECUTOR)
                .expect("base_overrides() always writes an entry for the executor address");
            executor_entry.code = Some(executor_code.clone());

            let sample = measure_and_record(
                provider.clone(),
                SYNTHETIC_EXECUTOR,
                SYNTHETIC_CALLER,
                amount_in,
                block_number,
                block_hash,
                deadline,
                args.chain_id,
                &executor_code_hash,
                ClassPlan { overrides, ..class_plan },
            )
            .await?;

            println!(
                "{class} amount_in={amount_in} -> outcome={:?} gas_used={}",
                sample.outcome, sample.gas_used
            );

            let line = serde_json::to_string(&sample).context("serialize gas sample")?;
            writeln!(file, "{line}").context("append gas sample")?;
            recorded += 1;
        }
    }

    println!("recorded {recorded} fork-replay samples into {}", samples_path.display());

    let config = load_generator_config(&args.config)
        .with_context(|| format!("load generator config {}", args.config.display()))?;
    let samples = load_samples_jsonl(&samples_path)
        .with_context(|| format!("load merged samples {}", samples_path.display()))?;
    let artifact = generate_artifact(&config, &samples).context("generate gas profile artifact")?;

    let approved = artifact
        .profiles
        .iter()
        .filter(|p| p.status == ProfileStatus::Approved)
        .count();
    let unsupported = artifact
        .profiles
        .iter()
        .filter(|p| p.status == ProfileStatus::Unsupported)
        .count();
    println!("artifact: approved={approved} unsupported={unsupported} content_digest={}", artifact.content_digest);

    if approved == 0 {
        bail!("no approved profiles were produced from the merged sample set — refusing to write an empty artifact");
    }

    if args.dry_run {
        println!("--dry-run set: not writing {}", args.profile_out.display());
    } else {
        write_artifact(&args.profile_out, &artifact)
            .with_context(|| format!("write artifact to {}", args.profile_out.display()))?;
        println!("wrote {}", args.profile_out.display());
    }

    Ok(())
}
