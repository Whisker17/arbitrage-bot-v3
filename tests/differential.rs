//! Exact fixed-state AMM differential suite (WHI-522).
//!
//! Offline path rebuilds pools from committed fixtures only and asserts exact
//! integer equality against recorded on-chain outputs. Live capture is gated
//! behind `#[ignore]` and writes canonical fixtures under
//! `tests/fixtures/differential/`.

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    str::FromStr,
};

use alloy::{
    consensus::BlockHeader,
    eips::BlockId,
    network::primitives::{BlockResponse, HeaderResponse},
    primitives::{aliases::U24, Address, B256, U160, U256},
    providers::{Provider, ProviderBuilder},
    sol,
};
use amms::amms::{
    agni::{AgniPool, Info as AgniInfo},
    amm::AutomatedMarketMaker,
    error::AMMError,
    moe::{
        math::price_helper,
        sync_moe_snapshots_batch, sync_token_decimals, BinReserve, MoeBinRange, MoeError, MoeLbPair,
        MoeSnapshot, MoeSnapshotContext, MoeSnapshotSyncConfig, MoeSlot0,
    },
    uniswap_v2::UniswapV2Pool,
    uniswap_v3::{Info as UniV3Info, UniswapV3Pool},
    Token,
};
use eyre::{bail, eyre, Context, Result};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

const FIXTURE_DIR: &str = "tests/fixtures/differential";
const SCHEMA_VERSION: u32 = 1;
const MANTLE_CHAIN_ID: u64 = 5000;
const V2_FEE_DOMAIN_END: usize = 100_000;
const MOE_BINS_RADIUS: u32 = 25;


fn serialize_u128<S>(value: &u128, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&value.to_string())
}

fn deserialize_u128<'de, D>(deserializer: D) -> Result<u128, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    raw.parse::<u128>().map_err(serde::de::Error::custom)
}

fn serialize_i128<S>(value: &i128, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&value.to_string())
}

fn deserialize_i128<'de, D>(deserializer: D) -> Result<i128, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    raw.parse::<i128>().map_err(serde::de::Error::custom)
}


// Candidate venues used by the ignored capture path. Addresses are fixture
// inputs only; offline tests never hardcode them outside committed JSON.
const MOE_V1_PAIR: &str = "0x4e7685df06201521f35a182467feefe02c53d847";
const MOE_V1_FACTORY: &str = "0x5bEf015CA9424A7C07B68490616a4C1F094BEdEc";
const MOE_V1_ROUTER: &str = "0xeaEE7EE68874218c3558b40063c42B82D3E7232a";
const FUSIONX_V2_PAIR: &str = "0x3e5922cD0CeC71dc2d60eC8b36aa4C05B7c1672f";
const FUSIONX_V2_FACTORY: &str = "0xE5020961fA51ffd3662CDf307dEf18F9a87Cce7c";
const FUSIONX_V2_ROUTER: &str = "0xDd0840118bF9CCCc6d67b2944ddDfbdb995955FD";
const FUSIONX_V3_POOL: &str = "0xD3d3127D9654f806370da592eb292eA0a347f0e3";
const FUSIONX_V3_FACTORY: &str = "0x530d2766D1988CC1c000C8b7d00334c14B69AD71";
const FUSIONX_V3_QUOTER: &str = "0x90f72244294E7c5028aFd6a96E18CC2c1E913995";
const AGNI_POOL: &str = "0xeAfc4D6d4c3391Cd4Fc10c85D2f5f972d58C0dD5";
const AGNI_FACTORY: &str = "0x25780dc8Fc3cfBD75F33bFDAB65e969b603b2035";
const AGNI_QUOTER: &str = "0x9488C05a7b75a6FefdcAE4f11a33467bcBA60177";
const AGNI_QUOTER_V2: &str = "0xc4aaDc921E1cdb66c5300Bc158a313292923C0cb";
const MOE_LB_PAIR: &str = "0x2612E3280ca8836F58173bF7EcC35e258Dc1b54B";
const MOE_LB_FACTORY: &str = "0xa6630671775c4ea2743840f9a5016dcf2a104054";

sol! {
    #[sol(rpc)]
    interface IERC20Meta {
        function decimals() external view returns (uint8);
    }

    #[sol(rpc)]
    interface IUniswapV2PairView {
        function token0() external view returns (address);
        function token1() external view returns (address);
        function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);
        function factory() external view returns (address);
    }

    #[sol(rpc)]
    interface IUniswapV2RouterView {
        function getAmountsOut(uint256 amountIn, address[] calldata path) external view returns (uint256[] memory amounts);
        function factory() external view returns (address);
    }

    #[sol(rpc)]
    interface IQuoterV1 {
        function quoteExactInputSingle(
            address tokenIn,
            address tokenOut,
            uint24 fee,
            uint256 amountIn,
            uint160 sqrtPriceLimitX96
        ) external returns (uint256 amountOut);
    }

    #[sol(rpc)]
    interface IQuoterV2 {
        struct QuoteExactInputSingleParams {
            address tokenIn;
            address tokenOut;
            uint256 amountIn;
            uint24 fee;
            uint160 sqrtPriceLimitX96;
        }

        function quoteExactInputSingle(QuoteExactInputSingleParams memory params)
            external
            returns (
                uint256 amountOut,
                uint160 sqrtPriceX96After,
                uint32 initializedTicksCrossed,
                uint256 gasEstimate
            );
    }

    #[sol(rpc)]
    interface IUniswapV3PoolView {
        function factory() external view returns (address);
        function token0() external view returns (address);
        function token1() external view returns (address);
        function fee() external view returns (uint24);
        function tickSpacing() external view returns (int24);
        function liquidity() external view returns (uint128);
        function slot0() external view returns (
            uint160 sqrtPriceX96,
            int24 tick,
            uint16 observationIndex,
            uint16 observationCardinality,
            uint16 observationCardinalityNext,
            uint8 feeProtocol,
            bool unlocked
        );
    }

    #[sol(rpc)]
    interface IMoeLBPairHooks {
        function getLBHooksParameters() external view returns (bytes32);
        function getTokenX() external view returns (address);
        function getTokenY() external view returns (address);
        function getBinStep() external view returns (uint16);
        function getActiveId() external view returns (uint24);
        function getSwapOut(uint128 amountIn, bool swapForY)
            external
            view
            returns (uint128 amountInLeft, uint128 amountOut, uint128 fee);
        function getPriceFromId(uint24 id) external view returns (uint256 price);
        function getFactory() external view returns (address);
    }
}

#[derive(Debug, thiserror::Error)]
enum DifferentialError {
    #[error("unsupported LB hooks parameters: {0}")]
    UnsupportedHooks(B256),
    #[error("fixture factory provenance mismatch")]
    FactoryMismatch,
    #[error("fee provenance failed: {0}")]
    FeeProvenance(String),
    #[error("canonical block hash changed during capture")]
    BlockHashChanged,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ProtocolKind {
    UniswapV2,
    UniswapV3,
    Agni,
    Moe,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct SnapshotMeta {
    chain_id: u64,
    block_number: u64,
    block_hash: B256,
    block_timestamp: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct TruthSource {
    contract: Address,
    method: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum FeeProvenance {
    Getter {
        contract: Address,
        selector: String,
        value: usize,
    },
    InferredVerified {
        quoter: Address,
        method: String,
        search_domain: [usize; 2],
        inputs: Vec<U256>,
        value: usize,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct TokenFixture {
    address: Address,
    decimals: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct V2State {
    token0: TokenFixture,
    token1: TokenFixture,
    #[serde(serialize_with = "serialize_u128", deserialize_with = "deserialize_u128")]
    reserve0: u128,
    #[serde(serialize_with = "serialize_u128", deserialize_with = "deserialize_u128")]
    reserve1: u128,
    fee: usize,
    fee_provenance: FeeProvenance,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct TickInfoFixture {
    #[serde(serialize_with = "serialize_u128", deserialize_with = "deserialize_u128")]
    liquidity_gross: u128,
    #[serde(serialize_with = "serialize_i128", deserialize_with = "deserialize_i128")]
    liquidity_net: i128,
    initialized: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct V3State {
    token0: TokenFixture,
    token1: TokenFixture,
    fee: u32,
    tick_spacing: i32,
    sqrt_price: U256,
    tick: i32,
    #[serde(serialize_with = "serialize_u128", deserialize_with = "deserialize_u128")]
    liquidity: u128,
    tick_bitmap: Vec<(i16, U256)>,
    tick_bitmap_coverage: Vec<i16>,
    ticks: Vec<(i32, TickInfoFixture)>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct MoeBinFixture {
    id: u32,
    #[serde(serialize_with = "serialize_u128", deserialize_with = "deserialize_u128")]
    reserve_x: u128,
    #[serde(serialize_with = "serialize_u128", deserialize_with = "deserialize_u128")]
    reserve_y: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct MoeSlot0Fixture {
    active_id: u32,
    bin_step: u16,
    #[serde(serialize_with = "serialize_u128", deserialize_with = "deserialize_u128")]
    reserve_x: u128,
    #[serde(serialize_with = "serialize_u128", deserialize_with = "deserialize_u128")]
    reserve_y: u128,
    volatility_accumulator: u32,
    volatility_reference: u32,
    id_reference: u32,
    timestamp: U256,
    base_factor: u16,
    filter_period: u16,
    decay_period: u16,
    reduction_factor: u16,
    variable_fee_control: u32,
    protocol_share_bps: u16,
    max_volatility_acc: u32,
}

impl From<&MoeSlot0> for MoeSlot0Fixture {
    fn from(slot0: &MoeSlot0) -> Self {
        Self {
            active_id: slot0.active_id,
            bin_step: slot0.bin_step,
            reserve_x: slot0.reserve_x,
            reserve_y: slot0.reserve_y,
            volatility_accumulator: slot0.volatility_accumulator,
            volatility_reference: slot0.volatility_reference,
            id_reference: slot0.id_reference,
            timestamp: slot0.timestamp,
            base_factor: slot0.base_factor,
            filter_period: slot0.filter_period,
            decay_period: slot0.decay_period,
            reduction_factor: slot0.reduction_factor,
            variable_fee_control: slot0.variable_fee_control,
            protocol_share_bps: slot0.protocol_share_bps,
            max_volatility_acc: slot0.max_volatility_acc,
        }
    }
}

impl From<&MoeSlot0Fixture> for MoeSlot0 {
    fn from(slot0: &MoeSlot0Fixture) -> Self {
        Self {
            active_id: slot0.active_id,
            bin_step: slot0.bin_step,
            reserve_x: slot0.reserve_x,
            reserve_y: slot0.reserve_y,
            volatility_accumulator: slot0.volatility_accumulator,
            volatility_reference: slot0.volatility_reference,
            id_reference: slot0.id_reference,
            timestamp: slot0.timestamp,
            base_factor: slot0.base_factor,
            filter_period: slot0.filter_period,
            decay_period: slot0.decay_period,
            reduction_factor: slot0.reduction_factor,
            variable_fee_control: slot0.variable_fee_control,
            protocol_share_bps: slot0.protocol_share_bps,
            max_volatility_acc: slot0.max_volatility_acc,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MoeState {
    token_x: TokenFixture,
    token_y: TokenFixture,
    bin_step: u16,
    hooks_parameters: B256,
    slot0: MoeSlot0Fixture,
    bins: Vec<MoeBinFixture>,
    queried_ranges: Vec<MoeBinRange>,
    block_hash: B256,
    block_timestamp: u64,
    prices: Vec<MoePriceCase>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct MoePriceCase {
    id: u32,
    price: U256,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "protocol", rename_all = "snake_case")]
enum ProtocolState {
    UniswapV2(V2State),
    UniswapV3(V3State),
    Agni(V3State),
    Moe(MoeState),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct SwapCase {
    amount_in: U256,
    /// token_in address for V2/V3/Agni; for Moe, ignored when swap_for_y is set.
    token_in: Address,
    token_out: Address,
    /// Moe-only direction. When set, token_in/out are still recorded for readability.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    swap_for_y: Option<bool>,
    expected_amount_out: U256,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expected_amount_in_left: Option<U256>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expected_fee: Option<U256>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expected_sqrt_price_after: Option<U256>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expected_initialized_ticks_crossed: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    crosses_initialized_tick: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    crosses_bins: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DifferentialFixture {
    schema_version: u32,
    protocol: ProtocolKind,
    snapshot: SnapshotMeta,
    pool: Address,
    factory: Address,
    truth_source: TruthSource,
    state: ProtocolState,
    cases: Vec<SwapCase>,
}

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(FIXTURE_DIR)
}

fn canonical_json(value: &DifferentialFixture) -> Result<String> {
    // Stable pretty JSON via serde field order + BTree-backed map entries.
    let raw = serde_json::to_vec_pretty(value)?;
    // Ensure trailing newline for POSIX-friendly fixtures.
    let mut out = String::from_utf8(raw)?;
    if !out.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

fn write_fixture(name: &str, fixture: &DifferentialFixture) -> Result<PathBuf> {
    let dir = fixture_dir();
    fs::create_dir_all(&dir)?;
    let path = dir.join(name);
    let body = canonical_json(fixture)?;
    fs::write(&path, body)?;
    Ok(path)
}

fn load_fixture(path: &Path) -> Result<DifferentialFixture> {
    let body = fs::read_to_string(path)
        .with_context(|| format!("read fixture {}", path.display()))?;
    let fixture: DifferentialFixture = serde_json::from_str(&body)
        .with_context(|| format!("parse fixture {}", path.display()))?;
    if fixture.schema_version != SCHEMA_VERSION {
        bail!(
            "unsupported fixture schema_version {} in {}",
            fixture.schema_version,
            path.display()
        );
    }
    Ok(fixture)
}

fn load_all_fixtures() -> Result<Vec<(PathBuf, DifferentialFixture)>> {
    let dir = fixture_dir();
    if !dir.exists() {
        bail!("fixture directory missing: {}", dir.display());
    }
    let mut paths: Vec<PathBuf> = fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("json"))
        .collect();
    paths.sort();
    paths
        .into_iter()
        .map(|p| load_fixture(&p).map(|f| (p, f)))
        .collect()
}

fn addr(s: &str) -> Address {
    Address::from_str(s).expect("valid address literal")
}

fn sorted_bitmap(map: &HashMap<i16, U256>) -> Vec<(i16, U256)> {
    let mut out: Vec<(i16, U256)> = map.iter().map(|(k, v)| (*k, *v)).collect();
    out.sort_by_key(|(k, _)| *k);
    out
}

fn sorted_ticks_uni(map: &HashMap<i32, UniV3Info>) -> Vec<(i32, TickInfoFixture)> {
    let mut out: Vec<(i32, TickInfoFixture)> = map
        .iter()
        .map(|(k, v)| {
            (
                *k,
                TickInfoFixture {
                    liquidity_gross: v.liquidity_gross,
                    liquidity_net: v.liquidity_net,
                    initialized: v.initialized,
                },
            )
        })
        .collect();
    out.sort_by_key(|(k, _)| *k);
    out
}

fn sorted_ticks_agni(map: &HashMap<i32, AgniInfo>) -> Vec<(i32, TickInfoFixture)> {
    let mut out: Vec<(i32, TickInfoFixture)> = map
        .iter()
        .map(|(k, v)| {
            (
                *k,
                TickInfoFixture {
                    liquidity_gross: v.liquidity_gross,
                    liquidity_net: v.liquidity_net,
                    initialized: v.initialized,
                },
            )
        })
        .collect();
    out.sort_by_key(|(k, _)| *k);
    out
}

fn sorted_coverage(set: &HashSet<i16>) -> Vec<i16> {
    let mut out: Vec<i16> = set.iter().copied().collect();
    out.sort_unstable();
    out
}

fn sorted_bins(map: &HashMap<u32, BinReserve>) -> Vec<MoeBinFixture> {
    let mut out: Vec<MoeBinFixture> = map
        .iter()
        .map(|(id, b)| MoeBinFixture {
            id: *id,
            reserve_x: b.reserve_x,
            reserve_y: b.reserve_y,
        })
        .collect();
    out.sort_by_key(|b| b.id);
    out
}



fn crosses_initialized_tick_v3(state: &V3State, pool_addr: Address, case_token_in: Address, case_token_out: Address, amount_in: U256) -> bool {
    let pool = rebuild_v3(state, pool_addr);
    let before_liq = pool.liquidity;
    let mut live = pool.clone();
    let Ok(_) = live.simulate_swap_mut(case_token_in, case_token_out, amount_in) else {
        return false;
    };
    if live.liquidity != before_liq {
        return true;
    }
    // Tick can move inside a single liquidity range without crossing an initialized
    // tick. Force a missing record on the next initialized boundary and require fail-closed.
    let mut incomplete = state.clone();
    let boundary = next_initialized_tick_on_path(state, case_token_in);
    drop_initialized_tick(&mut incomplete, boundary);
    let broken = rebuild_v3(&incomplete, pool_addr);
    matches!(
        broken.simulate_swap(case_token_in, case_token_out, amount_in),
        Err(AMMError::IncompleteState)
    )
}

fn crosses_initialized_tick_agni(state: &V3State, pool_addr: Address, case_token_in: Address, case_token_out: Address, amount_in: U256) -> bool {
    let pool = rebuild_agni(state, pool_addr);
    let before_liq = pool.liquidity;
    let mut live = pool.clone();
    let Ok(_) = live.simulate_swap_mut(case_token_in, case_token_out, amount_in) else {
        return false;
    };
    if live.liquidity != before_liq {
        return true;
    }
    let mut incomplete = state.clone();
    let boundary = next_initialized_tick_on_path(state, case_token_in);
    drop_initialized_tick(&mut incomplete, boundary);
    let broken = rebuild_agni(&incomplete, pool_addr);
    matches!(
        broken.simulate_swap(case_token_in, case_token_out, amount_in),
        Err(AMMError::IncompleteState)
    )
}

fn next_initialized_tick_on_path(state: &V3State, token_in: Address) -> i32 {
    let zero_for_one = token_in == state.token0.address;
    let candidates: Vec<i32> = state
        .ticks
        .iter()
        .filter(|(_, info)| info.initialized)
        .map(|(tick, _)| *tick)
        .collect();
    if zero_for_one {
        candidates
            .into_iter()
            .filter(|tick| *tick <= state.tick)
            .max()
            .expect("zero_for_one path needs an initialized tick at or below current")
    } else {
        candidates
            .into_iter()
            .filter(|tick| *tick > state.tick)
            .min()
            .expect("one_for_zero path needs an initialized tick above current")
    }
}

fn drop_initialized_tick(state: &mut V3State, tick: i32) {
    let before = state.ticks.len();
    state.ticks.retain(|(t, _)| *t != tick);
    assert!(
        state.ticks.len() + 1 == before,
        "expected to drop exactly one tick record for {tick}"
    );
}

fn rebuild_v2(state: &V2State, pool: Address) -> UniswapV2Pool {
    UniswapV2Pool {
        address: pool,
        token_a: Token::new_with_decimals(state.token0.address, state.token0.decimals),
        token_b: Token::new_with_decimals(state.token1.address, state.token1.decimals),
        reserve_0: state.reserve0,
        reserve_1: state.reserve1,
        fee: state.fee,
    }
}

fn rebuild_v3(state: &V3State, pool: Address) -> UniswapV3Pool {
    let mut tick_bitmap = HashMap::new();
    for (k, v) in &state.tick_bitmap {
        tick_bitmap.insert(*k, *v);
    }
    let mut ticks = HashMap::new();
    for (k, v) in &state.ticks {
        ticks.insert(
            *k,
            UniV3Info {
                liquidity_gross: v.liquidity_gross,
                liquidity_net: v.liquidity_net,
                initialized: v.initialized,
            },
        );
    }
    let mut coverage = HashSet::new();
    coverage.extend(state.tick_bitmap_coverage.iter().copied());
    UniswapV3Pool {
        address: pool,
        token_a: Token::new_with_decimals(state.token0.address, state.token0.decimals),
        token_b: Token::new_with_decimals(state.token1.address, state.token1.decimals),
        liquidity: state.liquidity,
        sqrt_price: state.sqrt_price,
        fee: state.fee,
        tick: state.tick,
        tick_spacing: state.tick_spacing,
        tick_bitmap,
        tick_bitmap_coverage: coverage,
        ticks,
    }
}

fn rebuild_agni(state: &V3State, pool: Address) -> AgniPool {
    let mut tick_bitmap = HashMap::new();
    for (k, v) in &state.tick_bitmap {
        tick_bitmap.insert(*k, *v);
    }
    let mut ticks = HashMap::new();
    for (k, v) in &state.ticks {
        ticks.insert(
            *k,
            AgniInfo {
                liquidity_gross: v.liquidity_gross,
                liquidity_net: v.liquidity_net,
                initialized: v.initialized,
            },
        );
    }
    let mut coverage = HashSet::new();
    coverage.extend(state.tick_bitmap_coverage.iter().copied());
    AgniPool {
        address: pool,
        token_a: Token::new_with_decimals(state.token0.address, state.token0.decimals),
        token_b: Token::new_with_decimals(state.token1.address, state.token1.decimals),
        liquidity: state.liquidity,
        sqrt_price: state.sqrt_price,
        fee: state.fee,
        tick: state.tick,
        tick_spacing: state.tick_spacing,
        tick_bitmap,
        tick_bitmap_coverage: coverage,
        ticks,
    }
}

fn rebuild_moe(state: &MoeState, pool: Address) -> Result<MoeLbPair, AMMError> {
    if state.hooks_parameters != B256::ZERO {
        return Err(AMMError::from(MoeError::IncompleteState)); // replaced below
    }
    let mut bins = HashMap::new();
    for b in &state.bins {
        bins.insert(
            b.id,
            BinReserve {
                reserve_x: b.reserve_x,
                reserve_y: b.reserve_y,
            },
        );
    }
    let snapshot = MoeSnapshot::new(
        MoeSlot0::from(&state.slot0),
        bins,
        state.queried_ranges.clone(),
        MoeSnapshotContext::new(state.block_hash, state.block_timestamp),
    )?;
    let mut pair = MoeLbPair::new(pool);
    pair.token_x = Token::new_with_decimals(state.token_x.address, state.token_x.decimals);
    pair.token_y = Token::new_with_decimals(state.token_y.address, state.token_y.decimals);
    pair.bin_step = state.bin_step;
    pair.install_snapshot(snapshot)?;
    Ok(pair)
}

fn assert_v2_case(pool: &UniswapV2Pool, case: &SwapCase) {
    let out = pool
        .simulate_swap(case.token_in, case.token_out, case.amount_in)
        .expect("v2 simulate_swap");
    assert_eq!(out, case.expected_amount_out, "v2 simulate_swap exact out");
    let mut mut_pool = pool.clone();
    let out_mut = mut_pool
        .simulate_swap_mut(case.token_in, case.token_out, case.amount_in)
        .expect("v2 simulate_swap_mut");
    assert_eq!(
        out_mut, case.expected_amount_out,
        "v2 simulate_swap_mut exact out"
    );
}

fn assert_v3_case(pool: &UniswapV3Pool, case: &SwapCase) {
    let out = pool
        .simulate_swap(case.token_in, case.token_out, case.amount_in)
        .expect("v3 simulate_swap");
    assert_eq!(out, case.expected_amount_out, "v3 simulate_swap exact out");
    let mut mut_pool = pool.clone();
    let out_mut = mut_pool
        .simulate_swap_mut(case.token_in, case.token_out, case.amount_in)
        .expect("v3 simulate_swap_mut");
    assert_eq!(out_mut, out, "v3 mutable/immutable agreement");
    assert_eq!(
        out_mut, case.expected_amount_out,
        "v3 simulate_swap_mut exact out"
    );
    if let Some(sqrt_after) = case.expected_sqrt_price_after {
        assert_eq!(mut_pool.sqrt_price, sqrt_after, "v3 sqrt after");
    }
    if let Some(true) = case.crosses_initialized_tick {
        assert!(
            mut_pool.liquidity != pool.liquidity,
            "expected initialized tick cross to change liquidity"
        );
    }
}

fn assert_agni_case(pool: &AgniPool, case: &SwapCase) {
    let out = pool
        .simulate_swap(case.token_in, case.token_out, case.amount_in)
        .expect("agni simulate_swap");
    assert_eq!(out, case.expected_amount_out, "agni simulate_swap exact out");
    let mut mut_pool = pool.clone();
    let out_mut = mut_pool
        .simulate_swap_mut(case.token_in, case.token_out, case.amount_in)
        .expect("agni simulate_swap_mut");
    assert_eq!(out_mut, out, "agni mutable/immutable agreement");
    assert_eq!(
        out_mut, case.expected_amount_out,
        "agni simulate_swap_mut exact out"
    );
    if let Some(sqrt_after) = case.expected_sqrt_price_after {
        assert_eq!(mut_pool.sqrt_price, sqrt_after, "agni sqrt after");
    }
    // Sequential second swap must start from committed state.
    let second = mut_pool
        .simulate_swap_mut(case.token_in, case.token_out, case.amount_in)
        .expect("agni sequential simulate_swap_mut");
    let mut fresh = pool.clone();
    let _ = fresh
        .simulate_swap_mut(case.token_in, case.token_out, case.amount_in)
        .expect("agni first hop on fresh");
    let second_from_fresh = fresh
        .simulate_swap_mut(case.token_in, case.token_out, case.amount_in)
        .expect("agni second hop on fresh");
    assert_eq!(
        second, second_from_fresh,
        "agni sequential swap uses committed state"
    );
    if let Some(true) = case.crosses_initialized_tick {
        assert!(
            mut_pool.liquidity != pool.liquidity,
            "expected initialized tick cross to change liquidity"
        );
    }
}

fn assert_moe_case(pair: &MoeLbPair, case: &SwapCase, timestamp: u64) {
    let swap_for_y = case
        .swap_for_y
        .expect("moe cases must set swap_for_y");
    let left = case
        .expected_amount_in_left
        .expect("moe cases must record amount_in_left");
    assert_eq!(left, U256::ZERO, "exact moe cases require full fill");
    let fee = case.expected_fee.expect("moe cases must record fee");
    assert!(fee > U256::ZERO, "moe fixture needs nonzero variable fee");

    let mut precise = pair.clone();
    let out_precise = precise
        .simulate_swap_precise(swap_for_y, case.amount_in, timestamp)
        .expect("moe simulate_swap_precise");
    assert_eq!(out_precise, case.expected_amount_out, "moe precise exact out");

    let out = pair
        .simulate_swap(case.token_in, case.token_out, case.amount_in)
        .expect("moe simulate_swap");
    assert_eq!(out, case.expected_amount_out, "moe trait simulate_swap");

    let mut mut_pair = pair.clone();
    let out_mut = mut_pair
        .simulate_swap_mut(case.token_in, case.token_out, case.amount_in)
        .expect("moe simulate_swap_mut");
    assert_eq!(out_mut, case.expected_amount_out, "moe simulate_swap_mut");

    if let Some(bins) = case.crosses_bins {
        assert!(bins >= 2, "moe exact case must cross >=2 bins");
    }
}

fn validate_fixture_hooks(fixture: &DifferentialFixture) -> Result<(), DifferentialError> {
    if let ProtocolState::Moe(state) = &fixture.state {
        if state.hooks_parameters != B256::ZERO {
            return Err(DifferentialError::UnsupportedHooks(state.hooks_parameters));
        }
    }
    Ok(())
}

fn run_fixture(fixture: &DifferentialFixture) {
    validate_fixture_hooks(fixture).expect("supported hooks only");
    match (&fixture.protocol, &fixture.state) {
        (ProtocolKind::UniswapV2, ProtocolState::UniswapV2(state)) => {
            let pool = rebuild_v2(state, fixture.pool);
            for case in &fixture.cases {
                assert_v2_case(&pool, case);
            }
        }
        (ProtocolKind::UniswapV3, ProtocolState::UniswapV3(state)) => {
            let pool = rebuild_v3(state, fixture.pool);
            for case in &fixture.cases {
                assert_v3_case(&pool, case);
            }
        }
        (ProtocolKind::Agni, ProtocolState::Agni(state)) => {
            let pool = rebuild_agni(state, fixture.pool);
            for case in &fixture.cases {
                assert_agni_case(&pool, case);
            }
        }
        (ProtocolKind::Moe, ProtocolState::Moe(state)) => {
            assert_eq!(state.hooks_parameters, B256::ZERO);
            let pair = rebuild_moe(state, fixture.pool).expect("rebuild moe");
            for price in &state.prices {
                let offchain = price_helper::get_price_from_id(price.id, state.bin_step)
                    .expect("price_helper");
                assert_eq!(offchain, price.price, "moe get_price_from_id exact");
            }
            for case in &fixture.cases {
                assert_moe_case(&pair, case, state.block_timestamp);
            }
        }
        _ => panic!("protocol/state mismatch in fixture"),
    }
}

#[test]
fn differential_fixtures_exact_offline() {
    let fixtures = load_all_fixtures().expect("load fixtures");
    assert!(
        !fixtures.is_empty(),
        "expected committed fixtures under {FIXTURE_DIR}"
    );

    let mut saw_v2_fees = BTreeSet::new();
    let mut saw_v3 = false;
    let mut saw_agni = false;
    let mut saw_moe = false;

    for (path, fixture) in &fixtures {
        run_fixture(fixture);
        match (&fixture.protocol, &fixture.state) {
            (ProtocolKind::UniswapV2, ProtocolState::UniswapV2(state)) => {
                saw_v2_fees.insert(state.fee);
                match &state.fee_provenance {
                    FeeProvenance::Getter { value, .. } => assert_eq!(*value, state.fee),
                    FeeProvenance::InferredVerified {
                        search_domain,
                        inputs,
                        value,
                        ..
                    } => {
                        assert_eq!(*value, state.fee);
                        assert_eq!(*search_domain, [0, V2_FEE_DOMAIN_END]);
                        assert!(inputs.len() >= 3);
                    }
                }
            }
            (ProtocolKind::UniswapV3, ProtocolState::UniswapV3(_)) => {
                saw_v3 = true;
                assert!(fixture.cases.iter().any(|c| c.crosses_initialized_tick == Some(true)));
            }
            (ProtocolKind::Agni, ProtocolState::Agni(_)) => {
                saw_agni = true;
                assert!(fixture.cases.iter().any(|c| c.crosses_initialized_tick == Some(true)));
            }
            (ProtocolKind::Moe, ProtocolState::Moe(state)) => {
                saw_moe = true;
                assert_eq!(state.hooks_parameters, B256::ZERO);
                assert!(fixture.cases.iter().any(|c| c.crosses_bins.unwrap_or(0) >= 2));
                assert!(fixture
                    .cases
                    .iter()
                    .all(|c| c.expected_amount_in_left == Some(U256::ZERO)));
                assert!(fixture
                    .cases
                    .iter()
                    .any(|c| c.expected_fee.unwrap_or_default() > U256::ZERO));
            }
            _ => panic!("bad fixture {}", path.display()),
        }
    }

    assert!(
        saw_v2_fees.len() >= 2,
        "need >=2 distinct V2 fees, got {saw_v2_fees:?}"
    );
    assert!(saw_v3, "missing uniswap_v3 fixture");
    assert!(saw_agni, "missing agni fixture");
    assert!(saw_moe, "missing moe fixture");
}

#[test]
fn differential_fail_closed_cases() {
    let fixtures = load_all_fixtures().expect("fixtures");
    let (_, v3) = fixtures
        .iter()
        .find(|(_, f)| f.protocol == ProtocolKind::UniswapV3)
        .expect("v3 fixture");
    let ProtocolState::UniswapV3(v3_state) = &v3.state else {
        panic!("bad v3 state");
    };
    let case = v3
        .cases
        .iter()
        .find(|c| c.crosses_initialized_tick == Some(true))
        .expect("cross-tick case");

    // Beyond coverage.
    let mut pool = rebuild_v3(v3_state, v3.pool);
    pool.tick_bitmap_coverage.clear();
    let err = pool
        .simulate_swap(case.token_in, case.token_out, case.amount_in)
        .expect_err("unsynced bitmap must fail");
    assert!(matches!(err, AMMError::IncompleteState));

    // Missing crossed tick record while leaving the bitmap bit set.
    let crossed = next_initialized_tick_on_path(v3_state, case.token_in);
    let mut incomplete_state = v3_state.clone();
    drop_initialized_tick(&mut incomplete_state, crossed);
    let pool = rebuild_v3(&incomplete_state, v3.pool);
    let err = pool
        .simulate_swap(case.token_in, case.token_out, case.amount_in)
        .expect_err("missing tick record must fail");
    assert!(matches!(err, AMMError::IncompleteState));
    let err_mut = {
        let mut p = rebuild_v3(&incomplete_state, v3.pool);
        p.simulate_swap_mut(case.token_in, case.token_out, case.amount_in)
            .expect_err("missing tick record mut")
    };
    assert!(matches!(err_mut, AMMError::IncompleteState));

    let (_, agni) = fixtures
        .iter()
        .find(|(_, f)| f.protocol == ProtocolKind::Agni)
        .expect("agni fixture");
    let ProtocolState::Agni(agni_state) = &agni.state else {
        panic!("bad agni");
    };
    let agni_case = &agni.cases[0];
    let mut agni_pool = rebuild_agni(agni_state, agni.pool);
    agni_pool.tick_bitmap_coverage.clear();
    let err = agni_pool
        .simulate_swap(agni_case.token_in, agni_case.token_out, agni_case.amount_in)
        .expect_err("agni unsynced bitmap");
    assert!(matches!(err, AMMError::IncompleteState));

    let (_, moe) = fixtures
        .iter()
        .find(|(_, f)| f.protocol == ProtocolKind::Moe)
        .expect("moe fixture");
    let ProtocolState::Moe(moe_state) = &moe.state else {
        panic!("bad moe");
    };
    let pair = rebuild_moe(moe_state, moe.pool).expect("moe rebuild");
    let moe_case = &moe.cases[0];

    // Timestamp mismatch.
    let err = pair
        .simulate_swap_with_timestamp(
            moe_case.token_in,
            moe_case.token_out,
            moe_case.amount_in,
            moe_state.block_timestamp + 1,
        )
        .expect_err("timestamp mismatch");
    assert!(matches!(
        err,
        AMMError::MoeError(MoeError::SnapshotTimestampMismatch { .. })
    ));

    // Unsupported token.
    let err = pair
        .simulate_swap(
            addr("0x00000000000000000000000000000000000000Aa"),
            moe_case.token_out,
            moe_case.amount_in,
        )
        .expect_err("unsupported token");
    assert!(matches!(err, AMMError::MoeError(MoeError::UnsupportedToken)));

    // Incomplete bin coverage: shrink queried ranges around active id only.
    let mut incomplete = moe_state.clone();
    let active = incomplete.slot0.active_id;
    incomplete.queried_ranges = vec![MoeBinRange::new(active, active)];
    incomplete.bins.retain(|b| b.id == active);
    let pair = rebuild_moe(&incomplete, moe.pool).expect("narrow moe");
    // Large input should walk outside the single active bin coverage.
    let huge = moe_case.amount_in.saturating_mul(U256::from(10_000u64));
    let err = pair
        .simulate_swap(moe_case.token_in, moe_case.token_out, huge)
        .expect_err("incomplete moe coverage");
    assert!(matches!(err, AMMError::MoeError(MoeError::IncompleteState)));

    // Unsupported hooks harness error.
    let mut hooks_fixture = moe.clone();
    if let ProtocolState::Moe(state) = &mut hooks_fixture.state {
        state.hooks_parameters = B256::repeat_byte(0x11);
    }
    let err = validate_fixture_hooks(&hooks_fixture).expect_err("hooks");
    assert!(matches!(err, DifferentialError::UnsupportedHooks(_)));
}

#[test]
fn differential_mutation_guards() {
    let fixtures = load_all_fixtures().expect("fixtures");

    // (a) V2 fee +/- 1
    let (_, v2) = fixtures
        .iter()
        .find(|(_, f)| f.protocol == ProtocolKind::UniswapV2)
        .expect("v2");
    let ProtocolState::UniswapV2(v2_state) = &v2.state else {
        panic!("bad v2");
    };
    let case = &v2.cases[0];
    let mut bad = v2_state.clone();
    bad.fee = bad.fee.saturating_add(1);
    let pool = rebuild_v2(&bad, v2.pool);
    let out = pool
        .simulate_swap(case.token_in, case.token_out, case.amount_in)
        .expect("mutated fee still quotes");
    assert_ne!(out, case.expected_amount_out, "fee+1 must change out");

    // (b) drop crossed tick from V3 while leaving its bitmap bit set
    let (_, v3) = fixtures
        .iter()
        .find(|(_, f)| f.protocol == ProtocolKind::UniswapV3)
        .expect("v3");
    let ProtocolState::UniswapV3(v3_state) = &v3.state else {
        panic!("bad v3");
    };
    let case = v3
        .cases
        .iter()
        .find(|c| c.crosses_initialized_tick == Some(true))
        .expect("cross tick");
    let crossed = next_initialized_tick_on_path(v3_state, case.token_in);
    let mut bad = v3_state.clone();
    drop_initialized_tick(&mut bad, crossed);
    let pool = rebuild_v3(&bad, v3.pool);
    let err = pool
        .simulate_swap(case.token_in, case.token_out, case.amount_in)
        .expect_err("dropping crossed tick fails closed");
    assert!(matches!(err, AMMError::IncompleteState));

    // (c) Moe bin reserve offset on a bin that supplies the output token.
    let (_, moe) = fixtures
        .iter()
        .find(|(_, f)| f.protocol == ProtocolKind::Moe)
        .expect("moe");
    let ProtocolState::Moe(moe_state) = &moe.state else {
        panic!("bad moe");
    };
    let case = &moe.cases[0];
    let swap_for_y = case.swap_for_y.expect("moe swap_for_y");
    let mut bad = moe_state.clone();
    let active = bad.slot0.active_id;
    // Y->X walks upward into X reserves; X->Y walks downward into Y reserves.
    let bin = if swap_for_y {
        bad.bins
            .iter_mut()
            .filter(|b| b.id <= active && b.reserve_y > 0)
            .max_by_key(|b| b.id)
            .expect("Y-supplying bin")
    } else {
        bad.bins
            .iter_mut()
            .filter(|b| b.id >= active && b.reserve_x > 0)
            .min_by_key(|b| b.id)
            .expect("X-supplying bin")
    };
    if swap_for_y {
        // Large enough to beat fee/rounding quantization on the quote.
        bin.reserve_y = bin.reserve_y / 2;
    } else {
        bin.reserve_x = bin.reserve_x / 2;
    }
    let pair = rebuild_moe(&bad, moe.pool).expect("mut moe");
    let out = pair
        .simulate_swap(case.token_in, case.token_out, case.amount_in)
        .expect("mut moe quote");
    assert_ne!(out, case.expected_amount_out, "bin offset must change out");

    // (d) expected out +1 wei
    let mut case_plus = case.clone();
    case_plus.expected_amount_out = case.expected_amount_out + U256::from(1u64);
    let pair = rebuild_moe(moe_state, moe.pool).expect("moe");
    let out = pair
        .simulate_swap(case.token_in, case.token_out, case.amount_in)
        .expect("moe");
    assert_ne!(
        out, case_plus.expected_amount_out,
        "+1 wei expected must fail equality"
    );
}

#[test]
fn differential_canonical_fixture_bytes_are_stable() {
    for (path, fixture) in load_all_fixtures().expect("fixtures") {
        let body = fs::read_to_string(&path).unwrap();
        let regenerated = canonical_json(&fixture).unwrap();
        assert_eq!(
            body, regenerated,
            "fixture {} is not in canonical form",
            path.display()
        );
        // No unordered object maps for tick/bin collections: they are arrays.
        let v: Value = serde_json::from_str(&body).unwrap();
        if let Some(state) = v.get("state") {
            if let Some(obj) = state.as_object() {
                for key in ["tick_bitmap", "ticks", "bins", "tick_bitmap_coverage"] {
                    if let Some(val) = obj.get(key) {
                        assert!(val.is_array(), "{key} must be array in {}", path.display());
                    }
                }
            }
        }
    }
}

// ---------------- live capture (ignored) ----------------

fn mantle_rpc_url() -> String {
    std::env::var("MANTLE_HTTP_URL").unwrap_or_else(|_| "https://rpc.mantle.xyz".to_string())
}

async fn init_provider() -> Result<impl Provider + Clone> {
    let url = mantle_rpc_url();
    Ok(ProviderBuilder::new().connect_http(url.parse()?))
}

async fn pinned_tip<P: Provider + Clone>(provider: P) -> Result<(SnapshotMeta, BlockId)> {
    // Optional pin for deterministic recapture of committed fixtures.
    if let Ok(hash_hex) = std::env::var("CAPTURE_BLOCK_HASH") {
        let block_hash = B256::from_str(hash_hex.trim())
            .map_err(|e| eyre!("invalid CAPTURE_BLOCK_HASH: {e}"))?;
        let block = provider
            .get_block_by_hash(block_hash)
            .await?
            .ok_or_else(|| eyre!("missing pinned block {block_hash}"))?;
        let header = block.header();
        if header.hash() != block_hash {
            return Err(DifferentialError::BlockHashChanged.into());
        }
        let meta = SnapshotMeta {
            chain_id: MANTLE_CHAIN_ID,
            block_number: header.number(),
            block_hash,
            block_timestamp: header.timestamp,
        };
        return Ok((meta, BlockId::hash_canonical(block_hash)));
    }

    let block_number = provider.get_block_number().await?;
    let block = provider
        .get_block_by_number(block_number.into())
        .await?
        .ok_or_else(|| eyre!("missing block {block_number}"))?;
    let header = block.header();
    let meta = SnapshotMeta {
        chain_id: MANTLE_CHAIN_ID,
        block_number,
        block_hash: header.hash(),
        block_timestamp: header.timestamp,
    };
    let id = BlockId::hash_canonical(meta.block_hash);
    // Verify hash stability immediately.
    let again = provider
        .get_block_by_number(block_number.into())
        .await?
        .ok_or_else(|| eyre!("missing block re-check"))?;
    if again.header().hash() != meta.block_hash {
        return Err(DifferentialError::BlockHashChanged.into());
    }
    Ok((meta, id))
}

async fn assert_hash_stable<P: Provider + Clone>(
    provider: P,
    meta: &SnapshotMeta,
) -> Result<()> {
    let block = provider
        .get_block_by_number(meta.block_number.into())
        .await?
        .ok_or_else(|| eyre!("missing block {}", meta.block_number))?;
    if block.header().hash() != meta.block_hash {
        return Err(DifferentialError::BlockHashChanged.into());
    }
    Ok(())
}

fn v2_amount_out(fee: usize, amount_in: U256, reserve_in: U256, reserve_out: U256) -> U256 {
    if amount_in.is_zero() || reserve_in.is_zero() || reserve_out.is_zero() {
        return U256::ZERO;
    }
    let fee_factor = U256::from(V2_FEE_DOMAIN_END - fee);
    let amount_in_with_fee = amount_in * fee_factor;
    let numerator = amount_in_with_fee * reserve_out;
    let denominator = reserve_in * U256::from(V2_FEE_DOMAIN_END) + amount_in_with_fee;
    numerator / denominator
}

fn infer_v2_fee(
    quoter: Address,
    inputs: &[(U256, U256)],
    reserve_in: U256,
    reserve_out: U256,
) -> Result<FeeProvenance> {
    let mut matches = Vec::new();
    for fee in 0..V2_FEE_DOMAIN_END {
        if inputs.iter().all(|(amount_in, expected)| {
            v2_amount_out(fee, *amount_in, reserve_in, reserve_out) == *expected
        }) {
            matches.push(fee);
        }
    }
    if matches.len() != 1 {
        return Err(DifferentialError::FeeProvenance(format!(
            "expected unique fee, found {} matches",
            matches.len()
        ))
        .into());
    }
    Ok(FeeProvenance::InferredVerified {
        quoter,
        method: "getAmountsOut(uint256,address[])".to_string(),
        search_domain: [0, V2_FEE_DOMAIN_END],
        inputs: inputs.iter().map(|(a, _)| *a).collect(),
        value: matches[0],
    })
}

async fn capture_v2<P: Provider + Clone>(
    provider: P,
    meta: &SnapshotMeta,
    block_id: BlockId,
    name: &str,
    pool: Address,
    factory: Address,
    router: Address,
    amounts: &[U256],
) -> Result<()> {
    let pair = IUniswapV2PairView::new(pool, provider.clone());
    let on_factory = pair.factory().call().block(block_id).await?;
    if on_factory != factory {
        return Err(DifferentialError::FactoryMismatch.into());
    }
    let token0 = pair.token0().call().block(block_id).await?;
    let token1 = pair.token1().call().block(block_id).await?;
    let reserves = pair.getReserves().call().block(block_id).await?;
    let dec0 = IERC20Meta::new(token0, provider.clone())
        .decimals()
        .call()
        .block(block_id)
        .await?;
    let dec1 = IERC20Meta::new(token1, provider.clone())
        .decimals()
        .call()
        .block(block_id)
        .await?;

    let router_c = IUniswapV2RouterView::new(router, provider.clone());
    let path = vec![token0, token1];
    let mut quoted = Vec::new();
    for amount in amounts {
        let outs = router_c
            .getAmountsOut(*amount, path.clone())
            .call()
            .block(block_id)
            .await?;
        if outs.len() < 2 {
            bail!("getAmountsOut returned short path");
        }
        quoted.push((*amount, outs[1]));
    }
    let reserve_in = U256::from(reserves.reserve0);
    let reserve_out = U256::from(reserves.reserve1);
    let provenance = infer_v2_fee(router, &quoted, reserve_in, reserve_out)?;
    let fee = match &provenance {
        FeeProvenance::InferredVerified { value, .. } => *value,
        FeeProvenance::Getter { value, .. } => *value,
    };

    let cases = quoted
        .into_iter()
        .map(|(amount_in, expected_amount_out)| SwapCase {
            amount_in,
            token_in: token0,
            token_out: token1,
            swap_for_y: None,
            expected_amount_out,
            expected_amount_in_left: None,
            expected_fee: None,
            expected_sqrt_price_after: None,
            expected_initialized_ticks_crossed: None,
            crosses_initialized_tick: None,
            crosses_bins: None,
        })
        .collect();

    let fixture = DifferentialFixture {
        schema_version: SCHEMA_VERSION,
        protocol: ProtocolKind::UniswapV2,
        snapshot: meta.clone(),
        pool,
        factory,
        truth_source: TruthSource {
            contract: router,
            method: "getAmountsOut(uint256,address[])".to_string(),
        },
        state: ProtocolState::UniswapV2(V2State {
            token0: TokenFixture {
                address: token0,
                decimals: dec0,
            },
            token1: TokenFixture {
                address: token1,
                decimals: dec1,
            },
            reserve0: reserves.reserve0.to::<u128>(),
            reserve1: reserves.reserve1.to::<u128>(),
            fee,
            fee_provenance: provenance,
        }),
        cases,
    };
    // Offline verify before writing.
    run_fixture(&fixture);
    write_fixture(name, &fixture)?;
    assert_hash_stable(provider, meta).await?;
    Ok(())
}

async fn quote_v3_v1<P: Provider + Clone>(
    provider: P,
    quoter: Address,
    block_id: BlockId,
    token_in: Address,
    token_out: Address,
    fee: u32,
    amount_in: U256,
) -> Result<U256> {
    let q = IQuoterV1::new(quoter, provider);
    let out = q
        .quoteExactInputSingle(token_in, token_out, U24::from(fee), amount_in, U160::ZERO)
        .call()
        .block(block_id)
        .await?;
    Ok(out)
}

async fn quote_v3_v2<P: Provider + Clone>(
    provider: P,
    quoter: Address,
    block_id: BlockId,
    token_in: Address,
    token_out: Address,
    fee: u32,
    amount_in: U256,
) -> Result<(U256, U256, u32)> {
    let q = IQuoterV2::new(quoter, provider);
    let out = q
        .quoteExactInputSingle(IQuoterV2::QuoteExactInputSingleParams {
            tokenIn: token_in,
            tokenOut: token_out,
            amountIn: amount_in,
            fee: U24::from(fee),
            sqrtPriceLimitX96: U160::ZERO,
        })
        .call()
        .block(block_id)
        .await?;
    Ok((
        out.amountOut,
        U256::from(out.sqrtPriceX96After),
        out.initializedTicksCrossed,
    ))
}


async fn load_univ3_pool<P: Provider + Clone>(
    provider: P,
    block_id: BlockId,
    pool_addr: Address,
) -> Result<UniswapV3Pool> {
    // FusionX and other Pancake-style V3 forks use uint32 feeProtocol in slot0.
    // The UniV3 batch decoder expects uint8 and reverts; Agni's decoder matches.
    let agni = AgniPool::new(pool_addr)
        .init_basic(block_id, provider)
        .await
        .map_err(|e| eyre!("Agni-compatible UniV3 load failed: {e}"))?;
    Ok(UniswapV3Pool {
        address: agni.address,
        token_a: agni.token_a,
        token_b: agni.token_b,
        liquidity: agni.liquidity,
        sqrt_price: agni.sqrt_price,
        fee: agni.fee,
        tick: agni.tick,
        tick_spacing: agni.tick_spacing,
        tick_bitmap: agni.tick_bitmap,
        tick_bitmap_coverage: agni.tick_bitmap_coverage,
        ticks: agni
            .ticks
            .into_iter()
            .map(|(k, v)| {
                (
                    k,
                    UniV3Info {
                        liquidity_gross: v.liquidity_gross,
                        liquidity_net: v.liquidity_net,
                        initialized: v.initialized,
                    },
                )
            })
            .collect(),
    })
}

async fn load_agni_pool<P: Provider + Clone>(
    provider: P,
    block_id: BlockId,
    pool_addr: Address,
) -> Result<AgniPool> {
    AgniPool::new(pool_addr)
        .init_basic(block_id, provider)
        .await
        .map_err(|e| eyre!("AgniPool::init_basic failed: {e}"))
}

async fn capture_v3_like<P: Provider + Clone>(
    provider: P,
    meta: &SnapshotMeta,
    block_id: BlockId,
    name: &str,
    protocol: ProtocolKind,
    pool_addr: Address,
    factory: Address,
    quoter_v1: Option<Address>,
    quoter_v2: Option<Address>,
    amount_in: U256,
) -> Result<()> {
    let view = IUniswapV3PoolView::new(pool_addr, provider.clone());
    eprintln!("v3 capture: factory check {name}");
    let on_factory = view.factory().call().block(block_id).await
        .map_err(|e| eyre!("factory() failed for {name}: {e}"))?;
    if on_factory != factory {
        return Err(DifferentialError::FactoryMismatch.into());
    }
    eprintln!("v3 capture: init pool {name}");

    let (state, token0, token1, fee) = match protocol {
        ProtocolKind::UniswapV3 => {
            let pool = load_univ3_pool(provider.clone(), block_id, pool_addr)
                .await
                .map_err(|e| eyre!("load_univ3_pool failed for {name}: {e}"))?;
            let state = V3State {
                token0: TokenFixture {
                    address: pool.token_a.address,
                    decimals: pool.token_a.decimals,
                },
                token1: TokenFixture {
                    address: pool.token_b.address,
                    decimals: pool.token_b.decimals,
                },
                fee: pool.fee,
                tick_spacing: pool.tick_spacing,
                sqrt_price: pool.sqrt_price,
                tick: pool.tick,
                liquidity: pool.liquidity,
                tick_bitmap: sorted_bitmap(&pool.tick_bitmap),
                tick_bitmap_coverage: sorted_coverage(&pool.tick_bitmap_coverage),
                ticks: sorted_ticks_uni(&pool.ticks),
            };
            let t0 = pool.token_a.address;
            let t1 = pool.token_b.address;
            let fee = pool.fee;
            (ProtocolState::UniswapV3(state), t0, t1, fee)
        }
        ProtocolKind::Agni => {
            let pool = load_agni_pool(provider.clone(), block_id, pool_addr)
                .await
                .map_err(|e| eyre!("load_agni_pool failed for {name}: {e}"))?;
            let state = V3State {
                token0: TokenFixture {
                    address: pool.token_a.address,
                    decimals: pool.token_a.decimals,
                },
                token1: TokenFixture {
                    address: pool.token_b.address,
                    decimals: pool.token_b.decimals,
                },
                fee: pool.fee,
                tick_spacing: pool.tick_spacing,
                sqrt_price: pool.sqrt_price,
                tick: pool.tick,
                liquidity: pool.liquidity,
                tick_bitmap: sorted_bitmap(&pool.tick_bitmap),
                tick_bitmap_coverage: sorted_coverage(&pool.tick_bitmap_coverage),
                ticks: sorted_ticks_agni(&pool.ticks),
            };
            let t0 = pool.token_a.address;
            let t1 = pool.token_b.address;
            let fee = pool.fee;
            (ProtocolState::Agni(state), t0, t1, fee)
        }
        _ => bail!("not a v3-like protocol"),
    };

    // Search directions and amount sizes for a fully matching cross-tick swap.
    let directions = [(token0, token1), (token1, token0)];
    let amounts = [
        amount_in,
        amount_in / U256::from(10u64),
        amount_in * U256::from(5u64),
        amount_in * U256::from(10u64),
        amount_in * U256::from(50u64),
        amount_in * U256::from(100u64),
        amount_in * U256::from(500u64),
        amount_in * U256::from(1_000u64),
        amount_in / U256::from(100u64),
        U256::from(10u64).pow(U256::from(16u64)),
        U256::from(10u64).pow(U256::from(17u64)),
        U256::from(10u64).pow(U256::from(18u64)),
        U256::from(10u64).pow(U256::from(19u64)),
        U256::from(10u64).pow(U256::from(20u64)),
        U256::from(10u64).pow(U256::from(21u64)),
        U256::from(10u64).pow(U256::from(22u64)),
    ];
    let mut chosen = None;
    'outer: for (token_in, token_out) in directions {
        for amt in amounts {
            if amt.is_zero() {
                continue;
            }
            let quote = if let Some(q2) = quoter_v2 {
                match quote_v3_v2(provider.clone(), q2, block_id, token_in, token_out, fee, amt).await
                {
                    Ok((out, sqrt_after, crossed)) => (out, Some(sqrt_after), Some(crossed)),
                    Err(_) => continue,
                }
            } else if let Some(q1) = quoter_v1 {
                match quote_v3_v1(provider.clone(), q1, block_id, token_in, token_out, fee, amt).await
                {
                    Ok(out) => (out, None, None),
                    Err(_) => continue,
                }
            } else {
                bail!("no quoter configured");
            };
            let (expected, sqrt_after, ticks_crossed) = quote;

            let (matches, crosses) = match &state {
                ProtocolState::UniswapV3(s) => {
                    let pool = rebuild_v3(s, pool_addr);
                    match pool.simulate_swap(token_in, token_out, amt) {
                        Ok(local) if local == expected => (
                            true,
                            crosses_initialized_tick_v3(s, pool_addr, token_in, token_out, amt),
                        ),
                        _ => (false, false),
                    }
                }
                ProtocolState::Agni(s) => {
                    let pool = rebuild_agni(s, pool_addr);
                    match pool.simulate_swap(token_in, token_out, amt) {
                        Ok(local) if local == expected => (
                            true,
                            crosses_initialized_tick_agni(s, pool_addr, token_in, token_out, amt),
                        ),
                        _ => (false, false),
                    }
                }
                _ => (false, false),
            };

            // Require a local initialized-tick cross. Quoter ticksCrossed alone is not
            // enough: some forks count differently, and fail-closed mutation guards need
            // the local path to actually consume a tick record.
            if matches && crosses {
                if let Some(crossed) = ticks_crossed {
                    assert!(
                        crossed > 0,
                        "local cross without quoter ticksCrossed for {name}"
                    );
                }
                chosen = Some(SwapCase {
                    amount_in: amt,
                    token_in,
                    token_out,
                    swap_for_y: None,
                    expected_amount_out: expected,
                    expected_amount_in_left: None,
                    expected_fee: None,
                    expected_sqrt_price_after: sqrt_after,
                    expected_initialized_ticks_crossed: ticks_crossed,
                    crosses_initialized_tick: Some(true),
                    crosses_bins: None,
                });
                break 'outer;
            }
        }
    }
    let case = chosen.ok_or_else(|| eyre!("no matching v3/agni direction for {name}"))?;
    if case.crosses_initialized_tick != Some(true) {
        bail!("failed to find cross-tick input for {name}");
    }

    let truth = if let Some(q2) = quoter_v2 {
        TruthSource {
            contract: q2,
            method: "quoteExactInputSingle((address,address,uint256,uint24,uint160))".into(),
        }
    } else {
        TruthSource {
            contract: quoter_v1.expect("q1"),
            method: "quoteExactInputSingle(address,address,uint24,uint256,uint160)".into(),
        }
    };

    let fixture = DifferentialFixture {
        schema_version: SCHEMA_VERSION,
        protocol,
        snapshot: meta.clone(),
        pool: pool_addr,
        factory,
        truth_source: truth,
        state,
        cases: vec![case],
    };
    run_fixture(&fixture);
    write_fixture(name, &fixture)?;
    assert_hash_stable(provider, meta).await?;
    Ok(())
}

async fn capture_moe<P: Provider + Clone>(
    provider: P,
    meta: &SnapshotMeta,
    block_id: BlockId,
    name: &str,
    pool: Address,
    factory: Address,
) -> Result<()> {
    let hooks = IMoeLBPairHooks::new(pool, provider.clone());
    let hooks_parameters = match hooks.getLBHooksParameters().call().block(block_id).await {
        Ok(v) => v,
        Err(_) => {
            bail!("getLBHooksParameters failed; pair unsupported");
        }
    };
    if hooks_parameters != B256::ZERO {
        return Err(DifferentialError::UnsupportedHooks(hooks_parameters).into());
    }
    // Optional factory getter; some pairs may not expose it.
    if let Ok(on_factory) = hooks.getFactory().call().block(block_id).await {
        if on_factory != factory {
            return Err(DifferentialError::FactoryMismatch.into());
        }
    }

    let mut amms = vec![amms::amms::amm::AMM::MoeLbPair(MoeLbPair::new(pool))];
    let context = MoeSnapshotContext::new(meta.block_hash, meta.block_timestamp);
    sync_moe_snapshots_batch(
        &mut amms,
        block_id,
        provider.clone(),
        context,
        MoeSnapshotSyncConfig {
            bins_radius: MOE_BINS_RADIUS,
            bins_per_request: 15,
        },
    )
    .await?;
    sync_token_decimals(&mut amms, provider.clone()).await?;
    let pair = match amms.into_iter().next() {
        Some(amms::amms::amm::AMM::MoeLbPair(p)) => p,
        _ => bail!("expected moe pair"),
    };
    let snapshot = pair
        .snapshot
        .clone()
        .ok_or_else(|| eyre!("missing moe snapshot"))?;

    // Search for a fully-filled cross-bin input with nonzero fee.
    let mut chosen = None;
    let candidates = [
        U256::from(10u64).pow(U256::from(15u64)),
        U256::from(10u64).pow(U256::from(16u64)),
        U256::from(10u64).pow(U256::from(17u64)),
        U256::from(5u64) * U256::from(10u64).pow(U256::from(16u64)),
        U256::from(2u64) * U256::from(10u64).pow(U256::from(17u64)),
    ];
    for swap_for_y in [true, false] {
        for amount_in in candidates {
            let amount_u128: u128 = match amount_in.try_into() {
                Ok(v) => v,
                Err(_) => continue,
            };
            let ret = hooks
                .getSwapOut(amount_u128, swap_for_y)
                .call()
                .block(block_id)
                .await?;
            let left = U256::from(ret.amountInLeft);
            let out = U256::from(ret.amountOut);
            let fee = U256::from(ret.fee);
            if left != U256::ZERO || out.is_zero() || fee.is_zero() {
                continue;
            }
            // Estimate bins crossed via local sim active id delta.
            let mut local = pair.clone();
            let before = local.active_id;
            let local_out = local.simulate_swap_precise(swap_for_y, amount_in, meta.block_timestamp)?;
            if local_out != out {
                continue;
            }
            let after = local.active_id;
            let bins = before.abs_diff(after) + 1;
            if bins < 2 {
                continue;
            }
            let (token_in, token_out) = if swap_for_y {
                (pair.token_x.address, pair.token_y.address)
            } else {
                (pair.token_y.address, pair.token_x.address)
            };
            chosen = Some(SwapCase {
                amount_in,
                token_in,
                token_out,
                swap_for_y: Some(swap_for_y),
                expected_amount_out: out,
                expected_amount_in_left: Some(left),
                expected_fee: Some(fee),
                expected_sqrt_price_after: None,
                expected_initialized_ticks_crossed: None,
                crosses_initialized_tick: None,
                crosses_bins: Some(bins),
            });
            break;
        }
        if chosen.is_some() {
            break;
        }
    }
    let case = chosen.ok_or_else(|| eyre!("no moe cross-bin full-fill input found"))?;

    let active = snapshot.slot0.active_id;
    let mut prices = Vec::new();
    for id in [active.saturating_sub(1), active, active.saturating_add(1)] {
        let onchain = hooks.getPriceFromId(U24::from(id)).call().block(block_id).await?;
        prices.push(MoePriceCase {
            id,
            price: onchain,
        });
    }

    let state = MoeState {
        token_x: TokenFixture {
            address: pair.token_x.address,
            decimals: pair.token_x.decimals,
        },
        token_y: TokenFixture {
            address: pair.token_y.address,
            decimals: pair.token_y.decimals,
        },
        bin_step: pair.bin_step,
        hooks_parameters,
        slot0: MoeSlot0Fixture::from(&snapshot.slot0),
        bins: sorted_bins(&snapshot.bins),
        queried_ranges: snapshot.queried_ranges.clone(),
        block_hash: snapshot.block_hash,
        block_timestamp: snapshot.block_timestamp,
        prices,
    };

    let fixture = DifferentialFixture {
        schema_version: SCHEMA_VERSION,
        protocol: ProtocolKind::Moe,
        snapshot: meta.clone(),
        pool,
        factory,
        truth_source: TruthSource {
            contract: pool,
            method: "getSwapOut(uint128,bool)".into(),
        },
        state: ProtocolState::Moe(state),
        cases: vec![case],
    };
    run_fixture(&fixture);
    write_fixture(name, &fixture)?;
    assert_hash_stable(provider, meta).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "live Mantle RPC capture; run with --ignored and MANTLE_HTTP_URL"]
async fn capture_differential_fixtures_from_mantle() -> Result<()> {
    let provider = init_provider().await?;
    let (meta, block_id) = pinned_tip(provider.clone()).await?;
    eprintln!(
        "capturing differential fixtures at block {} hash {:?}",
        meta.block_number, meta.block_hash
    );

    // Two V2 fees: Moe V1 (expected 300) and FusionX V2 (expected 200).
    let v2_amounts = [
        U256::from(1_000_000u64),
        U256::from(5_000_000u64),
        U256::from(1_234_567u64),
        U256::from(10_000_000u64),
    ];
    capture_v2(
        provider.clone(),
        &meta,
        block_id,
        "uniswap_v2_moe_v1_wmnt_usdt.json",
        addr(MOE_V1_PAIR),
        addr(MOE_V1_FACTORY),
        addr(MOE_V1_ROUTER),
        &v2_amounts,
    )
    .await?;
    capture_v2(
        provider.clone(),
        &meta,
        block_id,
        "uniswap_v2_fusionx_usdt_wmnt.json",
        addr(FUSIONX_V2_PAIR),
        addr(FUSIONX_V2_FACTORY),
        addr(FUSIONX_V2_ROUTER),
        &v2_amounts,
    )
    .await?;

    // UniV3-compatible FusionX pool via QuoterV2.
    capture_v3_like(
        provider.clone(),
        &meta,
        block_id,
        "uniswap_v3_fusionx_wmnt_weth_2500.json",
        ProtocolKind::UniswapV3,
        addr(FUSIONX_V3_POOL),
        addr(FUSIONX_V3_FACTORY),
        None,
        Some(addr(FUSIONX_V3_QUOTER)),
        U256::from(10u64).pow(U256::from(18u64)), // 1 WMNT
    )
    .await?;

    // Agni via QuoterV1 + QuoterV2 metadata when available.
    capture_v3_like(
        provider.clone(),
        &meta,
        block_id,
        "agni_usde_wmnt_2500.json",
        ProtocolKind::Agni,
        addr(AGNI_POOL),
        addr(AGNI_FACTORY),
        Some(addr(AGNI_QUOTER)),
        Some(addr(AGNI_QUOTER_V2)),
        U256::from(10u64).pow(U256::from(18u64)),
    )
    .await?;

    capture_moe(
        provider.clone(),
        &meta,
        block_id,
        "moe_fbtc_cmeth.json",
        addr(MOE_LB_PAIR),
        addr(MOE_LB_FACTORY),
    )
    .await?;

    assert_hash_stable(provider, &meta).await?;
    Ok(())
}
