//! Builds the `StateOverride` for a shadow `eth_call`.
//!
//! Unlike `mainnet_fork_harness.rs`'s WHI-557 gas-measurement overrides (which
//! deliberately inflate pool reserves/prices to *force* a profitable outcome
//! regardless of real on-chain state), shadow mode's overrides exist only to
//! substitute for state a real, funded, pre-registered hot executor would already
//! have — access control, registry entries, and starting capital — so the call still
//! reports a genuine revert (insufficient liquidity, stale price, paused executor,
//! wrong CREATE2 venue, ...) exactly as it would for a real executor. No pool's
//! reserves, price, or liquidity are ever touched here.

use alloy::primitives::{Address, B256, U256};
use alloy::rpc::types::state::StateOverride;

use crate::execution::mainnet_fork_harness::{
    admin_override, build_state_override, erc20_balance_override, pad_address,
    registered_pool_slots, AccountStateOverride,
};
use crate::execution::provenance::contract_pool_type;
use crate::state_space::PoolProtocol;

use super::manifest::PoolProvenanceOutcome;
use super::moe_allowlist::{self, MoeAllowlist};
use super::wmnt_descriptor::WmntStorageShape;

/// Bypasses `onlyHotExecutor`'s `msg.sender == admin` check for `caller` — thin,
/// self-documenting wrapper over [`admin_override`].
pub(crate) fn caller_bypass_override(caller: Address) -> (B256, B256) {
    admin_override(caller)
}

/// Extracts the WMNT `balanceOf` mapping base slot from either storage shape — both
/// variants keep the mapping on the WMNT contract's own storage (see
/// `wmnt_descriptor::WmntStorageShape`'s doc comment on the proxy case).
fn balance_mapping_slot(shape: WmntStorageShape) -> u64 {
    match shape {
        WmntStorageShape::Direct {
            balance_mapping_slot,
        } => balance_mapping_slot,
        WmntStorageShape::Proxy {
            balance_mapping_slot,
            ..
        } => balance_mapping_slot,
    }
}

/// Credits `executor` with `amount` of WMNT — substituting for the starting capital a
/// real, funded hot executor would already hold.
pub(crate) fn executor_wmnt_funding_override(
    executor: Address,
    amount: U256,
    wmnt_storage_shape: WmntStorageShape,
) -> (B256, B256) {
    erc20_balance_override(executor, amount, balance_mapping_slot(wmnt_storage_shape))
}

/// One pool's registration/venue fields, one entry per hop in the candidate's route —
/// `ArbitrageExecutor.sol`'s multi-hop swap loop (`ArbitrageExecutor.sol:284`) reads
/// `registeredPools[pools[i]]` for *every* pool it visits, not just the first, so a
/// multi-hop candidate needs an override entry per hop.
#[derive(Debug, Clone, Copy)]
pub struct ShadowPoolOverrideInputs {
    pub pool: Address,
    pub pool_type: u8,
    pub token0: Address,
    pub token1: Address,
    pub fee: u32,
}

/// One candidate's full inputs for [`build_shadow_state_override`]: the executor/caller
/// identity and starting capital are shared across the whole route, while `pools` carries
/// one entry per hop.
#[derive(Debug, Clone)]
pub struct ShadowOverrideInputs {
    pub executor: Address,
    pub caller: Address,
    pub pools: Vec<ShadowPoolOverrideInputs>,
    pub wmnt_funding_amount: U256,
}

/// Establishes one hop's pool-address provenance. Moe LB pools are checked against
/// the committed allowlist (not CREATE2-derivable — see `create2.rs`'s doc comment on
/// `ArbitrageExecutor.sol:201`). Every other pool type currently has no committed
/// init-code-hash constant to CREATE2-verify against (see `create2.rs::expected_pool_address`'s
/// callers), so it is honestly recorded as unverifiable rather than checked against a
/// fabricated hash.
pub(crate) fn check_pool_provenance(
    pool: &ShadowPoolOverrideInputs,
    moe_allowlist: &MoeAllowlist,
) -> PoolProvenanceOutcome {
    if pool.pool_type != contract_pool_type(PoolProtocol::MoeLb) {
        return PoolProvenanceOutcome::Create2CheckSkipped;
    }
    if moe_allowlist::is_allowlisted(moe_allowlist, pool.pool, pool.token0, pool.token1, pool.fee) {
        PoolProvenanceOutcome::MoeAllowlisted
    } else {
        PoolProvenanceOutcome::Rejected(format!(
            "pool {} not present on the Moe LB allowlist for (token0={}, token1={}, bin_step={})",
            pool.pool, pool.token0, pool.token1, pool.fee
        ))
    }
}

/// Combines every hop's provenance outcome into one candidate-level outcome: a
/// multi-hop route's provenance can only be as strong as its weakest hop. Any rejected
/// hop rejects the whole route; otherwise the least-verified outcome present (an
/// unverifiable hop drags down an all-Moe route rather than being reported as fully
/// verified) is what gets recorded.
pub(crate) fn combine_provenance_outcomes(
    outcomes: impl IntoIterator<Item = PoolProvenanceOutcome>,
) -> PoolProvenanceOutcome {
    fn rank(outcome: &PoolProvenanceOutcome) -> u8 {
        match outcome {
            PoolProvenanceOutcome::Rejected(_) => 0,
            PoolProvenanceOutcome::Create2CheckSkipped => 1,
            PoolProvenanceOutcome::MoeAllowlisted => 2,
            PoolProvenanceOutcome::Verified => 3,
        }
    }
    outcomes
        .into_iter()
        .min_by_key(rank)
        .unwrap_or(PoolProvenanceOutcome::Verified)
}

/// Assembles the full `StateOverride` for a shadow `eth_call`: the executor's
/// `admin` bypass, each hop's `registeredPools[pool]` registry entry, and the
/// executor's WMNT starting balance — using `mainnet_fork_harness::build_state_override`
/// for the pure map assembly rather than re-deriving it. `registeredPools` lives on the
/// executor contract's own storage, so every hop's entries fold into the same
/// `executor_diff`. There is no `venues[poolType]` override here: `executeArbitrage`
/// never reads the `venues` mapping (only `registerPool` does), so writing it into the
/// override would substitute for state the real call path never consults.
pub(crate) fn build_shadow_state_override(
    wmnt_address: Address,
    wmnt_storage_shape: WmntStorageShape,
    inputs: &ShadowOverrideInputs,
) -> StateOverride {
    let mut executor_diff = Vec::with_capacity(1 + inputs.pools.len() * 2);
    executor_diff.push(caller_bypass_override(inputs.caller));
    for pool in &inputs.pools {
        executor_diff.extend(registered_pool_slots(
            pool.pool,
            pool.pool_type,
            pool.token0,
            pool.token1,
            pool.fee,
        ));
    }

    let (funding_slot, funding_value) = executor_wmnt_funding_override(
        inputs.executor,
        inputs.wmnt_funding_amount,
        wmnt_storage_shape,
    );

    let accounts = vec![
        AccountStateOverride {
            address: inputs.executor,
            code: None,
            balance: None,
            state_diff: executor_diff,
        },
        AccountStateOverride {
            address: wmnt_address,
            code: None,
            balance: None,
            state_diff: vec![(funding_slot, funding_value)],
        },
    ];

    build_state_override(accounts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    fn sample_pool() -> ShadowPoolOverrideInputs {
        ShadowPoolOverrideInputs {
            pool: address!("f6C9020c9E915808481757779EDB53DACEaE2415"),
            pool_type: 2,
            token0: address!("201EBa5CC46D216Ce6DC03F6a759e8E766e956aE"),
            token1: address!("78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8"),
            fee: 0,
        }
    }

    fn sample_inputs() -> ShadowOverrideInputs {
        ShadowOverrideInputs {
            executor: Address::repeat_byte(0x11),
            caller: Address::repeat_byte(0x22),
            pools: vec![sample_pool()],
            wmnt_funding_amount: U256::from(5_000_000_000_000_000_000u64),
        }
    }

    #[test]
    fn caller_bypass_override_targets_admin_slot_zero() {
        let caller = Address::repeat_byte(0x22);
        let (slot, value) = caller_bypass_override(caller);
        assert_eq!(slot, B256::ZERO);
        assert_eq!(value, pad_address(caller));
    }

    #[test]
    fn balance_mapping_slot_reads_both_shapes() {
        assert_eq!(
            balance_mapping_slot(WmntStorageShape::Direct {
                balance_mapping_slot: 7
            }),
            7
        );
        assert_eq!(
            balance_mapping_slot(WmntStorageShape::Proxy {
                implementation_slot: 1,
                balance_mapping_slot: 9
            }),
            9
        );
    }

    #[test]
    fn executor_wmnt_funding_override_matches_erc20_balance_override() {
        let executor = Address::repeat_byte(0x11);
        let amount = U256::from(42u64);
        let shape = WmntStorageShape::Direct {
            balance_mapping_slot: 0,
        };
        let (slot, value) = executor_wmnt_funding_override(executor, amount, shape);
        let expected = erc20_balance_override(executor, amount, 0);
        assert_eq!((slot, value), expected);
    }

    #[test]
    fn build_shadow_state_override_writes_the_executor_and_wmnt_accounts() {
        let wmnt_address = address!("78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8");
        let inputs = sample_inputs();

        let overrides = build_shadow_state_override(
            wmnt_address,
            WmntStorageShape::Direct {
                balance_mapping_slot: 0,
            },
            &inputs,
        );

        let executor_override = overrides.get(&inputs.executor).unwrap();
        // admin bypass + 2 registeredPools words = 3 state_diff entries.
        assert_eq!(executor_override.state_diff.as_ref().unwrap().len(), 3);
        assert!(executor_override.state.is_none());

        let wmnt_override = overrides.get(&wmnt_address).unwrap();
        assert_eq!(wmnt_override.state_diff.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn build_shadow_state_override_folds_every_hop_into_one_executor_account() {
        let wmnt_address = address!("78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8");
        let mut second_pool = sample_pool();
        second_pool.pool = Address::repeat_byte(0xde);
        second_pool.pool_type = 0;
        let inputs = ShadowOverrideInputs {
            executor: Address::repeat_byte(0x11),
            caller: Address::repeat_byte(0x22),
            pools: vec![sample_pool(), second_pool],
            wmnt_funding_amount: U256::from(5_000_000_000_000_000_000u64),
        };

        let overrides = build_shadow_state_override(
            wmnt_address,
            WmntStorageShape::Direct {
                balance_mapping_slot: 0,
            },
            &inputs,
        );

        let executor_override = overrides.get(&inputs.executor).unwrap();
        // admin bypass + 2 hops * 2 registeredPools words = 5.
        assert_eq!(executor_override.state_diff.as_ref().unwrap().len(), 5);
    }

    fn moe_pool() -> ShadowPoolOverrideInputs {
        let mut pool = sample_pool();
        pool.pool_type = contract_pool_type(PoolProtocol::MoeLb);
        pool
    }

    fn allowlist_for(pool: &ShadowPoolOverrideInputs) -> MoeAllowlist {
        MoeAllowlist {
            schema_version: 1,
            entries: vec![super::moe_allowlist::MoeAllowlistEntry {
                pool: pool.pool,
                token_x: pool.token0,
                token_y: pool.token1,
                bin_step: pool.fee,
                notes: None,
            }],
        }
    }

    #[test]
    fn check_pool_provenance_skips_create2_for_non_moe_pool_types() {
        let pool = sample_pool();
        assert_eq!(pool.pool_type, contract_pool_type(PoolProtocol::MoeLb));
        let mut v2_pool = pool;
        v2_pool.pool_type = 0;
        let empty_allowlist = MoeAllowlist {
            schema_version: 1,
            entries: vec![],
        };

        let outcome = check_pool_provenance(&v2_pool, &empty_allowlist);

        assert_eq!(outcome, PoolProvenanceOutcome::Create2CheckSkipped);
    }

    #[test]
    fn check_pool_provenance_accepts_an_allowlisted_moe_pool() {
        let pool = moe_pool();
        let allowlist = allowlist_for(&pool);

        let outcome = check_pool_provenance(&pool, &allowlist);

        assert_eq!(outcome, PoolProvenanceOutcome::MoeAllowlisted);
    }

    #[test]
    fn check_pool_provenance_rejects_a_moe_pool_missing_from_the_allowlist() {
        let pool = moe_pool();
        let empty_allowlist = MoeAllowlist {
            schema_version: 1,
            entries: vec![],
        };

        let outcome = check_pool_provenance(&pool, &empty_allowlist);

        match outcome {
            PoolProvenanceOutcome::Rejected(reason) => {
                assert!(reason.contains(&pool.pool.to_string()));
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn combine_provenance_outcomes_is_verified_for_an_empty_route() {
        let combined = combine_provenance_outcomes(std::iter::empty());
        assert_eq!(combined, PoolProvenanceOutcome::Verified);
    }

    #[test]
    fn combine_provenance_outcomes_reports_the_weakest_hop() {
        let combined = combine_provenance_outcomes([
            PoolProvenanceOutcome::MoeAllowlisted,
            PoolProvenanceOutcome::Create2CheckSkipped,
        ]);
        assert_eq!(combined, PoolProvenanceOutcome::Create2CheckSkipped);
    }

    #[test]
    fn combine_provenance_outcomes_lets_any_rejection_reject_the_whole_route() {
        let combined = combine_provenance_outcomes([
            PoolProvenanceOutcome::MoeAllowlisted,
            PoolProvenanceOutcome::Rejected("bad pool".to_string()),
            PoolProvenanceOutcome::Create2CheckSkipped,
        ]);
        assert_eq!(
            combined,
            PoolProvenanceOutcome::Rejected("bad pool".to_string())
        );
    }

}
