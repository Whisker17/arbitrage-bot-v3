//! Centralized gas limit schedule for swap execution on Mantle.
//!
//! Mantle 的 `estimate_gas` 在复杂路径时并不可靠，因此我们根据交易中
//! 包含的 hop 数量使用固定的 gas 限额。该配置在执行器与脚本之间共用，
//! 便于统一维护。

/// Gas limit for single-hop swaps (1 pool interaction).
pub const ONE_HOP_GAS_LIMIT: u64 = 300_000_000;
/// Gas limit for two-hop swaps.
pub const TWO_HOP_GAS_LIMIT: u64 = 600_000_000;
/// Gas limit for three-hop swaps.
pub const THREE_HOP_GAS_LIMIT: u64 = 800_000_000;
/// Gas limit for four-hop swaps.
pub const FOUR_HOP_GAS_LIMIT: u64 = 1_500_000_000;
/// Default gas limit when hop count exceeds predefined schedule.
pub const DEFAULT_GAS_LIMIT: u64 = FOUR_HOP_GAS_LIMIT;

/// Return the recommended gas limit for a swap transaction containing the given number of hops.
///
/// Values greater than four hops will be clamped to [`DEFAULT_GAS_LIMIT`].
pub const fn gas_limit_for_hops(hops: usize) -> u64 {
    match hops {
        0 | 1 => ONE_HOP_GAS_LIMIT,
        2 => TWO_HOP_GAS_LIMIT,
        3 => THREE_HOP_GAS_LIMIT,
        4 => FOUR_HOP_GAS_LIMIT,
        _ => DEFAULT_GAS_LIMIT,
    }
}
