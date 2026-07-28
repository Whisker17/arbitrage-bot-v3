//! WHI-557: state-override construction for measuring `ArbitrageExecutor`'s real gas
//! cost against pinned Mantle mainnet state via `eth_call`/`eth_estimateGas` — no
//! anvil subprocess, no broadcast transaction. Pure slot/word arithmetic lives here so
//! it is unit-testable against the real `storageLayout` artifact and hand-verified
//! `cast`/`eth_getStorageAt` fixtures without any network access; the one live-RPC
//! piece (`measure_route`) is a thin wrapper that assembles these overrides and calls
//! `estimate_gas`.

use alloy::contract::Error as ContractError;
use alloy::primitives::map::AddressHashMap;
use alloy::primitives::{keccak256, Address, Bytes, B256, U256};
use alloy::providers::Provider;
use alloy::rpc::types::state::{AccountOverride, StateOverride};

use super::contract::IArbitrageExecutor;

/// `ArbitrageExecutor.admin` storage slot — confirmed against
/// `contracts/executor/artifacts/ArbitrageExecutor.full.json`'s `storageLayout`
/// (`{"label":"admin","slot":"0","offset":0,"type":"t_address"}`).
const ADMIN_SLOT: u64 = 0;

/// `ArbitrageExecutor.registeredPools` mapping base slot — confirmed against the same
/// `storageLayout` (`{"label":"registeredPools","slot":"3",...}`). `pub` (not
/// `pub(crate)`) because `examples/remeasure_mainnet_gas_profile.rs` — a separate
/// compilation unit linking against this crate like any external consumer — passes it
/// explicitly to [`registered_pool_slots`]; `execution::shadow` instead derives the
/// same value from the compiled `storageLayout` via `shadow::slots::registered_pools_base_slot`.
pub const REGISTERED_POOLS_BASE_SLOT: u64 = 3;

/// WMNT (`0x78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8`) `balanceOf` mapping slot.
/// Empirically discovered (not assumed) by brute-forcing candidate slots 0..20 with a
/// magic-value `--override-state-diff` probe against live Mantle mainnet and observing
/// which candidate's override was reflected back by a real `balanceOf` call:
///
/// ```text
/// cast call 0x78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8 "balanceOf(address)(uint256)" \
///   0x00000000000000000000000000000000deadbeef --rpc-url https://rpc.mantle.xyz \
///   --override-state-diff 0x78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8:<slot(holder,0)>:<magic>
/// ```
///
/// Candidate 0 matched exactly (a canonical WETH9-style layout, balances as the first
/// declared storage variable).
pub const WMNT_BALANCE_SLOT: u64 = 0;

/// Moe Liquidity Book `_bins` mapping base slot. Empirically discovered against the
/// real WMNT/USDT binStep=15 pair `0xf6C9020c9E915808481757779EDB53DACEaE2415` by
/// brute-forcing candidate base slots 0..40, computing
/// `keccak256(pad32(activeId) ++ pad32(candidate))` for each, reading the raw storage
/// word at that key, and comparing against the pair's own `getBin(activeId)` result.
/// Candidate 6 matched exactly: real `getBin(8369590)` returned
/// `(binReserveX=8988900694668660, binReserveY=2109)`; the raw word at
/// `keccak256(pad32(8369590) ++ pad32(6))` decodes to the same values under
/// `(reserveX: low 128 bits, reserveY: high 128 bits)`.
pub const MOE_LB_BINS_MAPPING_SLOT: u64 = 6;

/// Moe Liquidity Book `_parameters` packed word slot (holds `activeId` among other
/// packed fields — base factor, filter/decay periods, volatility accumulator/reference,
/// `idReference`, `timeOfLastUpdate`, etc.). Empirically discovered against the same
/// real pair `0xf6C9020c9E915808481757779EDB53DACEaE2415`: raw storage slot 3 read
/// `0x7fb5b6000b006a633d807fb5b6000000000055730271001d4c138825801e1a0a`, whose top 24
/// bits (`0x7fb5b6` = 8369590) exactly match the pair's real `getActiveId()`. Confirmed
/// by splicing the top 24 bits to `0x7fb5b7` via a `stateOverride` and observing
/// `getActiveId()` return `8369591` — an exact, unambiguous match (a second, unrelated
/// field — `idReference` — happens to carry the same pre-override value at bits
/// 152..176, which is expected: `idReference` tracks `activeId` and commonly matches it
/// absent recent volatility).
pub const MOE_LB_PARAMETERS_SLOT: u64 = 3;

/// Agni V3 (canonical UniswapV3Pool fork) `slot0` storage slot. Empirically confirmed
/// against the live WMNT/USDT fee=500 pool `0xD08C50F7E69e9aeb2867DefF4A8053d9A855e26A`:
/// `cast storage <pool> 0`'s low 160 bits and next 24 bits decode to exactly the
/// `sqrtPriceX96`/`tick` returned by the pool's own `slot0()` view call at the same
/// block.
pub const V3_SLOT0_SLOT: u64 = 0;

/// Agni V3 `liquidity` storage slot. The canonical UniswapV3Pool layout puts this at
/// slot 4, but Agni's fork inserts one extra word before it — empirically confirmed
/// against the same live pool: `cast storage <pool> 5`'s low 128 bits decode to exactly
/// the value returned by the pool's own `liquidity()` view call (`3311946261459528`),
/// while slot 4 does not match.
pub const V3_LIQUIDITY_SLOT: u64 = 5;

/// `pub(crate)` so `execution::shadow` can build state-override words from
/// programmatically-derived storage layout slots using the exact same arithmetic,
/// instead of duplicating it.
pub(crate) fn pad_address(addr: Address) -> B256 {
    let mut word = [0u8; 32];
    word[12..].copy_from_slice(addr.as_slice());
    B256::from(word)
}

pub(crate) fn pad_u64(value: u64) -> B256 {
    B256::from(U256::from(value))
}

/// Solidity mapping slot formula: `keccak256(pad32(key) ++ pad32(base_slot))`.
pub(crate) fn mapping_slot(key: B256, base_slot: B256) -> B256 {
    let mut preimage = [0u8; 64];
    preimage[..32].copy_from_slice(key.as_slice());
    preimage[32..].copy_from_slice(base_slot.as_slice());
    keccak256(preimage)
}

/// Overrides `admin` (slot 0) to `caller`, so a synthetic measurement call from
/// `caller` passes `onlyHotExecutor`'s `msg.sender == admin` bypass without needing a
/// separate `isHotExecutor` mapping entry (`ArbitrageExecutor.sol`'s
/// `onlyHotExecutor` modifier: `!isHotExecutor[msg.sender] && msg.sender != admin`).
pub fn admin_override(caller: Address) -> (B256, B256) {
    (pad_u64(ADMIN_SLOT), pad_address(caller))
}

/// Computes the two storage words for `registeredPools[pool]`
/// (`RegisteredPool { poolType: uint8, token0: address, token1: address, fee: uint24,
/// enabled: bool }`, packed per the real `storageLayout`):
///
/// - word 0: `poolType` at byte offset 0 (low byte) | `token0` at byte offset 1..21
/// - word 1: `token1` at byte offset 0..20 | `fee` at byte offset 20..23 | `enabled`
///   at byte offset 23
///
/// (Solidity's "offset" counts bytes from the *low* end of the 32-byte word.)
///
/// `base_slot` is the caller's responsibility: this module's own callers (tests,
/// `examples/remeasure_mainnet_gas_profile.rs`) pass the doc-verified
/// [`REGISTERED_POOLS_BASE_SLOT`] constant directly, while `execution::shadow` derives
/// it from the compiled `storageLayout` via `shadow::slots::registered_pools_base_slot`
/// instead of trusting the hand-written constant.
pub fn registered_pool_slots(
    pool: Address,
    pool_type: u8,
    token0: Address,
    token1: Address,
    fee: u32,
    base_slot: u64,
) -> [(B256, B256); 2] {
    let base = mapping_slot(pad_address(pool), pad_u64(base_slot));
    let base_int = U256::from_be_bytes(base.0);
    let slot0 = base;
    let slot1 = B256::from(base_int + U256::from(1u8));

    let mut word0 = [0u8; 32];
    word0[11..31].copy_from_slice(token0.as_slice());
    word0[31] = pool_type;

    let mut word1 = [0u8; 32];
    word1[12..32].copy_from_slice(token1.as_slice());
    let fee_bytes = fee.to_be_bytes(); // u32 -> 4 bytes, low 3 used for uint24
    word1[9..12].copy_from_slice(&fee_bytes[1..4]);
    word1[8] = 1; // enabled = true

    [(slot0, B256::from(word0)), (slot1, B256::from(word1))]
}

/// Computes the ERC20 `balanceOf` mapping slot/value pair for `holder` on a token
/// whose balance mapping lives at `balance_slot` (e.g. [`WMNT_BALANCE_SLOT`]). Reused
/// both for funding the synthetic executor and for the V2 profit-invariant nudge
/// (inflating a V2 pool's real WMNT balance so its `swap()` can pay out an
/// `amountsOut[i]` larger than its stale cached reserve would otherwise allow — see
/// module-level docs on `ArbitrageExecutor::_swapV2`'s caller-supplied `expectedOut`).
pub fn erc20_balance_override(holder: Address, amount: U256, balance_slot: u64) -> (B256, B256) {
    let slot = mapping_slot(pad_address(holder), pad_u64(balance_slot));
    (slot, B256::from(amount))
}

/// Splices a new `sqrtPriceX96` into the low 160 bits of a real `slot0` word, leaving
/// the upper bits (`tick`, `observationIndex`, `observationCardinality`,
/// `observationCardinalityNext`, `feeProtocol`, `unlocked`) untouched. This is the V3
/// profit-invariant nudge: V3 pools compute swap output from `sqrtPriceX96`/tick math
/// and ignore any caller-supplied `amountsOut`, so the only lever is the price itself.
/// `current_slot0` must be the pool's real, freshly-fetched `slot0` storage word —
/// splicing (rather than a bare low-160-bit write) is required so `unlocked` stays
/// `true` and the swap doesn't revert on the reentrancy lock.
pub fn v3_slot0_with_sqrt_price(current_slot0: B256, new_sqrt_price_x96: U256) -> B256 {
    let mask: U256 = (U256::from(1u8) << 160) - U256::from(1u8);
    let current = U256::from_be_bytes(current_slot0.0);
    let spliced = (current & !mask) | (new_sqrt_price_x96 & mask);
    B256::from(spliced)
}

/// Splices a new `sqrtPriceX96` *and* its corresponding `tick` into a real `slot0`
/// word, leaving `observationIndex`/`observationCardinality`/
/// `observationCardinalityNext`/`feeProtocol`/`unlocked` untouched. Unlike
/// [`v3_slot0_with_sqrt_price`] (which leaves `tick` stale relative to the nudged
/// price), this keeps `tickBitmap.nextInitializedTickWithinOneWord` lookups
/// self-consistent — required because a real V3 pool's tick bitmap can be densely
/// initialized near the current price (confirmed empirically on Agni's live WMNT/USDT
/// fee=500 pool: every ~10-raw-tick bracket initialized, current tick only ~2 raw ticks
/// from the next boundary), so a stale tick risks an internally-inconsistent swap-step
/// calculation even when [`v3_liquidity_override`] makes the swap's own price impact
/// negligible.
pub fn v3_slot0_with_price_and_tick(
    current_slot0: B256,
    new_sqrt_price_x96: U256,
    new_tick: i32,
) -> B256 {
    let price_mask: U256 = (U256::from(1u8) << 160) - U256::from(1u8);
    let tick_mask: U256 = U256::from(0xFFFFFFu32) << 160;
    let current = U256::from_be_bytes(current_slot0.0);

    // int24 tick, twos-complement, packed into its 24-bit field.
    let tick_bits = U256::from(new_tick as u32 & 0x00FF_FFFF) << 160;

    let spliced = (current & !price_mask & !tick_mask)
        | (new_sqrt_price_x96 & price_mask)
        | (tick_bits & tick_mask);
    B256::from(spliced)
}

/// Convenience wrapper combining [`v3_slot0_with_price_and_tick`] with
/// `uniswap_v3_math::tick_math::get_tick_at_sqrt_ratio`, so callers only need to supply
/// the desired price and the function keeps `tick` self-consistent automatically. Fails
/// only if `new_sqrt_price_x96` falls outside V3's valid `[MIN_SQRT_RATIO,
/// MAX_SQRT_RATIO)` range.
pub fn v3_slot0_nudge(
    current_slot0: B256,
    new_sqrt_price_x96: U256,
) -> Result<B256, uniswap_v3_math::error::UniswapV3MathError> {
    let tick = uniswap_v3_math::tick_math::get_tick_at_sqrt_ratio(new_sqrt_price_x96)?;
    Ok(v3_slot0_with_price_and_tick(
        current_slot0,
        new_sqrt_price_x96,
        tick,
    ))
}

/// Overrides a V3 pool's real `liquidity` word with a massively larger value, making a
/// swap's actual price impact negligible regardless of the real tick bitmap's density —
/// so a [`v3_slot0_nudge`] of any reasonable magnitude can't cross an initialized tick
/// during execution. The full 32-byte word is set directly (not spliced): empirically
/// confirmed the upper 128 bits at [`V3_LIQUIDITY_SLOT`] are unused padding on Agni's
/// fork (a full-word `cast storage` read decoded exactly to `liquidity()`'s low-128-bit
/// value with nothing else present).
pub fn v3_liquidity_override(inflated_liquidity: u128) -> (B256, B256) {
    (
        pad_u64(V3_LIQUIDITY_SLOT),
        B256::from(U256::from(inflated_liquidity)),
    )
}

/// Nudges a real `sqrtPriceX96` by `bps` basis points in the direction favorable to a
/// swap of the given direction. Mirrors
/// [`moe_lb_parameters_word_with_active_id`]'s directional convention: `zero_for_one =
/// true` (token0 in, token1 out) benefits from a *higher* price (`sqrtPriceX96`
/// represents `sqrt(token1/token0)`, so a higher price means each unit of token0 buys
/// more token1); `zero_for_one = false` benefits from a *lower* price.
pub fn v3_favorable_sqrt_price(current_sqrt_price_x96: U256, zero_for_one: bool, bps: u32) -> U256 {
    if zero_for_one {
        nudge_up_bps(current_sqrt_price_x96, bps)
    } else {
        nudge_down_bps(current_sqrt_price_x96, bps)
    }
}

/// Splices a new `activeId` into the top 24 bits of a real `_parameters` word, leaving
/// every other packed field (base factor, volatility accumulator/reference,
/// `idReference`, `timeOfLastUpdate`, etc.) untouched. This is the Moe LB
/// profit-invariant nudge: `get_amounts`'s per-bin swap math computes `amount_out` as
/// `amount_in_net * price` (or `/ price`) where `price = get_price_from_id(active_id,
/// bin_step)` depends purely on `(active_id, bin_step)` — inflating the active bin's
/// *reserves* alone (see [`moe_lb_bin_reserve_word`]) cannot raise `amount_out` above
/// this price-implied ceiling, only remove a too-small-reserve cap below it. Moving
/// `active_id` by a small amount (e.g. 1) shifts price by roughly one `bin_step`
/// (typically a few bps) and requires no reserve inflation to become the binding lever.
/// Direction: for a `swapForY` (X→Y) hop, a *higher* active_id raises price and thus
/// `amount_out`; for a Y→X hop, a *lower* active_id lowers price and raises `amount_out`
/// (mirrors `v3_slot0_with_sqrt_price`'s zeroForOne/oneForZero direction convention).
/// `current_parameters` must be the pair's real, freshly-fetched `_parameters` word —
/// splicing (not a bare top-24-bit write) is required so `idReference`,
/// `timeOfLastUpdate`, and the fee-control fields stay intact.
pub fn moe_lb_parameters_word_with_active_id(current_parameters: B256, new_active_id: u32) -> B256 {
    let mask: U256 = U256::from(0xFFFFFFu32) << 232;
    let current = U256::from_be_bytes(current_parameters.0);
    let new_id_bits = U256::from(new_active_id) << 232;
    let spliced = (current & !mask) | (new_id_bits & mask);
    B256::from(spliced)
}

/// Computes the Moe LB `_bins[active_id]` storage slot key.
pub fn moe_lb_bin_slot(active_id: u32) -> B256 {
    mapping_slot(pad_u64(active_id as u64), pad_u64(MOE_LB_BINS_MAPPING_SLOT))
}

/// Packs a Moe LB bin's reserves into its raw storage word
/// (`reserveX`: low 128 bits, `reserveY`: high 128 bits — confirmed exactly against the
/// live pair's own `getBin()` result, see [`MOE_LB_BINS_MAPPING_SLOT`]). This is the
/// Moe profit-invariant nudge: the active bin's reserves are inflated so the pair's
/// internal bin math can pay out enough of the output token.
pub fn moe_lb_bin_reserve_word(reserve_x: u128, reserve_y: u128) -> B256 {
    let value = (U256::from(reserve_y) << 128) | U256::from(reserve_x);
    B256::from(value)
}

/// Conservative (deliberately under-estimated) constant-product output for a
/// *non-final* hop, where the swap only needs to succeed against the pool's real,
/// unmodified reserves (no profit requirement applies until the last hop). Ignores the
/// pool's actual fee bps and halves the fee-free ideal, so it clears the pool's
/// internal K-invariant check regardless of the real fee tier (FusionX 200bps, Moe
/// 300bps, etc.) without needing to know it ahead of time. Not used for V3/Moe hops,
/// whose pools ignore the caller-supplied `amountsOut` entirely.
pub fn v2_conservative_amount_out(amount_in: U256, reserve_in: U256, reserve_out: U256) -> U256 {
    let numerator = amount_in * reserve_out;
    let denominator = (reserve_in + amount_in) * U256::from(2u8);
    numerator / denominator
}

/// Bumps a `U256` value up by `bps` basis points: `value * (10_000 + bps) / 10_000`.
pub fn nudge_up_bps(value: U256, bps: u32) -> U256 {
    value + (value * U256::from(bps)) / U256::from(10_000u32)
}

/// Bumps a `U256` value down by `bps` basis points: `value * (10_000 - bps) / 10_000`.
pub fn nudge_down_bps(value: U256, bps: u32) -> U256 {
    value - (value * U256::from(bps)) / U256::from(10_000u32)
}

/// Bumps a `u128` value up by `bps` basis points, for Moe LB bin reserves.
pub fn nudge_u128_up_bps(value: u128, bps: u32) -> u128 {
    value + (value * bps as u128) / 10_000
}

/// `UniswapV2Pair.swap()`'s K-invariant check evaluates
/// `balance0Adjusted * balance1Adjusted >= reserve0 * reserve1 * 1000^2` against the
/// pool's REAL, unmodified `getReserves()` values (a separate storage slot on the pool
/// itself, never touched by [`erc20_balance_override`]). A payout-side balance override
/// of only `amount_out * 2` leaves the post-payout balance at just `amount_out` — tiny
/// next to a real pool's actual reserve (hundreds of WMNT / hundreds of thousands of
/// USDT) — which understates the LHS and fails K. The override must instead swamp the
/// check: many orders of magnitude larger than any realistic on-chain reserve, so the
/// post-payout balance alone dominates the product regardless of the real reserves.
const V2_BALANCE_OVERRIDE_HEADROOM: u128 = 1_000_000_000_000_000_000_000_000_000_000; // 1e30

/// The final hop's WMNT-settlement overrides: the amount the executor must receive
/// back (`amount_in_leg1 + 1` wei — trivial, deliberate 1-wei profit so
/// `balanceAfter >= balanceBefore + minProfit` clears with `minProfit = 0`) and a
/// generously inflated WMNT balance for the pool/venue paying it out (see
/// [`V2_BALANCE_OVERRIDE_HEADROOM`]), so the payout succeeds and clears the K-invariant
/// regardless of the pool's real (unmodified) cached reserves. Only meaningful for a V2
/// final hop, whose `pool.swap()` call actually moves real ERC20 balance; V3/Moe final
/// hops use [`v3_slot0_with_sqrt_price`] / [`nudge_u128_up_bps`] on their own reserve
/// state instead and don't consult this pool-balance override.
pub fn v2_final_hop_settlement(amount_in_leg1: U256) -> (U256, U256) {
    let amount_out = amount_in_leg1 + U256::from(1u8);
    let pool_balance_override = amount_out + U256::from(V2_BALANCE_OVERRIDE_HEADROOM);
    (amount_out, pool_balance_override)
}

/// General V2 profit-invariant nudge, usable for *any* V2 hop (final or non-final) that
/// needs to hand a downstream hop more than its real, unmodified reserves would pay
/// out. Exploits `UniswapV2Pair.swap()`'s real K-invariant check comparing the LIVE
/// post-transfer `balanceOf()` against the CACHED `reserve0*reserve1`: overriding the
/// pool's real output-token balance (via [`erc20_balance_override`]) makes that check
/// trivially satisfied for any `amount_out` up to nearly the full inflated balance,
/// regardless of the pool's real (unmodified, smaller) cached reserves. This
/// generalizes [`v2_final_hop_settlement`] (the final-hop special case targeting an
/// exact 1-wei round-trip profit in the *same* token as `amount_in`) to a hop whose
/// output token differs from `amount_in`'s token — `amount_out` is a modest `extra_bps`
/// boost over the real fee-free ideal, generous enough to leave a downstream hop its own
/// real fee headroom to clear, but not tied to any exact fee-adjusted formula (the
/// [`V2_BALANCE_OVERRIDE_HEADROOM`]-backed balance override makes precision
/// unnecessary, exactly as it does for the final-hop case).
pub fn v2_generous_amount_out(
    amount_in: U256,
    reserve_in: U256,
    reserve_out: U256,
    extra_bps: u32,
) -> (U256, U256) {
    let fee_free_ideal = (amount_in * reserve_out) / (reserve_in + amount_in);
    let amount_out = nudge_up_bps(fee_free_ideal, extra_bps);
    let pool_balance_override = amount_out + U256::from(V2_BALANCE_OVERRIDE_HEADROOM);
    (amount_out, pool_balance_override)
}

/// One account's worth of overrides to assemble into a [`StateOverride`].
#[derive(Debug, Clone, Default)]
pub struct AccountStateOverride {
    pub address: Address,
    pub code: Option<Bytes>,
    pub balance: Option<U256>,
    pub state_diff: Vec<(B256, B256)>,
}

/// Pure assembly of per-account overrides into alloy's `StateOverride` map, using
/// `state_diff` (never `state`) so unrelated storage on each account is left alone.
pub fn build_state_override(accounts: Vec<AccountStateOverride>) -> StateOverride {
    let mut map: AddressHashMap<AccountOverride> =
        AddressHashMap::with_capacity_and_hasher(accounts.len(), Default::default());
    for account in accounts {
        let mut over = AccountOverride::default();
        if let Some(code) = account.code {
            over = over.with_code(code);
        }
        if let Some(balance) = account.balance {
            over = over.with_balance(balance);
        }
        if !account.state_diff.is_empty() {
            over = over.with_state_diff(account.state_diff);
        }
        map.insert(account.address, over);
    }
    map
}

/// Estimates the gas cost of `executeArbitrage(...)` at `executor` under
/// `state_override`, pinned to `block_number`. The caller is responsible for building
/// `state_override` (executor code + admin/registeredPools overrides, funding, and any
/// profit-invariant nudge) via the pure helpers above.
#[allow(clippy::too_many_arguments)]
pub async fn measure_route<P: Provider + Clone>(
    provider: P,
    executor: Address,
    caller: Address,
    amount_in: U256,
    token_path: Vec<Address>,
    pool_addresses: Vec<Address>,
    pool_types: Vec<u8>,
    amounts_out: Vec<U256>,
    min_profit: U256,
    deadline: U256,
    state_override: StateOverride,
    block_number: u64,
) -> Result<u64, ContractError> {
    let contract = IArbitrageExecutor::new(executor, provider);
    let call = contract
        .executeArbitrage(
            amount_in,
            token_path,
            pool_addresses,
            pool_types,
            amounts_out,
            min_profit,
            deadline,
        )
        .from(caller)
        .state(state_override)
        .block(block_number.into());
    call.estimate_gas().await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(hex: &str) -> Address {
        hex.parse().unwrap()
    }

    #[test]
    fn admin_override_targets_slot_zero_and_pads_the_address() {
        let caller = addr("0x00000000000000000000000000000000DeaDBeef");
        let (slot, value) = admin_override(caller);
        assert_eq!(slot, B256::ZERO);
        assert_eq!(value, pad_address(caller));
    }

    #[test]
    fn erc20_balance_override_matches_the_empirically_discovered_wmnt_slot() {
        // Fixture independently reproduced via:
        //   cast keccak 0x00000000000000000000000000000000000000000000000000000000deadbeef0000000000000000000000000000000000000000000000000000000000000000
        let holder = addr("0x00000000000000000000000000000000DeaDBeef");
        let expected_slot: B256 =
            "0x49361c85d50c86f43fdb7ff3f85f3cac00c47bda054279b6563a02b9b214ccf4"
                .parse()
                .unwrap();
        let (slot, value) = erc20_balance_override(holder, U256::from(42u64), WMNT_BALANCE_SLOT);
        assert_eq!(slot, expected_slot);
        assert_eq!(value, B256::from(U256::from(42u64)));
    }

    #[test]
    fn registered_pool_slots_matches_the_real_storage_layout_packing() {
        // Fixture: FusionX V2 pool address, base slot 3 (confirmed via
        // ArbitrageExecutor.full.json's storageLayout), independently reproduced via:
        //   cast keccak <pad32(pool) ++ pad32(3)>
        let pool = addr("0x3e5922cd0cec71dc2d60ec8b36aa4c05b7c1672f");
        let token0 = addr("0x201eba5cc46d216ce6dc03f6a759e8e766e956ae"); // USDT
        let token1 = addr("0x78c1b0c915c4faa5fffa6cabf0219da63d7f4cb8"); // WMNT
        let expected_slot0: B256 =
            "0x6a12a5758f94f61435968633224da27ece3f452e504e4535a52ee023220ee7f1"
                .parse()
                .unwrap();

        let [(slot0, word0), (slot1, word1)] =
            registered_pool_slots(pool, 0, token0, token1, 0, REGISTERED_POOLS_BASE_SLOT);

        assert_eq!(slot0, expected_slot0);
        let slot1_int = U256::from_be_bytes(expected_slot0.0) + U256::from(1u8);
        assert_eq!(slot1, B256::from(slot1_int));

        // word0: poolType (byte 31) | token0 (bytes 11..31)
        assert_eq!(word0.0[31], 0u8);
        assert_eq!(&word0.0[11..31], token0.as_slice());
        assert_eq!(&word0.0[..11], &[0u8; 11]);

        // word1: enabled (byte 8) | fee (bytes 9..12) | token1 (bytes 12..32)
        assert_eq!(word1.0[8], 1u8);
        assert_eq!(&word1.0[9..12], &[0u8, 0u8, 0u8]);
        assert_eq!(&word1.0[12..32], token1.as_slice());
    }

    #[test]
    fn registered_pool_slots_packs_a_nonzero_fee_into_the_uint24_range() {
        let pool = addr("0xd08c50f7e69e9aeb2867deff4a8053d9a855e26a");
        let token0 = addr("0x201eba5cc46d216ce6dc03f6a759e8e766e956ae");
        let token1 = addr("0x78c1b0c915c4faa5fffa6cabf0219da63d7f4cb8");

        let [(_, _), (_, word1)] =
            registered_pool_slots(pool, 1, token0, token1, 500, REGISTERED_POOLS_BASE_SLOT);

        // fee = 500 = 0x0001F4, occupying bytes 9..12 (uint24, big-endian).
        assert_eq!(&word1.0[9..12], &[0x00, 0x01, 0xf4]);
    }

    #[test]
    fn v3_slot0_with_sqrt_price_preserves_the_upper_bits() {
        // Real Agni V3 pool `0xd08c50f7e69e9aeb2867deff4a8053d9a855e26a` slot0 word,
        // confirmed via `cast storage` against live Mantle mainnet: decodes to
        // sqrtPriceX96=123415148464859037259230048648473303, tick=285188, matching the
        // pool's own `slot0()` view call exactly.
        let real_word: B256 = "0x000000000100010000045a04000000000017c4d62bf97c25c89f75b9f920aad7"
            .parse()
            .unwrap();
        let new_sqrt_price = U256::from(200_000_000_000_000_000_000_000_000_000_000u128);

        let spliced = v3_slot0_with_sqrt_price(real_word, new_sqrt_price);

        let mask: U256 = (U256::from(1u8) << 160) - U256::from(1u8);
        let spliced_int = U256::from_be_bytes(spliced.0);
        assert_eq!(spliced_int & mask, new_sqrt_price & mask);
        let real_int = U256::from_be_bytes(real_word.0);
        assert_eq!(spliced_int & !mask, real_int & !mask);
    }

    #[test]
    fn v3_slot0_with_price_and_tick_splices_both_and_preserves_the_rest() {
        // Same real Agni V3 slot0 fixture as `v3_slot0_with_sqrt_price_preserves_the_upper_bits`.
        let real_word: B256 = "0x000000000100010000045a04000000000017c4d62bf97c25c89f75b9f920aad7"
            .parse()
            .unwrap();
        let new_sqrt_price = U256::from(200_000_000_000_000_000_000_000_000_000_000u128);
        let new_tick: i32 = 300_000;

        let spliced = v3_slot0_with_price_and_tick(real_word, new_sqrt_price, new_tick);
        let spliced_int = U256::from_be_bytes(spliced.0);

        let price_mask: U256 = (U256::from(1u8) << 160) - U256::from(1u8);
        assert_eq!(spliced_int & price_mask, new_sqrt_price & price_mask);

        let tick_mask: U256 = U256::from(0xFFFFFFu32) << 160;
        assert_eq!(
            (spliced_int & tick_mask) >> 160,
            U256::from(new_tick as u32)
        );

        // Bits above the tick field (observationIndex, cardinality, feeProtocol,
        // unlocked) must be untouched.
        let real_int = U256::from_be_bytes(real_word.0);
        let upper_mask: U256 = !(price_mask | tick_mask);
        assert_eq!(spliced_int & upper_mask, real_int & upper_mask);
    }

    #[test]
    fn v3_slot0_with_price_and_tick_round_trips_a_negative_tick() {
        let real_word = B256::ZERO;
        let new_tick: i32 = -285_188;

        let spliced = v3_slot0_with_price_and_tick(real_word, U256::ZERO, new_tick);
        let spliced_int = U256::from_be_bytes(spliced.0);
        let tick_mask: U256 = U256::from(0xFFFFFFu32) << 160;
        let recovered_bits = u32::try_from((spliced_int & tick_mask) >> 160).unwrap();
        // Sign-extend the 24-bit twos-complement field back to i32.
        let recovered = if recovered_bits & 0x0080_0000 != 0 {
            (recovered_bits | 0xFF00_0000) as i32
        } else {
            recovered_bits as i32
        };
        assert_eq!(recovered, new_tick);
    }

    #[test]
    fn v3_slot0_nudge_computes_a_self_consistent_tick_for_the_new_price() {
        let real_word: B256 = "0x000000000100010000045a04000000000017c4d62bf97c25c89f75b9f920aad7"
            .parse()
            .unwrap();
        let current_sqrt_price =
            U256::from_be_bytes(real_word.0) & ((U256::from(1u8) << 160) - U256::from(1u8));
        let new_sqrt_price = nudge_up_bps(current_sqrt_price, 200);

        let spliced = v3_slot0_nudge(real_word, new_sqrt_price).expect("valid sqrt price range");
        let expected_tick =
            uniswap_v3_math::tick_math::get_tick_at_sqrt_ratio(new_sqrt_price).unwrap();

        let manual = v3_slot0_with_price_and_tick(real_word, new_sqrt_price, expected_tick);
        assert_eq!(spliced, manual);
    }

    #[test]
    fn v3_liquidity_override_targets_the_liquidity_slot_with_the_full_word() {
        let (slot, value) = v3_liquidity_override(3_311_946_261_459_528_000_000u128);
        assert_eq!(slot, pad_u64(V3_LIQUIDITY_SLOT));
        assert_eq!(
            value,
            B256::from(U256::from(3_311_946_261_459_528_000_000u128))
        );
    }

    #[test]
    fn nudge_down_bps_applies_the_expected_percentage() {
        let value = U256::from(1_000_000u64);
        assert_eq!(nudge_down_bps(value, 0), value);
        assert_eq!(nudge_down_bps(value, 100), U256::from(990_000u64)); // -1%
    }

    #[test]
    fn v3_favorable_sqrt_price_nudges_up_for_zero_for_one_and_down_otherwise() {
        let price = U256::from(1_000_000u64);
        assert_eq!(
            v3_favorable_sqrt_price(price, true, 100),
            nudge_up_bps(price, 100)
        );
        assert_eq!(
            v3_favorable_sqrt_price(price, false, 100),
            nudge_down_bps(price, 100)
        );
    }

    #[test]
    fn v2_generous_amount_out_boosts_the_fee_free_ideal_and_doubles_for_balance_override() {
        let amount_in = U256::from(1_000_000u64);
        let reserve_in = U256::from(300_000_000_000u64);
        let reserve_out = U256::from(700_000_000_000u64);

        let fee_free_ideal = (amount_in * reserve_out) / (reserve_in + amount_in);
        let (amount_out, pool_balance) =
            v2_generous_amount_out(amount_in, reserve_in, reserve_out, 500);

        assert_eq!(amount_out, nudge_up_bps(fee_free_ideal, 500));
        assert!(amount_out > fee_free_ideal);
        assert_eq!(
            pool_balance,
            amount_out + U256::from(V2_BALANCE_OVERRIDE_HEADROOM)
        );
        // Headroom must dwarf any realistic on-chain reserve so the K-invariant check
        // passes regardless of the pool's real, unmodified reserves.
        assert!(pool_balance - amount_out > reserve_out * U256::from(1_000_000u64));
    }

    #[test]
    fn moe_lb_parameters_word_with_active_id_preserves_other_packed_fields() {
        // Real `_parameters` word on the WMNT/USDT binStep=15 pair
        // (`0xf6C9020c9E915808481757779EDB53DACEaE2415`), confirmed via `cast storage`
        // at slot 3; top 24 bits (`0x7fb5b6` = 8369590) match the pair's real
        // `getActiveId()`. Independently reproduced via:
        //   cast call <pair> "getActiveId()(uint24)" --rpc-url <mantle> \
        //     --override-state <pair>:0x...03:0x7fb5b7000b006a633d807fb5b6000000000055730271001d4c138825801e1a0a
        // which returned 8369591 (the nudged value), confirming the exact bit range.
        let real_word: B256 = "0x7fb5b6000b006a633d807fb5b6000000000055730271001d4c138825801e1a0a"
            .parse()
            .unwrap();

        let spliced = moe_lb_parameters_word_with_active_id(real_word, 8_369_591);

        let mask: U256 = U256::from(0xFFFFFFu32) << 232;
        let spliced_int = U256::from_be_bytes(spliced.0);
        assert_eq!(spliced_int & mask, U256::from(8_369_591u32) << 232);
        let real_int = U256::from_be_bytes(real_word.0);
        assert_eq!(spliced_int & !mask, real_int & !mask);
    }

    #[test]
    fn moe_lb_bin_slot_matches_the_empirically_discovered_mapping_slot() {
        // Fixture: activeId=8369590 on the real WMNT/USDT binStep=15 pair, reproduced
        // via `cast keccak <pad32(8369590) ++ pad32(6)>`.
        let expected: B256 = "0xccba41cb2675bd3ae518c8a0aa8ef25a845f2f6afb4302a4e2f6d2441fa3e1ea"
            .parse()
            .unwrap();
        assert_eq!(moe_lb_bin_slot(8_369_590), expected);
    }

    #[test]
    fn moe_lb_bin_reserve_word_matches_the_real_getbin_result() {
        // Real getBin(8369590) result on the WMNT/USDT binStep=15 pair
        // (binReserveX=8988900694668660, binReserveY=2109); this raw word was
        // independently confirmed via `cast storage` at moe_lb_bin_slot(8369590).
        let word = moe_lb_bin_reserve_word(8_988_900_694_668_660u128, 2_109u128);
        let expected: B256 = "0x0000000000000000000000000000083d0000000000000000001fef5b88d3b574"
            .parse()
            .unwrap();
        assert_eq!(word, expected);
    }

    #[test]
    fn v2_conservative_amount_out_stays_well_under_the_fee_free_ideal() {
        let amount_in = U256::from(1_000_000u64);
        let reserve_in = U256::from(300_000_000_000u64);
        let reserve_out = U256::from(700_000_000_000u64);

        let conservative = v2_conservative_amount_out(amount_in, reserve_in, reserve_out);
        let fee_free_ideal = (amount_in * reserve_out) / (reserve_in + amount_in);

        assert!(conservative > U256::ZERO);
        assert!(conservative < fee_free_ideal);
        assert_eq!(conservative, fee_free_ideal / U256::from(2u8));
    }

    #[test]
    fn nudge_up_bps_applies_the_expected_percentage() {
        let value = U256::from(1_000_000u64);
        assert_eq!(nudge_up_bps(value, 0), value);
        assert_eq!(nudge_up_bps(value, 100), U256::from(1_010_000u64)); // +1%
        assert_eq!(nudge_up_bps(value, 10_000), U256::from(2_000_000u64)); // +100%
    }

    #[test]
    fn nudge_u128_up_bps_applies_the_expected_percentage() {
        assert_eq!(nudge_u128_up_bps(1_000_000u128, 0), 1_000_000u128);
        assert_eq!(nudge_u128_up_bps(1_000_000u128, 100), 1_010_000u128);
    }

    #[test]
    fn v2_final_hop_settlement_yields_a_one_wei_profit_and_generous_payout_headroom() {
        let amount_in_leg1 = U256::from(5_000_000_000_000_000_000u64); // 5 WMNT (18 dp)
        let (amount_out, pool_balance) = v2_final_hop_settlement(amount_in_leg1);

        assert_eq!(amount_out, amount_in_leg1 + U256::from(1u8));
        assert_eq!(
            pool_balance,
            amount_out + U256::from(V2_BALANCE_OVERRIDE_HEADROOM)
        );
        assert!(pool_balance - amount_out > amount_in_leg1 * U256::from(1_000_000u64));
    }

    #[test]
    fn build_state_override_uses_state_diff_not_full_state_replace() {
        let addr1 = addr("0x000000000000000000000000000000000000000a");
        let accounts = vec![AccountStateOverride {
            address: addr1,
            code: Some(Bytes::from(vec![0x60, 0x00])),
            balance: Some(U256::from(1u64)),
            state_diff: vec![(B256::ZERO, B256::from(U256::from(7u64)))],
        }];

        let overrides = build_state_override(accounts);

        let over = overrides.get(&addr1).unwrap();
        assert!(over.state.is_none());
        assert_eq!(
            over.state_diff.as_ref().unwrap().get(&B256::ZERO),
            Some(&B256::from(U256::from(7u64)))
        );
        assert_eq!(over.balance, Some(U256::from(1u64)));
        assert!(over.code.is_some());
    }
}
