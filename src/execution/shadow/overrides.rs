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

use alloy::eips::BlockId;
use alloy::primitives::{keccak256, Address, Bytes, B256, U256};
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::state::StateOverride;
use futures::future::join_all;
use serde_json::Value;

use crate::execution::contract::{IAgniPool, IMoeLBPair};
use crate::execution::mainnet_fork_harness::{
    build_state_override, erc20_balance_override, registered_pool_slots, AccountStateOverride,
};
use crate::execution::provenance::contract_pool_type;
use crate::state_space::PoolProtocol;

use super::approved_pools::{approved_entry_for, ApprovedPoolsConfig};
use super::create2::expected_create2_derivation;
use super::digest::digest_of;
use super::manifest::{Create2Proof, PoolProvenanceOutcome};
use super::moe_allowlist::{self, MoeAllowlist};
use super::slots::{self, SlotsError};
use super::wmnt_descriptor::WmntStorageShape;

/// Extracts the WMNT `balanceOf` mapping base slot from either storage shape — both
/// variants keep the mapping on the WMNT contract's own storage (see
/// `wmnt_descriptor::WmntStorageShape`'s doc comment on the proxy case).
fn balance_mapping_slot(shape: &WmntStorageShape) -> u64 {
    match shape {
        WmntStorageShape::Direct { storage_layout, .. } => storage_layout.balance_mapping_slot,
        WmntStorageShape::Proxy { storage_layout, .. } => storage_layout.balance_mapping_slot,
    }
}

/// Credits `executor` with `amount` of WMNT — substituting for the starting capital a
/// real, funded hot executor would already hold.
pub(crate) fn executor_wmnt_funding_override(
    executor: Address,
    amount: U256,
    wmnt_storage_shape: &WmntStorageShape,
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
    /// For Moe LB, `token0`/`token1` carry the pair's `tokenX`/`tokenY`.
    pub token0: Address,
    pub token1: Address,
    /// The pool's fee tier — except for Moe LB, where this carries the pair's
    /// `bin_step` instead. Deliberately one field, mirroring the on-chain
    /// `registeredPools` struct's own single `fee` word (`ArbitrageExecutor.sol`), which
    /// this feeds via [`registered_pool_slots`] and which stores a Moe pair's bin step
    /// in exactly the same slot. V2 pools have neither and pass `0`.
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
    pub candidate_amount_in: U256,
    pub wmnt_funding_amount: U256,
}

/// What the ledger records about *which* route a candidate is, independent of the
/// transaction built for it: its topology fingerprint, the pools that fingerprint covers,
/// and the input size tried. These three always travel together — from
/// `ShadowExecutionContext::build_preflight` through `ShadowSemanticCallExecutor` into
/// `ShadowLedgerWriter::record_context` — so they move as one value rather than as three
/// positional arguments repeated at each hop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowRouteSummary {
    /// Digest over the ordered, decoded pool list. Stable across nonce/gas/amount
    /// variations of the same route, so the ledger can group every attempt at one
    /// opportunity — unlike `final_request_digest`, which is over the fully-built
    /// transaction.
    pub opportunity_id: B256,
    /// Every hop's pool address, in route order, so a ledger row names the pools
    /// `opportunity_id` covers.
    pub ordered_pools: Vec<Address>,
    pub amount_in: U256,
}

impl ShadowRouteSummary {
    /// Derives the summary from one candidate's override inputs. Lives here, beside
    /// [`ShadowOverrideInputs`], because it reads nothing but that type's own fields.
    pub fn of(inputs: &ShadowOverrideInputs) -> Self {
        let pools_value = serde_json::json!(inputs
            .pools
            .iter()
            .map(|pool| serde_json::json!({
                "pool": pool.pool,
                "protocol": format!("{:?}", pool.protocol),
                "token0": pool.token0,
                "token1": pool.token1,
                "fee": pool.fee,
            }))
            .collect::<Vec<_>>());
        Self {
            opportunity_id: digest_of(&pools_value),
            ordered_pools: inputs.pools.iter().map(|pool| pool.pool).collect(),
            amount_in: inputs.candidate_amount_in,
        }
    }
}

/// Independently confirms `pool`'s on-chain `token0()`/`token1()` (V2/V3/Agni) or
/// `getTokenX()`/`getTokenY()` (Moe) match the claimed token identity — the CREATE2
/// recomputation and allowlist lookup above only establish that the *address* is
/// registration-authorized; they say nothing about what the contract at that address
/// actually reports today. A call failure is treated the same as a mismatch: an
/// unverifiable proof must reject, never pass through as if verified.
///
/// Pinned to `block` (the same block every other hop's checks in this route use, see
/// [`check_route_provenance`]) rather than an implicit "latest" — without a pin, two
/// calls issued moments apart (e.g. this and [`verify_moe_runtime_codehash`], or this
/// hop and the next) could silently land on different blocks if one arrives right at a
/// new-block boundary, making the combined proof internally inconsistent.
async fn verify_token_getters(
    provider: &DynProvider,
    pool: &ShadowPoolOverrideInputs,
    block: BlockId,
) -> Result<(), String> {
    let getter_failed = |getter: &str, error: alloy::contract::Error| -> String {
        format!("{getter} call failed for pool {}: {error}", pool.pool)
    };
    let (onchain_token0, onchain_token1) = match pool.protocol {
        // `token0()`/`token1()` are byte-identical selectors returning `address` on all
        // three of V2, V3, and Agni pools, so one binding reads all three — only Moe LB
        // renames them (`getTokenX`/`getTokenY`) and needs its own branch.
        PoolProtocol::UniswapV2 | PoolProtocol::UniswapV3 | PoolProtocol::Agni => {
            let contract = IAgniPool::new(pool.pool, provider);
            let token0 = contract
                .token0()
                .call()
                .block(block)
                .await
                .map_err(|error| getter_failed("token0()", error))?;
            let token1 = contract
                .token1()
                .call()
                .block(block)
                .await
                .map_err(|error| getter_failed("token1()", error))?;
            (token0, token1)
        }
        PoolProtocol::MoeLb => {
            let contract = IMoeLBPair::new(pool.pool, provider);
            let token_x = contract
                .getTokenX()
                .call()
                .block(block)
                .await
                .map_err(|error| getter_failed("getTokenX()", error))?;
            let token_y = contract
                .getTokenY()
                .call()
                .block(block)
                .await
                .map_err(|error| getter_failed("getTokenY()", error))?;
            (token_x, token_y)
        }
    };

    if onchain_token0 != pool.token0 || onchain_token1 != pool.token1 {
        return Err(format!(
            "pool {} on-chain token getters returned (token0={}, token1={}), which does not \
             match the claimed (token0={}, token1={})",
            pool.pool, onchain_token0, onchain_token1, pool.token0, pool.token1
        ));
    }
    Ok(())
}

/// Independently confirms `pool`'s on-chain runtime bytecode (`eth_getCode`) hashes to
/// the allowlist's pinned `runtime_codehash` — Moe LB pairs are not CREATE2-derivable,
/// so this is the substitute proof that the allowlisted address still carries the
/// expected contract rather than a swapped-in impersonator.
///
/// Pinned to `block`, same as [`verify_token_getters`] — see that function's doc comment
/// for why an implicit "latest" isn't good enough here.
async fn verify_moe_runtime_codehash(
    provider: &DynProvider,
    pool: Address,
    expected: B256,
    block: BlockId,
) -> Result<(), String> {
    let code = provider
        .get_code_at(pool)
        .block_id(block)
        .await
        .map_err(|error| format!("eth_getCode failed for pool {pool}: {error}"))?;
    let actual = keccak256(&code);
    if actual != expected {
        return Err(format!(
            "pool {pool} on-chain runtime codehash {actual} does not match the pinned {expected}"
        ));
    }
    Ok(())
}

/// Establishes one hop's pool-address provenance. Moe LB pools are checked against
/// the committed allowlist plus a pinned runtime codehash (not CREATE2-derivable — see
/// `create2.rs`'s doc comment on `ArbitrageExecutor.sol:201`). Every other pool type is
/// CREATE2-verified against the committed `(factory, init_code_hash)` entry for its
/// protocol in `approved_pools` — a protocol with no committed entry is rejected as
/// unverifiable rather than treated as a pass. Every pool type, regardless of protocol,
/// also gets an independent on-chain token-getter check (`verify_token_getters`): a
/// registration-authorized address whose live token getters don't match the claimed
/// identity is rejected, not waved through.
///
/// `block` pins every RPC this function issues to the same block number — see
/// [`check_route_provenance`], which resolves it once per route so every hop's checks
/// (and each hop's own token-getter/codehash checks against each other) read
/// consistent state.
pub(crate) async fn check_pool_provenance(
    pool: &ShadowPoolOverrideInputs,
    moe_allowlist: &MoeAllowlist,
    approved_pools: &ApprovedPoolsConfig,
    provider: &DynProvider,
    block: BlockId,
) -> PoolProvenanceOutcome {
    if pool.protocol == PoolProtocol::MoeLb {
        let Some(entry) =
            moe_allowlist::find_entry(moe_allowlist, pool.pool, pool.token0, pool.token1, pool.fee)
        else {
            return PoolProvenanceOutcome::Rejected(format!(
                "pool {} not present on the Moe LB allowlist for (token0={}, token1={}, bin_step={})",
                pool.pool, pool.token0, pool.token1, pool.fee
            ));
        };
        if let Err(reason) =
            verify_moe_runtime_codehash(provider, pool.pool, entry.runtime_codehash, block).await
        {
            return PoolProvenanceOutcome::Rejected(reason);
        }
        if let Err(reason) = verify_token_getters(provider, pool, block).await {
            return PoolProvenanceOutcome::Rejected(reason);
        }
        return PoolProvenanceOutcome::MoeAllowlisted;
    }

    let Some(entry) = approved_entry_for(approved_pools, pool.protocol) else {
        return PoolProvenanceOutcome::Rejected(format!(
            "no approved CREATE2 registration entry committed for protocol {:?}",
            pool.protocol
        ));
    };

    match expected_create2_derivation(
        pool.protocol,
        entry.factory,
        pool.token0,
        pool.token1,
        pool.fee,
        entry.init_code_hash,
    ) {
        Some(derivation) if derivation.address == pool.pool => {
            if let Err(reason) = verify_token_getters(provider, pool, block).await {
                return PoolProvenanceOutcome::Rejected(reason);
            }
            PoolProvenanceOutcome::Verified(Create2Proof {
                protocol: entry.protocol,
                factory: entry.factory,
                init_code_hash: entry.init_code_hash,
                salt: derivation.salt,
            })
        }
        Some(derivation) => PoolProvenanceOutcome::Rejected(format!(
            "pool {} does not match its CREATE2-derived address {} for protocol {:?} (factory={}, init_code_hash={})",
            pool.pool, derivation.address, pool.protocol, entry.factory, entry.init_code_hash
        )),
        None => PoolProvenanceOutcome::Rejected(format!(
            "protocol {:?} is not CREATE2-derivable",
            pool.protocol
        )),
    }
}

/// Runs [`check_pool_provenance`] for every hop in `pools` concurrently and returns each
/// hop's own outcome, in route order — the caller (`context.rs::build_preflight`) both
/// records this full per-hop vector in the ledger (so a multi-hop route's provenance
/// coverage is independently auditable, not just its combined worst-case result) and
/// folds it into one candidate-level outcome via [`combine_provenance_outcomes`] for the
/// `EnvUnsupported` short-circuit.
///
/// Resolves the current block number once up front and pins every hop's checks to it,
/// rather than letting each of the (many) RPC calls below default to an implicit
/// "latest" independently — issued concurrently via `join_all`, two implicit-latest
/// calls straddling a new-block boundary could observe different chain states, making a
/// single route's combined provenance internally inconsistent. Failing to resolve a
/// block at all rejects the whole route fail-closed (a single-element `Rejected` vector),
/// the same posture as any other unverifiable proof here.
pub(crate) async fn check_route_provenance(
    pools: &[ShadowPoolOverrideInputs],
    moe_allowlist: &MoeAllowlist,
    approved_pools: &ApprovedPoolsConfig,
    provider: &DynProvider,
) -> Vec<PoolProvenanceOutcome> {
    let block = match provider.get_block_number().await {
        Ok(number) => BlockId::from(number),
        Err(error) => {
            return vec![PoolProvenanceOutcome::Rejected(format!(
                "failed to pin a block for this route's provenance checks: {error}"
            ))];
        }
    };
    let checks = pools
        .iter()
        .map(|pool| check_pool_provenance(pool, moe_allowlist, approved_pools, provider, block));
    join_all(checks).await
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
    wmnt_storage_shape: &WmntStorageShape,
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
    let registered_pools_base_slot = slots::registered_pools_base_slot(storage_layout)?;
    for pool in &inputs.pools {
        executor_diff.extend(registered_pool_slots(
            pool.pool,
            contract_pool_type(pool.protocol),
            pool.token0,
            pool.token1,
            pool.fee,
            registered_pools_base_slot,
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
    use super::super::approved_pools::{ApprovedPoolEntry, ApprovedPoolProtocol};
    use super::super::wmnt_descriptor::{StorageLayoutRef, VerifiedArtifactRef};
    use super::*;
    use alloy::primitives::address;
    use alloy::providers::ProviderBuilder;
    use alloy::sol_types::SolValue;
    use alloy::transports::mock::Asserter;
    use serde_json::json;

    fn mock_provider(asserter: Asserter) -> DynProvider {
        ProviderBuilder::new()
            .connect_mocked_client(asserter)
            .erased()
    }

    fn sample_runtime_code() -> Bytes {
        Bytes::from(vec![0xCA, 0xFE, 0xBA, 0xBE])
    }

    fn sample_runtime_codehash() -> B256 {
        keccak256(sample_runtime_code())
    }

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
            candidate_amount_in: U256::from(123u64),
            wmnt_funding_amount: U256::from(5_000_000_000_000_000_000u64),
        }
    }

    fn sample_verified_artifact() -> VerifiedArtifactRef {
        VerifiedArtifactRef {
            name: "config/gas_profiles/wmnt_storage_notes.mantle_mainnet.md".to_string(),
            digest: B256::ZERO,
        }
    }

    fn sample_wmnt_storage_layout(balance_mapping_slot: u64) -> StorageLayoutRef {
        StorageLayoutRef {
            name: "config/gas_profiles/wmnt_storage_notes.mantle_mainnet.md".to_string(),
            digest: B256::ZERO,
            balance_mapping_slot,
        }
    }

    fn sample_wmnt_direct_shape(balance_mapping_slot: u64) -> WmntStorageShape {
        WmntStorageShape::Direct {
            address: address!("78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8"),
            runtime_codehash: B256::ZERO,
            verified_artifact: sample_verified_artifact(),
            storage_layout: sample_wmnt_storage_layout(balance_mapping_slot),
        }
    }

    #[test]
    fn balance_mapping_slot_reads_both_shapes() {
        assert_eq!(balance_mapping_slot(&sample_wmnt_direct_shape(7)), 7);
        assert_eq!(
            balance_mapping_slot(&WmntStorageShape::Proxy {
                proxy: address!("78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8"),
                implementation: address!("0000000000000000000000000000000000000001"),
                implementation_codehash: B256::ZERO,
                storage_owner: address!("0000000000000000000000000000000000000001"),
                verified_artifacts: vec![sample_verified_artifact()],
                storage_layout: sample_wmnt_storage_layout(9),
            }),
            9
        );
    }

    #[test]
    fn executor_wmnt_funding_override_matches_erc20_balance_override() {
        let executor = Address::repeat_byte(0x11);
        let amount = U256::from(42u64);
        let shape = sample_wmnt_direct_shape(0);
        let (slot, value) = executor_wmnt_funding_override(executor, amount, &shape);
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
            &sample_wmnt_direct_shape(0),
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
            candidate_amount_in: U256::from(123u64),
            wmnt_funding_amount: U256::from(5_000_000_000_000_000_000u64),
        };
        let storage_layout = sample_storage_layout();
        let patched_runtime = sample_patched_runtime();

        let overrides = build_shadow_state_override(
            wmnt_address,
            &sample_wmnt_direct_shape(0),
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
                runtime_codehash: sample_runtime_codehash(),
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
        pool.pool = expected_create2_derivation(
            PoolProtocol::UniswapV2,
            factory,
            pool.token0,
            pool.token1,
            pool.fee,
            init_code_hash,
        )
        .unwrap()
        .address;
        pool
    }

    #[tokio::test]
    async fn check_pool_provenance_verifies_a_v2_pool_matching_its_create2_address() {
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
        let asserter = Asserter::new();
        asserter.push_success(&Bytes::from(pool.token0.abi_encode()));
        asserter.push_success(&Bytes::from(pool.token1.abi_encode()));
        let provider = mock_provider(asserter);

        let outcome = check_pool_provenance(
            &pool,
            &empty_allowlist,
            &approved,
            &provider,
            BlockId::latest(),
        )
        .await;

        match outcome {
            PoolProvenanceOutcome::Verified(proof) => {
                assert_eq!(proof.protocol, ApprovedPoolProtocol::UniswapV2);
                assert_eq!(proof.factory, factory);
                assert_eq!(proof.init_code_hash, init_code_hash);
                assert_eq!(
                    proof.salt,
                    expected_create2_derivation(
                        PoolProtocol::UniswapV2,
                        factory,
                        pool.token0,
                        pool.token1,
                        pool.fee,
                        init_code_hash
                    )
                    .unwrap()
                    .salt
                );
            }
            other => panic!("expected Verified, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn check_pool_provenance_rejects_a_v2_pool_not_matching_its_create2_address() {
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
        // No responses queued: a CREATE2 mismatch must reject before any RPC is issued.
        let provider = mock_provider(Asserter::new());

        let outcome = check_pool_provenance(
            &pool,
            &empty_allowlist,
            &approved,
            &provider,
            BlockId::latest(),
        )
        .await;

        assert!(matches!(outcome, PoolProvenanceOutcome::Rejected(_)));
    }

    #[tokio::test]
    async fn check_pool_provenance_rejects_a_v2_pool_whose_onchain_token_getters_mismatch() {
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
        let asserter = Asserter::new();
        asserter.push_success(&Bytes::from(pool.token0.abi_encode()));
        asserter.push_success(&Bytes::from(Address::repeat_byte(0x66).abi_encode()));
        let provider = mock_provider(asserter);

        let outcome = check_pool_provenance(
            &pool,
            &empty_allowlist,
            &approved,
            &provider,
            BlockId::latest(),
        )
        .await;

        assert!(matches!(outcome, PoolProvenanceOutcome::Rejected(_)));
    }

    #[tokio::test]
    async fn check_pool_provenance_rejects_when_no_approved_entry_is_committed_for_the_protocol() {
        let mut pool = sample_pool();
        pool.protocol = PoolProtocol::UniswapV3;
        let empty_allowlist = MoeAllowlist {
            schema_version: 1,
            entries: vec![],
        };
        // No responses queued: an unregistered protocol must reject before any RPC.
        let provider = mock_provider(Asserter::new());

        let outcome = check_pool_provenance(
            &pool,
            &empty_allowlist,
            &empty_approved_pools(),
            &provider,
            BlockId::latest(),
        )
        .await;

        assert!(matches!(outcome, PoolProvenanceOutcome::Rejected(_)));
    }

    #[tokio::test]
    async fn check_pool_provenance_accepts_an_allowlisted_moe_pool() {
        let pool = moe_pool();
        let allowlist = allowlist_for(&pool);
        let asserter = Asserter::new();
        asserter.push_success(&sample_runtime_code());
        asserter.push_success(&Bytes::from(pool.token0.abi_encode()));
        asserter.push_success(&Bytes::from(pool.token1.abi_encode()));
        let provider = mock_provider(asserter);

        let outcome = check_pool_provenance(
            &pool,
            &allowlist,
            &empty_approved_pools(),
            &provider,
            BlockId::latest(),
        )
        .await;

        assert_eq!(outcome, PoolProvenanceOutcome::MoeAllowlisted);
    }

    #[tokio::test]
    async fn check_pool_provenance_rejects_a_moe_pool_missing_from_the_allowlist() {
        let pool = moe_pool();
        let empty_allowlist = MoeAllowlist {
            schema_version: 1,
            entries: vec![],
        };
        // No responses queued: a missing allowlist entry must reject before any RPC.
        let provider = mock_provider(Asserter::new());

        let outcome = check_pool_provenance(
            &pool,
            &empty_allowlist,
            &empty_approved_pools(),
            &provider,
            BlockId::latest(),
        )
        .await;

        match outcome {
            PoolProvenanceOutcome::Rejected(reason) => {
                assert!(reason.contains(&pool.pool.to_string()));
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn check_pool_provenance_rejects_a_moe_pool_with_a_mismatched_runtime_codehash() {
        let pool = moe_pool();
        let allowlist = allowlist_for(&pool);
        let asserter = Asserter::new();
        asserter.push_success(&Bytes::from(vec![0xDE, 0xAD]));
        let provider = mock_provider(asserter);

        let outcome = check_pool_provenance(
            &pool,
            &allowlist,
            &empty_approved_pools(),
            &provider,
            BlockId::latest(),
        )
        .await;

        match outcome {
            PoolProvenanceOutcome::Rejected(reason) => {
                assert!(reason.contains("runtime codehash"));
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn route_summary_opportunity_id_covers_topology_only() {
        let base = sample_inputs();
        let mut refunded = base.clone();
        refunded.wmnt_funding_amount = U256::from(1u64);
        assert_eq!(
            ShadowRouteSummary::of(&base).opportunity_id,
            ShadowRouteSummary::of(&refunded).opportunity_id,
            "the fingerprint covers route topology only, not funding/amount fields"
        );
        assert_eq!(ShadowRouteSummary::of(&base).amount_in, U256::from(123u64));
        assert_eq!(
            ShadowRouteSummary::of(&refunded).amount_in,
            U256::from(123u64),
            "ledger amount must remain the sized candidate amount, not shadow funding"
        );

        let mut extra_hop = base.clone();
        extra_hop.pools.push(ShadowPoolOverrideInputs {
            pool: Address::repeat_byte(0xde),
            ..sample_pool()
        });
        assert_ne!(
            ShadowRouteSummary::of(&base).opportunity_id,
            ShadowRouteSummary::of(&extra_hop).opportunity_id
        );
    }

    #[test]
    fn route_summary_ordered_pools_preserves_route_order() {
        let mut inputs = sample_inputs();
        inputs.pools.push(ShadowPoolOverrideInputs {
            pool: Address::repeat_byte(0xde),
            ..sample_pool()
        });
        assert_eq!(
            ShadowRouteSummary::of(&inputs).ordered_pools,
            vec![sample_pool().pool, Address::repeat_byte(0xde)]
        );
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
