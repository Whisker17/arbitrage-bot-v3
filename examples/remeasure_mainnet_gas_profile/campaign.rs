//! WHI-1422: route-class qualification campaign over the committed pool universe.
//!
//! Extends the WHI-557 harness (same synthetic executor, same override helpers from
//! [`amms::execution::mainnet_fork_harness`], same `eth_estimateGas` measurement) from
//! seven hand-picked 2-hop classes to every topology the committed universe
//! generates (2..=3 hops over v2/v3/moe), on real universe pools, at real crossing
//! buckets.
//!
//! Per sample:
//! 1. Pool state is read at the pinned block hash from `--rpc-url` (read-only).
//! 2. The route is simulated hop by hop with the production route-key function
//!    [`simulate_mixed_path_with_route_key`], so each sample's key (and crossing
//!    bucket) is exactly the key the live optimizer would compute.
//! 3. A settlement "lever" makes the round trip clear the executor's on-chain
//!    profit check (a real same-block round trip always loses to fees):
//!    - `v2_boost`: a V2 hop pays out more than its reserves imply, backed by a
//!      balance override (V2 has no crossings, so every V3/Moe hop stays real);
//!    - `displace`: the last pool (V3 or Moe) is moved to the state a real
//!      WMNT-in swap of size D would leave (price, tick, liquidity / bins, active
//!      id, computed by the pool's own simulation), so the route's last hop
//!      crosses the real ticks/bins back;
//!    - `inflate` (WHI-557): the last V3/Moe hop gets a favourable price plus
//!      inflated liquidity, so it crosses nothing. Kept only when every other
//!      hop also crosses nothing (the sample is then an honest bucket-0 sample).
//! 4. `eth_estimateGas` runs on `--measure-rpc-url` (a local anvil fork pinned at
//!    the same block). Successes become `fork_replay` samples; failures become
//!    `research_revert` records, which never qualify or set a limit.
//!
//! Only Agni-factory V3 pools are used: the WHI-501 executor implements only
//! `agniSwapCallback`, so other UniV3-family forks in the universe cannot be
//! executed by it at all (recorded in the campaign report).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Write as _;

use alloy::eips::BlockId;
use alloy::primitives::{address, keccak256, Address, Bytes, B256, U256};
use alloy::providers::{DynProvider, Provider};
use alloy::sol;
use alloy::sol_types::SolCall;
use eyre::{Context, Result};
use serde::Serialize;

use amms::amms::agni::{AgniFactory, AgniPool};
use amms::amms::amm::{AutomatedMarketMaker, AMM};
use amms::amms::Token;
use amms::amms::moe::{
    sync_moe_snapshots_batch, MoeFactory, MoeLbPair, MoeSnapshot, MoeSnapshotContext,
    MoeSnapshotSyncConfig,
};
use amms::amms::uniswap_v2::{UniswapV2Factory, UniswapV2Pool};
use amms::arbitrage::pathfinder::{ArbitragePath, PathHop};
use amms::execution::contract::IArbitrageExecutor;
use amms::execution::gas_profile::{
    generate_artifact, load_generator_config, load_samples_jsonl, write_artifact,
    BinCrossingBucket, ForcedUnsupportedRoute, GasSample, GeneratorConfig, ProfileStatus,
    ProtocolKind, RouteKey, SampleOutcome, SampleSource, TickCrossingBucket, VenueRef,
};
use amms::execution::mainnet_fork_harness::{
    self, admin_override, build_state_override, erc20_balance_override, moe_lb_bin_reserve_word,
    moe_lb_bin_slot, moe_lb_parameters_word_with_active_id, registered_pool_slots,
    v3_favorable_sqrt_price, v3_liquidity_override, v3_slot0_nudge,
    v3_slot0_with_price_and_tick, AccountStateOverride, MOE_LB_PARAMETERS_SLOT,
    REGISTERED_POOLS_BASE_SLOT, V3_SLOT0_SLOT, WMNT_BALANCE_SLOT,
};
use amms::service::protocol::V2_FEE;
use amms::service::simulate_mixed_path_with_route_key;
use amms::service::unified_universe::read_unified_csv;

use super::{Args, SYNTHETIC_CALLER, SYNTHETIC_EXECUTOR, WMNT};

sol! {
    #[sol(rpc)]
    interface IERC20Balance {
        function balanceOf(address owner) external view returns (uint256);
    }
}

/// The only V3 factory whose pools call `agniSwapCallback` (Agni Finance).
const AGNI_V3_FACTORY: Address = address!("25780dc8fc3cfbd75f33bfdab65e969b603b2035");
/// Tag prefix on every campaign sample's `notes`; re-runs replace exactly these.
pub const TAG: &str = "[whi-1422]";
const MAX_HOPS: usize = 3;
const HEADROOM: u128 = 1_000_000_000_000_000_000_000_000_000_000; // 1e30

pub struct Pin {
    pub block_number: u64,
    pub block_hash: B256,
    pub block_timestamp: u64,
    pub executor_code: Bytes,
    pub executor_code_hash: String,
}

/// Amount grid in milli-WMNT: 0.01 .. 3000 WMNT, roughly x3 per step.
const AMOUNT_GRID_MILLI: [u64; 12] = [
    10, 30, 100, 300, 1_000, 3_000, 10_000, 30_000, 100_000, 300_000, 1_000_000, 3_000_000,
];

fn wmnt_milli(milli: u64) -> U256 {
    U256::from(milli) * U256::from(10u64.pow(15))
}

#[derive(Clone, Debug)]
struct Hop {
    pool: Address,
    token_in: Address,
    token_out: Address,
    kind: ProtocolKind,
}

fn kind_of(protocol: &str) -> Option<ProtocolKind> {
    match protocol {
        "agni-v2" => Some(ProtocolKind::V2),
        "agni-v3" => Some(ProtocolKind::V3),
        "moe" => Some(ProtocolKind::Moe),
        _ => None,
    }
}

fn topo_label(kinds: &[ProtocolKind]) -> String {
    kinds.iter().map(|k| k.as_str()).collect::<Vec<_>>().join("+")
}

/// WMNT settlement cycles of 2..=MAX_HOPS hops, same rules as `PathFinder`:
/// no repeated intermediate token, no immediate same-pool reversal.
fn enumerate_cycles(edges: &[(Address, Address, Address, ProtocolKind)]) -> Vec<Vec<Hop>> {
    let mut adj: HashMap<Address, Vec<(Address, Address, ProtocolKind)>> = HashMap::new();
    for &(pool, t0, t1, kind) in edges {
        adj.entry(t0).or_default().push((t1, pool, kind));
        adj.entry(t1).or_default().push((t0, pool, kind));
    }
    fn dfs(
        tok: Address,
        path: &mut Vec<Hop>,
        seen: &mut BTreeSet<Address>,
        adj: &HashMap<Address, Vec<(Address, Address, ProtocolKind)>>,
        out: &mut Vec<Vec<Hop>>,
    ) {
        if path.len() >= MAX_HOPS {
            return;
        }
        for &(next, pool, kind) in adj.get(&tok).into_iter().flatten() {
            if path.last().is_some_and(|h| h.pool == pool) {
                continue;
            }
            let hop = Hop { pool, token_in: tok, token_out: next, kind };
            if next == WMNT {
                if !path.is_empty() {
                    let mut c = path.clone();
                    c.push(hop);
                    out.push(c);
                }
                continue;
            }
            if !seen.insert(next) {
                continue;
            }
            path.push(hop);
            dfs(next, path, seen, adj, out);
            path.pop();
            seen.remove(&next);
        }
    }
    let mut out = Vec::new();
    let mut seen = BTreeSet::from([WMNT]);
    dfs(WMNT, &mut Vec::new(), &mut seen, &adj, &mut out);
    out
}

fn upsert(
    map: &mut HashMap<Address, AccountStateOverride>,
    address: Address,
    diffs: Vec<(B256, B256)>,
) {
    map.entry(address)
        .or_insert_with(|| AccountStateOverride {
            address,
            code: None,
            balance: None,
            state_diff: Vec::new(),
        })
        .state_diff
        .extend(diffs);
}

struct Ctx {
    measure: DynProvider,
    pools: HashMap<Address, AMM>,
    /// Real storage words / balances at the pin, fetched lazily.
    slot0_words: HashMap<Address, B256>,
    moe_params_words: HashMap<Address, B256>,
    wmnt_balances: HashMap<Address, U256>,
    balance_slots: HashMap<Address, Option<u64>>,
    pin: Pin,
    rpc: DynProvider,
}

impl Ctx {
    fn block_id(&self) -> BlockId {
        BlockId::hash_canonical(self.pin.block_hash)
    }

    async fn storage(&self, addr: Address, slot: u64) -> Result<B256> {
        Ok(B256::from(
            self.rpc
                .get_storage_at(addr, U256::from(slot))
                .block_id(self.block_id())
                .await?,
        ))
    }

    async fn wmnt_balance(&mut self, holder: Address) -> Result<U256> {
        if let Some(b) = self.wmnt_balances.get(&holder) {
            return Ok(*b);
        }
        let b = IERC20Balance::new(WMNT, self.rpc.clone())
            .balanceOf(holder)
            .block(self.block_id())
            .call()
            .await?;
        self.wmnt_balances.insert(holder, b);
        Ok(b)
    }

    /// ERC20 `balanceOf` mapping slot, found (not assumed) by overriding each
    /// candidate slot with a magic value and reading `balanceOf` back on the fork.
    async fn balance_slot(&mut self, token: Address) -> Result<Option<u64>> {
        if let Some(s) = self.balance_slots.get(&token) {
            return Ok(*s);
        }
        let probe = address!("00000000000000000000000000000000deadbeef");
        let magic = U256::from(0x1422_1422_1422u64);
        let mut found = None;
        for slot in 0..=20u64 {
            let mut m = HashMap::new();
            upsert(&mut m, token, vec![erc20_balance_override(probe, magic, slot)]);
            let got = IERC20Balance::new(token, self.measure.clone())
                .balanceOf(probe)
                .state(build_state_override(m.into_values().collect()))
                .block(self.pin.block_number.into())
                .call()
                .await;
            if matches!(got, Ok(v) if v == magic) {
                found = Some(slot);
                break;
            }
        }
        self.balance_slots.insert(token, found);
        Ok(found)
    }
}

/// Production route key + per-hop outputs and crossing counts, hop by hop.
struct Sim {
    outs: Vec<U256>,
    key: RouteKey,
    crossings: Vec<u32>,
}

fn sim_hop(amm: &AMM, hop: &Hop, amount_in: U256, ts: u64) -> Result<(U256, RouteKey, u32)> {
    let path = ArbitragePath {
        hops: vec![PathHop {
            pool_address: hop.pool,
            token_in: hop.token_in,
            token_out: hop.token_out,
            fee_bps: 0,
        }],
    };
    let (_, out, key) = simulate_mixed_path_with_route_key(&path, &[amm.clone()], amount_in, ts)
        .map_err(|e| eyre::eyre!("simulate hop {:#x}: {e}", hop.pool))?;
    let crossings = match amm {
        AMM::AgniPool(p) => p
            .simulate_swap_with_crossing_evidence(hop.token_in, amount_in)?
            .crossing_count,
        AMM::MoeLbPair(p) => {
            p.simulate_swap_with_crossing_evidence(hop.token_in == p.token_x.address, amount_in, ts)?
                .crossing_count
        }
        _ => 0,
    };
    Ok((out, key, crossings))
}

/// V2 hops pay out slightly less than simulated (0.1%) so the real K check
/// holds even if an upstream hop lands a few wei short of its simulation.
fn v2_haircut(v: U256) -> U256 {
    v - v / U256::from(1000u64)
}

/// Simulate the whole route. `boost` (hop index, multiplier in bps) raises one V2
/// hop's payout; `last` replaces the last pool's state.
fn simulate(
    pools: &[AMM],
    hops: &[Hop],
    amount_in: U256,
    ts: u64,
    boost: Option<(usize, u32)>,
) -> Result<Sim> {
    let mut current = amount_in;
    let mut outs = Vec::new();
    let mut crossings = Vec::new();
    let mut v3 = None::<TickCrossingBucket>;
    let mut moe = None::<BinCrossingBucket>;
    for (i, (amm, hop)) in pools.iter().zip(hops).enumerate() {
        let (mut out, key, c) = sim_hop(amm, hop, current, ts)?;
        if hop.kind == ProtocolKind::V2 {
            out = v2_haircut(out);
            if let Some((b, bps)) = boost {
                if b == i {
                    out = out * U256::from(10_000 + bps as u64) / U256::from(10_000u64);
                }
            }
        }
        if let Some(t) = key.v3_tick_crossings {
            v3 = Some(v3.map_or(t, |x| x.max(t)));
        }
        if let Some(b) = key.moe_bin_crossings {
            moe = Some(moe.map_or(b, |x| x.max(b)));
        }
        outs.push(out);
        crossings.push(c);
        current = out;
    }
    let mut key = RouteKey::new(hops.iter().map(|h| h.kind).collect())?;
    if let Some(t) = v3 {
        key = key.with_v3_ticks(t);
    }
    if let Some(b) = moe {
        key = key.with_moe_bins(b);
    }
    Ok(Sim { outs, key, crossings })
}

/// Required round-trip margin over `amount_in` in the simulation (covers fee /
/// volatility drift between the simulation and on-chain execution).
fn target(amount_in: U256, bps: u64) -> U256 {
    amount_in * U256::from(10_000 + bps) / U256::from(10_000u64)
}

#[derive(Serialize)]
struct Attempt {
    topology: String,
    cycle: Vec<String>,
    amount_in_wmnt_milli: u64,
    lever: String,
    lever_param: String,
    route_key: Option<String>,
    per_hop_crossings: Vec<u32>,
    outcome: String,
    gas_used: Option<u64>,
    calldata_digest: Option<String>,
}

enum Lever {
    V2Boost(usize),
    Displace,
    Inflate,
}

impl Lever {
    fn name(&self) -> &'static str {
        match self {
            Lever::V2Boost(_) => "v2_boost",
            Lever::Displace => "displace",
            Lever::Inflate => "inflate",
        }
    }
}

struct Plan {
    sim: Sim,
    overrides: HashMap<Address, AccountStateOverride>,
    param: String,
}

/// Search the smallest lever strength that makes the simulated round trip clear
/// `target`, doubling from a small start; `None` when nothing reasonable works.
async fn plan_lever(
    ctx: &mut Ctx,
    hops: &[Hop],
    amount_in: U256,
    lever: &Lever,
) -> Result<Option<Plan>> {
    let ts = ctx.pin.block_timestamp;
    let base: Vec<AMM> = hops.iter().map(|h| ctx.pools[&h.pool].clone()).collect();
    let last = hops.len() - 1;
    match lever {
        Lever::V2Boost(i) => {
            let i = *i;
            let final_hop = i == last;
            let out_token = hops[i].token_out;
            let slot = if out_token == WMNT {
                Some(WMNT_BALANCE_SLOT)
            } else {
                ctx.balance_slot(out_token).await?
            };
            let Some(slot) = slot else { return Ok(None) };
            let mut bps = 50u32;
            while bps <= 200_000 {
                let sim = simulate(&base, hops, amount_in, ts, Some((i, bps)))?;
                if sim.outs[last] >= target(amount_in, if final_hop { 0 } else { 100 }) {
                    let mut ov = HashMap::new();
                    upsert(
                        &mut ov,
                        out_token,
                        vec![erc20_balance_override(
                            hops[i].pool,
                            sim.outs[i] + U256::from(HEADROOM),
                            slot,
                        )],
                    );
                    return Ok(Some(Plan { sim, overrides: ov, param: format!("boost_bps={bps}") }));
                }
                bps *= 2;
            }
            Ok(None)
        }
        Lever::Displace => {
            let pool = hops[last].pool;
            let x = hops[last].token_in;
            // V3 simulation is exact; Moe leaves room for the on-chain
            // volatility fee, which the spliced parameters word does not move.
            let margin_bps = if hops[last].kind == ProtocolKind::Moe { 300 } else { 100 };
            let mut d = amount_in / U256::from(4u64);
            for _ in 0..24 {
                let mut displaced = base[last].clone();
                let moved = match &mut displaced {
                    AMM::MoeLbPair(p) => displace_moe(p, x, d),
                    other => other.simulate_swap_mut(WMNT, x, d).map(|_| ()).map_err(Into::into),
                };
                if moved.is_err() {
                    return Ok(None);
                }
                let mut pools = base.clone();
                pools[last] = displaced.clone();
                let Ok(sim) = simulate(&pools, hops, amount_in, ts, None) else {
                    return Ok(None);
                };
                if sim.outs[last] >= target(amount_in, margin_bps) {
                    let mut ov = HashMap::new();
                    let bal = ctx.wmnt_balance(pool).await?;
                    upsert(&mut ov, WMNT, vec![erc20_balance_override(pool, bal + d, WMNT_BALANCE_SLOT)]);
                    match (&base[last], &displaced) {
                        (AMM::AgniPool(_), AMM::AgniPool(p)) => {
                            let word = slot0_word(ctx, pool).await?;
                            upsert(
                                &mut ov,
                                pool,
                                vec![
                                    (B256::from(U256::from(V3_SLOT0_SLOT)), v3_slot0_with_price_and_tick(word, p.sqrt_price, p.tick)),
                                    v3_liquidity_override(p.liquidity),
                                ],
                            );
                        }
                        (AMM::MoeLbPair(orig), AMM::MoeLbPair(p)) => {
                            let word = moe_params_word(ctx, pool).await?;
                            let mut diffs = vec![(
                                B256::from(U256::from(MOE_LB_PARAMETERS_SLOT)),
                                moe_lb_parameters_word_with_active_id(word, p.active_id),
                            )];
                            let ids: BTreeSet<u32> = orig.bins.keys().chain(p.bins.keys()).copied().collect();
                            for id in ids {
                                let (a, b) = (orig.bins.get(&id), p.bins.get(&id));
                                let changed = match (a, b) {
                                    (Some(a), Some(b)) => a.reserve_x != b.reserve_x || a.reserve_y != b.reserve_y,
                                    (None, None) => false,
                                    _ => true,
                                };
                                if changed {
                                    let (rx, ry) = b.map_or((0, 0), |b| (b.reserve_x, b.reserve_y));
                                    diffs.push((moe_lb_bin_slot(id), moe_lb_bin_reserve_word(rx, ry)));
                                }
                            }
                            upsert(&mut ov, pool, diffs);
                        }
                        _ => return Ok(None),
                    }
                    return Ok(Some(Plan { sim, overrides: ov, param: format!("displace_wmnt_wei={d}") }));
                }
                d *= U256::from(2u64);
            }
            Ok(None)
        }
        Lever::Inflate => {
            // Real hops except the last; the last crosses nothing by construction.
            let sim_real = match simulate(&base, hops, amount_in, ts, None) {
                Ok(s) => s,
                Err(_) => return Ok(None),
            };
            if sim_real.crossings[..last].iter().any(|c| *c != 0) || sim_real.outs[last].is_zero() {
                return Ok(None);
            }
            let pool = hops[last].pool;
            let x = hops[last].token_in;
            // Needed price ratio with margin; the real-price simulation already
            // pays real price impact, so this overshoots under inflated liquidity.
            let ratio = u256_f64(target(amount_in, 200)) / u256_f64(sim_real.outs[last]);
            let bal = ctx.wmnt_balance(pool).await?;
            let mut ov = HashMap::new();
            upsert(
                &mut ov,
                WMNT,
                vec![erc20_balance_override(pool, bal + amount_in * U256::from(4u64), WMNT_BALANCE_SLOT)],
            );
            let param;
            match &base[last] {
                AMM::AgniPool(p) => {
                    let zero_for_one = x == p.token_a.address;
                    let bps = ((ratio.max(1.0).sqrt() - 1.0) * 10_000.0).ceil() as u32 + 10;
                    let price = v3_favorable_sqrt_price(p.sqrt_price, zero_for_one, bps);
                    let word = slot0_word(ctx, pool).await?;
                    let Ok(new_slot0) = v3_slot0_nudge(word, price) else { return Ok(None) };
                    upsert(
                        &mut ov,
                        pool,
                        vec![(B256::from(U256::from(V3_SLOT0_SLOT)), new_slot0), v3_liquidity_override(u128::MAX / 2)],
                    );
                    param = format!("sqrt_price_nudge_bps={bps}");
                }
                AMM::MoeLbPair(p) => {
                    let swap_for_y = x == p.token_x.address;
                    let step = 1.0 + p.bin_step as f64 / 10_000.0;
                    let shift = (ratio.max(1.0).ln() / step.ln()).ceil() as u32 + 1;
                    let id = if swap_for_y { p.active_id + shift } else { p.active_id.saturating_sub(shift) };
                    let word = moe_params_word(ctx, pool).await?;
                    upsert(
                        &mut ov,
                        pool,
                        vec![
                            (B256::from(U256::from(MOE_LB_PARAMETERS_SLOT)), moe_lb_parameters_word_with_active_id(word, id)),
                            (moe_lb_bin_slot(id), moe_lb_bin_reserve_word(HEADROOM, HEADROOM)),
                        ],
                    );
                    param = format!("active_id_shift={shift}");
                }
                _ => return Ok(None),
            }
            let mut sim = sim_real;
            // The inflated last hop crosses nothing; its key bucket is then the
            // other hops' maximum, which is zero here.
            let kinds: Vec<ProtocolKind> = hops.iter().map(|h| h.kind).collect();
            let mut key = RouteKey::new(kinds.clone())?;
            if kinds.contains(&ProtocolKind::V3) {
                key = key.with_v3_ticks(TickCrossingBucket::Zero);
            }
            if kinds.contains(&ProtocolKind::Moe) {
                key = key.with_moe_bins(BinCrossingBucket::Zero);
            }
            sim.key = key;
            sim.crossings[last] = 0;
            Ok(Some(Plan { sim, overrides: ov, param }))
        }
    }
}

fn u256_f64(v: U256) -> f64 {
    v.to_string().parse::<f64>().unwrap_or(f64::MAX)
}

/// Moe displacement: the pair's own bin simulation, then a fresh snapshot of the
/// moved state so the route's last hop is simulated against it.
fn displace_moe(p: &mut MoeLbPair, x: Address, d: U256) -> Result<()> {
    let snap = p.snapshot.clone().ok_or_else(|| eyre::eyre!("moe pair without snapshot"))?;
    p.simulate_swap_mut(WMNT, x, d)?;
    let moved = MoeSnapshot::new(
        p.snapshot_slot0(),
        p.bins.clone(),
        snap.queried_ranges.clone(),
        MoeSnapshotContext::new(snap.block_hash, snap.block_timestamp),
    )?;
    p.install_snapshot(moved)?;
    Ok(())
}

async fn slot0_word(ctx: &mut Ctx, pool: Address) -> Result<B256> {
    if let Some(w) = ctx.slot0_words.get(&pool) {
        return Ok(*w);
    }
    let w = ctx.storage(pool, V3_SLOT0_SLOT).await?;
    ctx.slot0_words.insert(pool, w);
    Ok(w)
}

async fn moe_params_word(ctx: &mut Ctx, pool: Address) -> Result<B256> {
    if let Some(w) = ctx.moe_params_words.get(&pool) {
        return Ok(*w);
    }
    let w = ctx.storage(pool, MOE_LB_PARAMETERS_SLOT).await?;
    ctx.moe_params_words.insert(pool, w);
    Ok(w)
}

fn pool_type(kind: ProtocolKind) -> u8 {
    match kind {
        ProtocolKind::V2 => 0,
        ProtocolKind::V3 => 1,
        ProtocolKind::Moe => 2,
    }
}

fn pool_tokens(amm: &AMM) -> (Address, Address, u32) {
    match amm {
        AMM::UniswapV2Pool(p) => (p.token_a.address, p.token_b.address, 0),
        AMM::AgniPool(p) => (p.token_a.address, p.token_b.address, p.fee),
        AMM::MoeLbPair(p) => (p.token_x.address, p.token_y.address, 0),
        other => {
            let t = other.tokens();
            (t[0], t[1], 0)
        }
    }
}

/// Every structurally valid bucket variant of a topology (mirrors the crate's
/// `fee_scoring::route_key_candidates`, which is `pub(crate)`).
fn all_bucket_variants(kinds: &[ProtocolKind]) -> Result<Vec<RouteKey>> {
    let base = RouteKey::new(kinds.to_vec())?;
    let ticks: Vec<Option<TickCrossingBucket>> = if kinds.contains(&ProtocolKind::V3) {
        TickCrossingBucket::ALL.iter().copied().map(Some).collect()
    } else {
        vec![None]
    };
    let bins: Vec<Option<BinCrossingBucket>> = if kinds.contains(&ProtocolKind::Moe) {
        BinCrossingBucket::ALL.iter().copied().map(Some).collect()
    } else {
        vec![None]
    };
    let mut out = Vec::new();
    for t in &ticks {
        for b in &bins {
            let mut k = base.clone();
            k.v3_tick_crossings = *t;
            k.moe_bin_crossings = *b;
            out.push(k);
        }
    }
    Ok(out)
}

pub const OPEN_ENDED_REASON: &str = "open-ended crossing bucket (ticks=21+ and/or bins=11+): gas grows with every \
     extra crossing and a finite sample set cannot bound the limit, so this class stays Unsupported whatever \
     was measured (WHI-1422; see evidence/gas/whi-1422/REPORT.md)";

pub async fn run<P: Provider + Clone + 'static>(
    args: &Args,
    provider: P,
    measure: DynProvider,
    pin: Pin,
) -> Result<()> {
    let block_id = BlockId::hash_canonical(pin.block_hash);
    let rows = read_unified_csv(&args.universe)
        .map_err(|e| eyre::eyre!("read universe {}: {e}", args.universe.display()))?;
    let mut edges = Vec::new();
    let mut non_agni_v3 = 0usize;
    for r in &rows {
        let Some(kind) = kind_of(&r.protocol) else { continue };
        if kind == ProtocolKind::V3 && r.factory != AGNI_V3_FACTORY {
            non_agni_v3 += 1;
            continue;
        }
        edges.push((r.pool, r.token0, r.token1, kind));
    }
    let cycles = enumerate_cycles(&edges);
    let mut by_topo: BTreeMap<String, Vec<Vec<Hop>>> = BTreeMap::new();
    for c in cycles {
        let label = topo_label(&c.iter().map(|h| h.kind).collect::<Vec<_>>());
        by_topo.entry(label).or_default().push(c);
    }
    println!(
        "campaign: {} executable cycles over {} topologies ({} non-Agni V3 pools excluded: no agniSwapCallback)",
        by_topo.values().map(Vec::len).sum::<usize>(),
        by_topo.len(),
        non_agni_v3
    );

    // Deterministic, spread-out cycle choice per topology.
    let mut chosen: BTreeMap<String, Vec<Vec<Hop>>> = BTreeMap::new();
    for (label, mut cs) in by_topo {
        cs.sort_by_key(|c| c.iter().map(|h| format!("{:#x}", h.pool)).collect::<Vec<_>>().join(","));
        let k = args.cycles_per_topology.min(cs.len()).max(1);
        let picked = (0..k).map(|i| cs[i * cs.len() / k].clone()).collect();
        chosen.insert(label, picked);
    }

    // Sync every pool used, read-only at the pinned block hash.
    let used: BTreeSet<Address> = chosen.values().flatten().flatten().map(|h| h.pool).collect();
    let kind_by_pool: HashMap<Address, ProtocolKind> =
        chosen.values().flatten().flatten().map(|h| (h.pool, h.kind)).collect();
    let tokens_by_pool: HashMap<Address, (Address, Address)> =
        edges.iter().map(|&(p, t0, t1, _)| (p, (t0, t1))).collect();
    let (mut v2, mut v3, mut moe) = (Vec::new(), Vec::new(), Vec::new());
    for p in &used {
        match kind_by_pool[p] {
            ProtocolKind::V2 => v2.push(AMM::UniswapV2Pool(UniswapV2Pool::new(*p, V2_FEE))),
            ProtocolKind::V3 => {
                // Shell tokens as the bot's `build_amm` sets them; batch init does not.
                let (t0, t1) = tokens_by_pool[p];
                let mut pool = AgniPool::new(*p);
                pool.token_a = Token::new_with_decimals(t0.min(t1), 18);
                pool.token_b = Token::new_with_decimals(t0.max(t1), 18);
                v3.push(AMM::AgniPool(pool))
            }
            ProtocolKind::Moe => moe.push(AMM::MoeLbPair(MoeLbPair::new(*p))),
        }
    }
    println!("syncing {} v2 / {} v3 / {} moe pools at block {}", v2.len(), v3.len(), moe.len(), pin.block_number);
    let mut synced = UniswapV2Factory::batch_init_pools(v2, block_id, provider.clone()).await?;
    synced.extend(AgniFactory::batch_init_pools(v3, block_id, provider.clone()).await?);
    let mut moe = MoeFactory::batch_init_pools(moe, block_id, provider.clone()).await?;
    sync_moe_snapshots_batch(
        &mut moe,
        block_id,
        provider.clone(),
        MoeSnapshotContext::new(pin.block_hash, pin.block_timestamp),
        MoeSnapshotSyncConfig::default(),
    )
    .await
    .context("sync moe snapshots")?;
    synced.extend(moe);
    let mut pools: HashMap<Address, AMM> = HashMap::new();
    for a in synced {
        let (t0, t1, _) = pool_tokens(&a);
        let (r0, r1) = tokens_by_pool[&a.address()];
        if BTreeSet::from([t0, t1]) != BTreeSet::from([r0, r1]) {
            println!("dropping {:#x}: synced tokens {t0:#x}/{t1:#x} != universe row", a.address());
            continue;
        }
        pools.insert(a.address(), a);
    }

    let rpc = provider.clone().erased();
    let mut ctx = Ctx {
        measure: measure.clone(),
        pools,
        slot0_words: HashMap::new(),
        moe_params_words: HashMap::new(),
        wmnt_balances: HashMap::new(),
        balance_slots: HashMap::new(),
        pin,
        rpc,
    };

    std::fs::create_dir_all(&args.evidence_out)?;
    let attempts_path = args.evidence_out.join("attempts.jsonl");
    let mut attempts_file = std::fs::File::create(&attempts_path)?;
    let mut samples: Vec<GasSample> = Vec::new();
    let deadline = U256::from(ctx.pin.block_timestamp) + U256::from(3600u64);

    for (label, cs) in &chosen {
        for hops in cs {
            if hops.iter().any(|h| !ctx.pools.contains_key(&h.pool)) {
                println!("{label}: skipping cycle with an unsynced/unquotable pool");
                continue;
            }
            let v2_idx = hops.iter().rposition(|h| h.kind == ProtocolKind::V2);
            let levers: Vec<Lever> = match v2_idx {
                Some(i) => vec![Lever::V2Boost(i)],
                None => vec![Lever::Inflate, Lever::Displace],
            };
            for milli in AMOUNT_GRID_MILLI {
                let amount_in = wmnt_milli(milli);
                for lever in &levers {
                    let plan = plan_lever(&mut ctx, hops, amount_in, lever).await;
                    let mut attempt = Attempt {
                        topology: label.clone(),
                        cycle: hops.iter().map(|h| format!("{:#x}", h.pool)).collect(),
                        amount_in_wmnt_milli: milli,
                        lever: lever.name().into(),
                        lever_param: String::new(),
                        route_key: None,
                        per_hop_crossings: vec![],
                        outcome: String::new(),
                        gas_used: None,
                        calldata_digest: None,
                    };
                    let plan = match plan {
                        Ok(Some(p)) => p,
                        Ok(None) => {
                            attempt.outcome = "skipped: lever cannot settle this amount".into();
                            writeln!(attempts_file, "{}", serde_json::to_string(&attempt)?)?;
                            continue;
                        }
                        Err(e) => {
                            attempt.outcome = format!("skipped: {e:#}");
                            writeln!(attempts_file, "{}", serde_json::to_string(&attempt)?)?;
                            continue;
                        }
                    };
                    attempt.lever_param = plan.param.clone();
                    attempt.route_key = Some(plan.sim.key.key_string());
                    attempt.per_hop_crossings = plan.sim.crossings.clone();

                    let mut ov = plan.overrides;
                    upsert(&mut ov, SYNTHETIC_EXECUTOR, vec![admin_override(SYNTHETIC_CALLER)]);
                    upsert(
                        &mut ov,
                        WMNT,
                        vec![erc20_balance_override(SYNTHETIC_EXECUTOR, amount_in, WMNT_BALANCE_SLOT)],
                    );
                    for h in hops {
                        let (t0, t1, fee) = pool_tokens(&ctx.pools[&h.pool]);
                        upsert(
                            &mut ov,
                            SYNTHETIC_EXECUTOR,
                            registered_pool_slots(h.pool, pool_type(h.kind), t0, t1, fee, REGISTERED_POOLS_BASE_SLOT).to_vec(),
                        );
                    }
                    ov.get_mut(&SYNTHETIC_EXECUTOR).expect("executor override").code =
                        Some(ctx.pin.executor_code.clone());

                    let path: Vec<Address> = std::iter::once(WMNT).chain(hops.iter().map(|h| h.token_out)).collect();
                    let pools_v: Vec<Address> = hops.iter().map(|h| h.pool).collect();
                    let types: Vec<u8> = hops.iter().map(|h| pool_type(h.kind)).collect();
                    let amounts_out: Vec<U256> = hops
                        .iter()
                        .zip(&plan.sim.outs)
                        .map(|(h, o)| if h.kind == ProtocolKind::V2 { *o } else { U256::ZERO })
                        .collect();
                    let calldata = IArbitrageExecutor::executeArbitrageCall {
                        amountIn: amount_in,
                        path: path.clone(),
                        pools: pools_v.clone(),
                        poolTypes: types.clone(),
                        amountsOut: amounts_out.clone(),
                        minProfit: U256::ZERO,
                        deadline,
                    }
                    .abi_encode();
                    let digest = format!("{}", keccak256(&calldata));
                    attempt.calldata_digest = Some(digest.clone());

                    let result = mainnet_fork_harness::measure_route(
                        measure.clone(),
                        SYNTHETIC_EXECUTOR,
                        SYNTHETIC_CALLER,
                        amount_in,
                        path,
                        pools_v,
                        types,
                        amounts_out,
                        U256::ZERO,
                        deadline,
                        build_state_override(ov.into_values().collect()),
                        ctx.pin.block_number,
                    )
                    .await;
                    let notes = format!(
                        "{TAG} topology={label} lever={} {} amount_in_wmnt_milli={milli} per_hop_crossings={:?} \
                         cycle={}",
                        lever.name(),
                        plan.param,
                        plan.sim.crossings,
                        attempt.cycle.join(">")
                    );
                    let (source, gas, outcome) = match &result {
                        Ok(g) => (SampleSource::ForkReplay, *g, SampleOutcome::Success),
                        Err(_) => (SampleSource::ResearchRevert, 0, SampleOutcome::Reverted),
                    };
                    attempt.outcome = match &result {
                        Ok(_) => "success".into(),
                        Err(e) => format!("reverted: {e:#}"),
                    };
                    attempt.gas_used = result.as_ref().ok().copied();
                    println!(
                        "{label} {milli}mWMNT {} -> {} {}",
                        lever.name(),
                        plan.sim.key.key_string(),
                        attempt.outcome.chars().take(120).collect::<String>()
                    );
                    writeln!(attempts_file, "{}", serde_json::to_string(&attempt)?)?;
                    samples.push(GasSample {
                        route_key: plan.sim.key.clone(),
                        gas_used: gas,
                        source,
                        executor_code_hash: ctx.pin.executor_code_hash.clone(),
                        chain_id: args.chain_id,
                        block_number: ctx.pin.block_number,
                        block_hash: Some(ctx.pin.block_hash.to_string()),
                        tx_hash: None,
                        effective_gas_price_wei: None,
                        base_fee_wei: None,
                        block_gas_limit: None,
                        inclusion_latency_blocks: None,
                        notes: Some(if result.is_err() {
                            format!("{notes} reverted: {}", attempt.outcome)
                        } else {
                            notes
                        }),
                        venues: Some(
                            hops.iter()
                                .map(|h| VenueRef { protocol: h.kind, pool: format!("{:#x}", h.pool) })
                                .collect(),
                        ),
                        calldata_digest: Some(digest),
                        outcome: Some(outcome),
                    });
                }
            }
        }
    }

    // Merge: replace earlier campaign samples, keep everything else (WHI-557 etc).
    let samples_path = args.out.join("samples.jsonl");
    let mut merged: Vec<GasSample> = load_samples_jsonl(&samples_path)?
        .into_iter()
        .filter(|s| !s.notes.as_deref().unwrap_or("").starts_with(TAG))
        .collect();
    merged.extend(samples.iter().cloned());
    let mut body = String::new();
    for s in &merged {
        body.push_str(&serde_json::to_string(s)?);
        body.push('\n');
    }

    // Config: every bucket variant of every 2..=3-hop topology is an explicit
    // active class; open-ended buckets are held Unsupported with a reason.
    let mut config: GeneratorConfig = load_generator_config(&args.config)?;
    let mut active: BTreeSet<RouteKey> = config.active_route_classes.iter().cloned().collect();
    let kinds = [ProtocolKind::V2, ProtocolKind::V3, ProtocolKind::Moe];
    for len in 2..=MAX_HOPS {
        for mut n in 0..kinds.len().pow(len as u32) {
            let mut topo = Vec::new();
            for _ in 0..len {
                topo.push(kinds[n % 3]);
                n /= 3;
            }
            active.extend(all_bucket_variants(&topo)?);
        }
    }
    config.active_route_classes = active.into_iter().collect();
    config.unsupported_route_classes = config
        .active_route_classes
        .iter()
        .filter(|k| {
            k.v3_tick_crossings == Some(TickCrossingBucket::High)
                || k.moe_bin_crossings == Some(BinCrossingBucket::High)
        })
        .map(|k| ForcedUnsupportedRoute { route_key: k.clone(), reason: OPEN_ENDED_REASON.into() })
        .collect();
    let artifact = generate_artifact(&config, &merged).context("generate artifact")?;
    let approved: Vec<String> = artifact
        .profiles
        .iter()
        .filter(|p| p.status == ProfileStatus::Approved)
        .map(|p| p.route_key.key_string())
        .collect();
    println!(
        "campaign samples={} (success={}) artifact profiles={} approved={} content_digest={}",
        samples.len(),
        samples.iter().filter(|s| s.source == SampleSource::ForkReplay).count(),
        artifact.profiles.len(),
        approved.len(),
        artifact.content_digest
    );
    for a in &approved {
        println!("  approved {a}");
    }

    if args.dry_run {
        println!("--dry-run: not writing samples, config or profile");
        return Ok(());
    }
    std::fs::write(&samples_path, body)?;
    std::fs::write(&args.config, serde_json::to_string_pretty(&config)? + "\n")?;
    write_artifact(&args.profile_out, &artifact)?;
    println!("wrote {} / {} / {}", samples_path.display(), args.config.display(), args.profile_out.display());
    Ok(())
}
