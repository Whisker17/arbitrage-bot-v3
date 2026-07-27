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

use alloy::primitives::{Address, Bytes, B256, U256};
use alloy::rpc::types::state::StateOverride;
use serde_json::Value;

use crate::execution::mainnet_fork_harness::{
    build_state_override, erc20_balance_override, registered_pool_slots, AccountStateOverride,
};
use crate::execution::provenance::contract_pool_type;
use crate::state_space::PoolProtocol;

use super::approved_pools::{approved_entry_for, ApprovedPoolsConfig};
use super::create2::{expected_pool_address, expected_salt};
use super::manifest::{Create2Proof, PoolProvenanceOutcome};
use super::moe_allowlist::{self, MoeAllowlist};
use super::slots::{self, SlotsError};
use super::wmnt_descriptor::WmntStorageShape;

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
    pub protocol: PoolProtocol,
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
/// `ArbitrageExecutor.sol:201`). Every other pool type is CREATE2-verified against the
/// committed `(factory, init_code_hash)` entry for its protocol in `approved_pools` — a
/// protocol with no committed entry is rejected as unverifiable rather than treated as
/// a pass.
pub(crate) fn check_pool_provenance(
    pool: &ShadowPoolOverrideInputs,
    moe_allowlist: &MoeAllowlist,
    approved_pools: &ApprovedPoolsConfig,
) -> PoolProvenanceOutcome {
    if pool.protocol == PoolProtocol::MoeLb {
        return if moe_allowlist::is_allowlisted(moe_allowlist, pool.pool, pool.token0, pool.token1, pool.fee)
        {
            PoolProvenanceOutcome::MoeAllowlisted
        } else {
            PoolProvenanceOutcome::Rejected(format!(
                "pool {} not present on the Moe LB allowlist for (token0={}, token1={}, bin_step={})",
                pool.pool, pool.token0, pool.token1, pool.fee
            ))
        };
    }

    let Some(entry) = approved_entry_for(approved_pools, pool.protocol) else {
        return PoolProvenanceOutcome::Rejected(format!(
            "no approved CREATE2 registration entry committed for protocol {:?}",
            pool.protocol
        ));
    };

    match expected_pool_address(
        pool.protocol,
        entry.factory,
        pool.token0,
        pool.token1,
        pool.fee,
        entry.init_code_hash,
    ) {
        Some(expected) if expected == pool.pool => {
            // `expected_pool_address` returning `Some` above guarantees the same
            // protocol is CREATE2-derivable, so `expected_salt` cannot be `None` here.
            let salt = expected_salt(pool.protocol, pool.token0, pool.token1, pool.fee)
                .expect("expected_pool_address returned Some, so expected_salt must too");
            PoolProvenanceOutcome::Verified(Create2Proof {
                protocol: entry.protocol,
                factory: entry.factory,
                init_code_hash: entry.init_code_hash,
                salt,
            })
        }
        Some(expected) => PoolProvenanceOutcome::Rejected(format!(
            "pool {} does not match its CREATE2-derived address {} for protocol {:?} (factory={}, init_code_hash={})",
            pool.pool, expected, pool.protocol, entry.factory, entry.init_code_hash
        )),
        None => PoolProvenanceOutcome::Rejected(format!(
            "protocol {:?} is not CREATE2-derivable",
            pool.protocol
        )),
    }
}

/// Combines every hop's provenance outcome into one candidate-level outcome: a
/// multi-hop route's provenance can only be as strong as its weakest hop. Any rejected
/// hop rejects the whole route; otherwise the least-verified outcome present is what
/// gets recorded. An empty route has no pools to verify and is rejected fail-closed,
/// not treated as vacuously verified.
pub(crate) fn combine_provenance_outcomes(
    outcomes: impl IntoIterator<Item = PoolProvenanceOutcome>,
) -> PoolProvenanceOutcome {
    fn rank(outcome: &PoolProvenanceOutcome) -> u8 {
        match outcome {
            PoolProvenanceOutcome::Rejected(_) => 0,
            PoolProvenanceOutcome::MoeAllowlisted => 1,
            PoolProvenanceOutcome::Verified(_) => 2,
        }
    }
    outcomes.into_iter().min_by_key(rank).unwrap_or_else(|| {
        PoolProvenanceOutcome::Rejected("empty route: no pools to verify".to_string())
    })
}

/// Assembles the full `StateOverride` for a shadow `eth_call`: the executor's patched
/// runtime code, `paused` forced to `false`, the caller's `isHotExecutor` role, each
/// hop's `registeredPools[pool]` registry entry, and the executor's WMNT starting
/// balance — using `mainnet_fork_harness::build_state_override` for the pure map
/// assembly rather than re-deriving it. `registeredPools` lives on the executor
/// contract's own storage, so every hop's entries fold into the same `executor_diff`.
/// There is no `venues[poolType]` override here: `executeArbitrage` never reads the
/// `venues` mapping (only `registerPool` does), so writing it into the override would
/// substitute for state the real call path never consults.
///
/// `storage_layout` and `patched_runtime` come from WHI-551's compiled evidence /
/// verified identity — shadow mode has no live RPC verification that the target address
/// already carries the patched runtime, so it must inject it itself rather than assume
/// so.
pub(crate) fn build_shadow_state_override(
    wmnt_address: Address,
    wmnt_storage_shape: WmntStorageShape,
    storage_layout: &Value,
    patched_runtime: &[u8],
    inputs: &ShadowOverrideInputs,
) -> Result<StateOverride, SlotsError> {
    let mut executor_diff = Vec::with_capacity(2 + inputs.pools.len() * 2);
    executor_diff.push(slots::paused_override(storage_layout)?);
    executor_diff.push(slots::is_hot_executor_override(
        storage_layout,
        inputs.caller,
    )?);
    for pool in &inputs.pools {
        executor_diff.extend(registered_pool_slots(
            pool.pool,
            contract_pool_type(pool.protocol),
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
            code: Some(Bytes::copy_from_slice(patched_runtime)),
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

    Ok(build_state_override(accounts))
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::approved_pools::{ApprovedPoolEntry, ApprovedPoolProtocol};
    use alloy::primitives::address;
    use serde_json::json;

    fn sample_storage_layout() -> Value {
        json!({
            "storage": [
                {"astId": 1, "contract": "c", "label": "admin", "offset": 0, "slot": "0", "type": "t_address"},
                {"astId": 2, "contract": "c", "label": "guardian", "offset": 0, "slot": "1", "type": "t_address"},
                {"astId": 3, "contract": "c", "label": "paused", "offset": 20, "slot": "1", "type": "t_bool"},
                {"astId": 4, "contract": "c", "label": "isHotExecutor", "offset": 0, "slot": "2", "type": "t_mapping(t_address,t_bool)"},
                {"astId": 5, "contract": "c", "label": "registeredPools", "offset": 0, "slot": "3", "type": "t_mapping(t_address,t_struct(RegisteredPool)storage)"}
            ],
            "types": {
                "t_address": {"encoding": "inplace", "label": "address", "numberOfBytes": "20"},
                "t_bool": {"encoding": "inplace", "label": "bool", "numberOfBytes": "1"}
            }
        })
    }

    fn sample_patched_runtime() -> Vec<u8> {
        vec![0xFE, 0xED, 0xFA, 0xCE]
    }

    fn sample_pool() -> ShadowPoolOverrideInputs {
        ShadowPoolOverrideInputs {
            pool: address!("f6C9020c9E915808481757779EDB53DACEaE2415"),
            protocol: PoolProtocol::MoeLb,
            token0: address!("201EBa5CC46D216Ce6DC03F6a759e8E766e956aE"),
            token1: address!("78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8"),
            fee: 0,
        }
    }

    fn empty_approved_pools() -> ApprovedPoolsConfig {
        ApprovedPoolsConfig {
            schema_version: 1,
            entries: vec![],
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
        let storage_layout = sample_storage_layout();
        let patched_runtime = sample_patched_runtime();

        let overrides = build_shadow_state_override(
            wmnt_address,
            WmntStorageShape::Direct {
                balance_mapping_slot: 0,
            },
            &storage_layout,
            &patched_runtime,
            &inputs,
        )
        .unwrap();

        let executor_override = overrides.get(&inputs.executor).unwrap();
        // paused + isHotExecutor + 2 registeredPools words = 4 state_diff entries.
        assert_eq!(executor_override.state_diff.as_ref().unwrap().len(), 4);
        assert!(executor_override.state.is_none());
        assert_eq!(
            executor_override.code.as_ref().unwrap().as_ref(),
            patched_runtime.as_slice()
        );

        let wmnt_override = overrides.get(&wmnt_address).unwrap();
        assert_eq!(wmnt_override.state_diff.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn build_shadow_state_override_folds_every_hop_into_one_executor_account() {
        let wmnt_address = address!("78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8");
        let mut second_pool = sample_pool();
        second_pool.pool = Address::repeat_byte(0xde);
        second_pool.protocol = PoolProtocol::UniswapV2;
        let inputs = ShadowOverrideInputs {
            executor: Address::repeat_byte(0x11),
            caller: Address::repeat_byte(0x22),
            pools: vec![sample_pool(), second_pool],
            wmnt_funding_amount: U256::from(5_000_000_000_000_000_000u64),
        };
        let storage_layout = sample_storage_layout();
        let patched_runtime = sample_patched_runtime();

        let overrides = build_shadow_state_override(
            wmnt_address,
            WmntStorageShape::Direct {
                balance_mapping_slot: 0,
            },
            &storage_layout,
            &patched_runtime,
            &inputs,
        )
        .unwrap();

        let executor_override = overrides.get(&inputs.executor).unwrap();
        // paused + isHotExecutor + 2 hops * 2 registeredPools words = 6.
        assert_eq!(executor_override.state_diff.as_ref().unwrap().len(), 6);
    }

    fn moe_pool() -> ShadowPoolOverrideInputs {
        let mut pool = sample_pool();
        pool.protocol = PoolProtocol::MoeLb;
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

    fn approved_pools_with(entry: ApprovedPoolEntry) -> ApprovedPoolsConfig {
        ApprovedPoolsConfig {
            schema_version: 1,
            entries: vec![entry],
        }
    }

    fn v2_pool_matching(factory: Address, init_code_hash: B256) -> ShadowPoolOverrideInputs {
        let mut pool = sample_pool();
        pool.protocol = PoolProtocol::UniswapV2;
        pool.pool = expected_pool_address(
            PoolProtocol::UniswapV2,
            factory,
            pool.token0,
            pool.token1,
            pool.fee,
            init_code_hash,
        )
        .unwrap();
        pool
    }

    #[test]
    fn check_pool_provenance_verifies_a_v2_pool_matching_its_create2_address() {
        let factory = Address::repeat_byte(0x33);
        let init_code_hash = B256::repeat_byte(0x44);
        let pool = v2_pool_matching(factory, init_code_hash);
        let approved = approved_pools_with(ApprovedPoolEntry {
            protocol: ApprovedPoolProtocol::UniswapV2,
            factory,
            init_code_hash,
            notes: None,
        });
        let empty_allowlist = MoeAllowlist {
            schema_version: 1,
            entries: vec![],
        };

        let outcome = check_pool_provenance(&pool, &empty_allowlist, &approved);

        match outcome {
            PoolProvenanceOutcome::Verified(proof) => {
                assert_eq!(proof.protocol, ApprovedPoolProtocol::UniswapV2);
                assert_eq!(proof.factory, factory);
                assert_eq!(proof.init_code_hash, init_code_hash);
                assert_eq!(
                    proof.salt,
                    expected_salt(PoolProtocol::UniswapV2, pool.token0, pool.token1, pool.fee)
                        .unwrap()
                );
            }
            other => panic!("expected Verified, got {other:?}"),
        }
    }

    #[test]
    fn check_pool_provenance_rejects_a_v2_pool_not_matching_its_create2_address() {
        let factory = Address::repeat_byte(0x33);
        let init_code_hash = B256::repeat_byte(0x44);
        let mut pool = v2_pool_matching(factory, init_code_hash);
        pool.pool = Address::repeat_byte(0x99);
        let approved = approved_pools_with(ApprovedPoolEntry {
            protocol: ApprovedPoolProtocol::UniswapV2,
            factory,
            init_code_hash,
            notes: None,
        });
        let empty_allowlist = MoeAllowlist {
            schema_version: 1,
            entries: vec![],
        };

        let outcome = check_pool_provenance(&pool, &empty_allowlist, &approved);

        assert!(matches!(outcome, PoolProvenanceOutcome::Rejected(_)));
    }

    #[test]
    fn check_pool_provenance_rejects_when_no_approved_entry_is_committed_for_the_protocol() {
        let mut pool = sample_pool();
        pool.protocol = PoolProtocol::UniswapV3;
        let empty_allowlist = MoeAllowlist {
            schema_version: 1,
            entries: vec![],
        };

        let outcome = check_pool_provenance(&pool, &empty_allowlist, &empty_approved_pools());

        assert!(matches!(outcome, PoolProvenanceOutcome::Rejected(_)));
    }

    #[test]
    fn check_pool_provenance_accepts_an_allowlisted_moe_pool() {
        let pool = moe_pool();
        let allowlist = allowlist_for(&pool);

        let outcome = check_pool_provenance(&pool, &allowlist, &empty_approved_pools());

        assert_eq!(outcome, PoolProvenanceOutcome::MoeAllowlisted);
    }

    #[test]
    fn check_pool_provenance_rejects_a_moe_pool_missing_from_the_allowlist() {
        let pool = moe_pool();
        let empty_allowlist = MoeAllowlist {
            schema_version: 1,
            entries: vec![],
        };

        let outcome = check_pool_provenance(&pool, &empty_allowlist, &empty_approved_pools());

        match outcome {
            PoolProvenanceOutcome::Rejected(reason) => {
                assert!(reason.contains(&pool.pool.to_string()));
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn combine_provenance_outcomes_rejects_an_empty_route() {
        let combined = combine_provenance_outcomes(std::iter::empty());
        assert!(matches!(combined, PoolProvenanceOutcome::Rejected(_)));
    }

    fn sample_create2_proof() -> Create2Proof {
        Create2Proof {
            protocol: ApprovedPoolProtocol::UniswapV2,
            factory: Address::repeat_byte(0x33),
            init_code_hash: B256::repeat_byte(0x44),
            salt: B256::repeat_byte(0x55),
        }
    }

    #[test]
    fn combine_provenance_outcomes_reports_the_weakest_hop() {
        let combined = combine_provenance_outcomes([
            PoolProvenanceOutcome::Verified(sample_create2_proof()),
            PoolProvenanceOutcome::MoeAllowlisted,
        ]);
        assert_eq!(combined, PoolProvenanceOutcome::MoeAllowlisted);
    }

    #[test]
    fn combine_provenance_outcomes_lets_any_rejection_reject_the_whole_route() {
        let combined = combine_provenance_outcomes([
            PoolProvenanceOutcome::MoeAllowlisted,
            PoolProvenanceOutcome::Rejected("bad pool".to_string()),
            PoolProvenanceOutcome::Verified(sample_create2_proof()),
        ]);
        assert_eq!(
            combined,
            PoolProvenanceOutcome::Rejected("bad pool".to_string())
        );
    }
}
