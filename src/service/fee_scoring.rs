//! Discovery-side measured fee scoring (WHI-949 / G-2).
//!
//! Live discovery ranks candidates with the **same** pure function the send path
//! uses: [`crate::execution::fee_plan_cost`] / [`FeePolicy::build`]. Offline
//! fixtures keep the fixed hop table in [`crate::service::gas::GasConfig`].
//!
//! Tip fee stamping for the initial live discovery pass (WHI-975) also lives
//! here: resolve the tip via the shared WHI-967 helper, then fail closed with a
//! message that distinguishes "never fetched" from "fetched zeros".
//!
//! The gas-profile **support predicate** ([`topology_profile_support`], WHI-1421)
//! also lives here: the one answer to "could the profile ever price this
//! topology?" shared by the WHI-1408 startup gate and the WHI-1409 pre-simulation
//! filter.

use crate::amms::amm::AMM;
use crate::execution::{
    fee_plan_cost, BinCrossingBucket, BlockFeeContext, FeePlanError, FeePolicy, FeeScoreKey,
    GasProfileError, GasQuote, ProtocolKind, RouteKey, RouteResolution, RuntimeGasProfile,
    RuntimeGasProfileError, TickCrossingBucket,
};
use crate::service::pool_universe::LoadedPoolUniverse;
use crate::service::select::protocol_kind_of_amm;
use crate::state_space::resolve_canonical_tip;
use crate::state_space::PoolProtocol;
use alloy::consensus::BlockHeader;
use alloy::network::primitives::{BlockResponse, HeaderResponse};
use alloy::network::Network;
use alloy::primitives::{B256, U256};
use alloy::providers::Provider;
use alloy::rpc::types::Block;
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

/// Measured fee inputs shared by discovery ranking and send admission.
///
/// Holds the O(1) profile index + EIP-1559 policy + live block fee context.
/// No per-candidate RPC.
#[derive(Clone, Debug)]
pub struct MeasuredFeeScoring {
    pub gas_profile: Arc<RuntimeGasProfile>,
    pub priority_fee_per_gas: u128,
    pub block_gas_reserve: u64,
    pub fee_context: BlockFeeContext,
}

/// Fail-closed discovery fee errors (profile lookup or FeePolicy rejections).
#[derive(Debug, thiserror::Error)]
pub enum DiscoveryFeeError {
    #[error(transparent)]
    Profile(#[from] RuntimeGasProfileError),
    #[error(transparent)]
    FeePlan(#[from] FeePlanError),
}

impl MeasuredFeeScoring {
    pub fn new(
        gas_profile: Arc<RuntimeGasProfile>,
        priority_fee_per_gas: u128,
        block_gas_reserve: u64,
        fee_context: BlockFeeContext,
    ) -> Self {
        Self {
            gas_profile,
            priority_fee_per_gas,
            block_gas_reserve,
            fee_context,
        }
    }

    pub fn policy(&self) -> FeePolicy {
        FeePolicy::new(self.priority_fee_per_gas, self.block_gas_reserve)
    }

    pub fn fee_score_key(&self) -> FeeScoreKey {
        FeeScoreKey::from_policy_and_context(self.policy(), &self.fee_context)
    }

    /// G-2 surface for G-1: `fee_plan_cost(route_key, fee_context)`.
    ///
    /// O(1) profile lookup + shared [`fee_plan_cost`]. Unknown / unapproved
    /// route buckets fail closed (no hop-table fallback).
    pub fn fee_plan_cost(&self, route_key: &RouteKey) -> Result<U256, DiscoveryFeeError> {
        let quote = self.gas_profile.quote(route_key)?;
        Ok(fee_plan_cost(&quote, &self.fee_context, self.policy())?)
    }

    /// Same as [`Self::fee_plan_cost`] but returns the full quote + cost for tests.
    pub fn quote_and_cost(
        &self,
        route_key: &RouteKey,
    ) -> Result<(GasQuote, U256), DiscoveryFeeError> {
        let quote = self.gas_profile.quote(route_key)?;
        let cost = fee_plan_cost(&quote, &self.fee_context, self.policy())?;
        Ok((quote, cost))
    }
}

/// Classify a discovery fee failure for metrics.
pub fn discovery_fee_reject_reason(err: &DiscoveryFeeError) -> &'static str {
    use crate::metrics::reject_reason;
    match err {
        DiscoveryFeeError::Profile(RuntimeGasProfileError::UnknownRoute(_)) => {
            reject_reason::UNKNOWN_ROUTE
        }
        DiscoveryFeeError::Profile(RuntimeGasProfileError::UnapprovedRoute(_)) => {
            reject_reason::UNAPPROVED_ROUTE
        }
        DiscoveryFeeError::FeePlan(FeePlanError::GasLimitExceedsBlockReserve { .. }) => {
            reject_reason::GAS_RESERVE
        }
        DiscoveryFeeError::FeePlan(FeePlanError::InvalidGasQuote) => reject_reason::GAS_SCREEN,
        _ => reject_reason::GAS_SCREEN,
    }
}

/// Tip header fields needed to stamp measured discovery scoring (WHI-949 / WHI-975).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryTipFeeFields {
    pub block_number: u64,
    pub block_hash: B256,
    pub parent_hash: B256,
    pub timestamp: u64,
    pub base_fee_per_gas: u128,
    pub block_gas_limit: u64,
}

/// Fail-closed tip fee context for measured live discovery (WHI-975).
///
/// Distinguishes a tip that was **never resolved** from a tip header that was
/// fetched but carried genuine zero gas fields — so operators do not chase
/// "chain sent zeros" when the block body simply never arrived.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DiscoveryTipFeeError {
    #[error(
        "live discovery tip was never resolved for measured FeePolicy scoring \
         (WHI-975); tip resolution failed: {detail}; refusing hop-table fallback"
    )]
    NotResolved { detail: String },
    #[error(
        "live discovery tip header carried zero base_fee_per_gas or block_gas_limit \
         (base_fee_per_gas={base_fee_per_gas}, block_gas_limit={block_gas_limit}) \
         for measured FeePolicy scoring (WHI-949); refusing hop-table fallback"
    )]
    ZeroGasFields {
        base_fee_per_gas: u128,
        block_gas_limit: u64,
    },
}

/// Pure WHI-949 guard over fields taken from a **successfully fetched** tip.
pub(crate) fn require_nonzero_tip_fee_fields(
    base_fee_per_gas: u128,
    block_gas_limit: u64,
) -> Result<(), DiscoveryTipFeeError> {
    if base_fee_per_gas == 0 || block_gas_limit == 0 {
        Err(DiscoveryTipFeeError::ZeroGasFields {
            base_fee_per_gas,
            block_gas_limit,
        })
    } else {
        Ok(())
    }
}

/// Resolve the tip with the shared WHI-967 retry, then require non-zero fee fields.
///
/// Used by the live bot's one-shot discovery stamp (WHI-975). Retries `Ok(None)`
/// tip bodies; never falls back to default zeros.
pub async fn resolve_discovery_tip_fee_fields<N, P>(
    provider: &P,
) -> Result<DiscoveryTipFeeFields, DiscoveryTipFeeError>
where
    P: Provider<N>,
    N: Network<BlockResponse = Block>,
{
    let (tip, block) =
        resolve_canonical_tip(provider)
            .await
            .map_err(|e| DiscoveryTipFeeError::NotResolved {
                detail: e.to_string(),
            })?;
    let header = block.header();
    let base_fee_per_gas = header.base_fee_per_gas().map(u128::from).unwrap_or(0);
    let block_gas_limit = header.gas_limit();
    require_nonzero_tip_fee_fields(base_fee_per_gas, block_gas_limit)?;
    Ok(DiscoveryTipFeeFields {
        block_number: tip,
        block_hash: header.hash(),
        parent_hash: header.parent_hash(),
        timestamp: header.timestamp(),
        base_fee_per_gas,
        block_gas_limit,
    })
}

/// All structurally valid [`RouteKey`] crossing-bucket variants for an
/// ordered protocol sequence (WHI-1409).
///
/// A pure-V2 sequence has exactly one (bucket-less) key. A V3-containing
/// sequence varies over every [`TickCrossingBucket`]; a Moe-containing
/// sequence varies over every [`BinCrossingBucket`]; a sequence with both
/// varies over their full cross product. Never guess a single bucket here:
/// `RouteKey::new` alone defaults both buckets to zero, which is exactly the
/// WHI-1421 startup-gate bug.
pub(crate) fn route_key_candidates(
    protocols: &[ProtocolKind],
) -> Result<Vec<RouteKey>, GasProfileError> {
    let base = RouteKey::new(protocols.to_vec())?;
    let has_v3 = protocols.contains(&ProtocolKind::V3);
    let has_moe = protocols.contains(&ProtocolKind::Moe);

    let mut keys = Vec::new();
    match (has_v3, has_moe) {
        (false, false) => keys.push(base),
        (true, false) => {
            for tick in TickCrossingBucket::ALL {
                keys.push(base.clone().with_v3_ticks(tick));
            }
        }
        (false, true) => {
            for bin in BinCrossingBucket::ALL {
                keys.push(base.clone().with_moe_bins(bin));
            }
        }
        (true, true) => {
            for tick in TickCrossingBucket::ALL {
                for bin in BinCrossingBucket::ALL {
                    keys.push(base.clone().with_v3_ticks(tick).with_moe_bins(bin));
                }
            }
        }
    }
    Ok(keys)
}

/// Verdict of the shared gas-profile support predicate for one ordered
/// protocol topology (WHI-1421).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProfileSupport {
    /// At least one crossing-bucket variant holds an active (not invalidated)
    /// approval. The profile *could* price this topology; whether live
    /// liquidity and input sizes actually land in an approved bucket is decided
    /// per sample by simulation, not here.
    Supported,
    /// Some variant has a profile entry, but none is actively approved
    /// (unsupported, research-only or invalidated).
    Unapproved,
    /// No variant of this topology has any profile entry at any bucket.
    Unknown,
}

/// Fail-closed errors from [`topology_profile_support`]. Every caller treats an
/// error as "not supported".
#[derive(Debug, thiserror::Error)]
pub enum ProfileSupportError {
    #[error("failed to build route keys for topology {0:?}: {1}")]
    RouteKey(Vec<ProtocolKind>, GasProfileError),
    #[error("gas profile support check failed closed: {0}")]
    Profile(RuntimeGasProfileError),
}

/// The one gas-profile support predicate (WHI-1421), shared by the WHI-1408
/// startup gate (universe, synced-pools and first-live-block variants, all via
/// [`evaluate_universe_gas_profile_compatibility`]) and the WHI-1409
/// pre-simulation filter (`path_index::optimize_path`).
///
/// A topology is unsupported only when **no** crossing-bucket variant from
/// [`route_key_candidates`] has an active approval. Passing is necessary, not
/// sufficient: it says nothing about whether current liquidity reaches an
/// approved bucket.
///
/// Classification uses the metric-free [`RuntimeGasProfile::inspect_route`],
/// but every `Approved` answer is confirmed through
/// [`RuntimeGasProfile::quote`] before it counts. `inspect_route` fails open on
/// a poisoned invalidation lock (DI-45); `quote` is invalidation-aware and
/// fails closed on poison, so a poisoned lock yields `Err` here, never
/// `Supported`. Only non-`Approved` answers skip the confirmation, and those
/// already mean "not supported". Cost: one `gas_profile_quote_total{hit}`
/// increment per supported topology checked.
pub fn topology_profile_support(
    profile: &RuntimeGasProfile,
    protocols: &[ProtocolKind],
) -> Result<ProfileSupport, ProfileSupportError> {
    profile_support_with(
        protocols,
        |key| profile.inspect_route(key),
        |key| profile.quote(key),
    )
}

/// [`topology_profile_support`] over injected lookups, so a test can reproduce
/// a poisoned invalidation lock (whose lock is private to `src/execution`).
fn profile_support_with(
    protocols: &[ProtocolKind],
    inspect: impl Fn(&RouteKey) -> RouteResolution,
    quote: impl Fn(&RouteKey) -> Result<GasQuote, RuntimeGasProfileError>,
) -> Result<ProfileSupport, ProfileSupportError> {
    let candidates = route_key_candidates(protocols)
        .map_err(|e| ProfileSupportError::RouteKey(protocols.to_vec(), e))?;
    let mut saw_entry = false;
    for key in &candidates {
        match inspect(key) {
            RouteResolution::Approved(_) => match quote(key) {
                Ok(_) => return Ok(ProfileSupport::Supported),
                // Invalidated between the two reads: an entry, not an approval.
                Err(RuntimeGasProfileError::UnapprovedRoute(_)) => saw_entry = true,
                Err(e) => return Err(ProfileSupportError::Profile(e)),
            },
            RouteResolution::Unsupported(_) | RouteResolution::ResearchOnly => saw_entry = true,
            RouteResolution::Unknown => {}
        }
    }
    Ok(if saw_entry {
        ProfileSupport::Unapproved
    } else {
        ProfileSupport::Unknown
    })
}

/// Bucket-less label for an ordered protocol topology, e.g. `h2:v3+v3`.
pub(crate) fn topology_label(protocols: &[ProtocolKind]) -> String {
    let protos: Vec<&str> = protocols.iter().map(|p| p.as_str()).collect();
    format!("h{}:{}", protocols.len(), protos.join("+"))
}

/// Census of universe-generated topologies evaluated against the loaded gas profile
/// (WHI-1408), each classified by the shared [`topology_profile_support`] predicate
/// (WHI-1421).
///
/// The census is **count-based**: it enumerates every protocol sequence the
/// per-protocol pool counts allow, ignoring which tokens the pools trade. It
/// therefore over-approximates the settlement cycles discovery can form (e.g.
/// `[v2,v2]` counts whenever two v2 pools exist, even if they share no WMNT/X
/// pair). A non-empty supported set is **necessary, not sufficient** for live
/// discovery; the WHI-1411 rejection-aware liveness alarm is the runtime backstop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UniverseTopologyCensus {
    pub pool_universe_fingerprint: B256,
    pub per_protocol_counts: Vec<(String, usize)>,
    pub gas_profile_identity: String,
    pub approved_routes_in_profile: Vec<String>,
    pub topologies_total: usize,
    /// [`ProfileSupport::Supported`] topologies (bucket-less labels).
    pub topologies_supported: Vec<String>,
    /// [`ProfileSupport::Unapproved`] topologies.
    pub topologies_unapproved: Vec<String>,
    /// [`ProfileSupport::Unknown`] topologies.
    pub topologies_unknown: Vec<String>,
}

impl fmt::Display for UniverseTopologyCensus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "Universe fingerprint: {}",
            self.pool_universe_fingerprint
        )?;
        let counts_str = self
            .per_protocol_counts
            .iter()
            .map(|(proto, count)| format!("{proto}={count}"))
            .collect::<Vec<_>>()
            .join(", ");
        writeln!(f, "Per-protocol pool counts: {counts_str}")?;
        writeln!(f, "Gas profile identity: {}", self.gas_profile_identity)?;
        writeln!(
            f,
            "Gas profile approved routes ({}):",
            self.approved_routes_in_profile.len()
        )?;
        for route in &self.approved_routes_in_profile {
            writeln!(f, "  - {route}")?;
        }
        writeln!(
            f,
            "Universe generated topologies ({} total; count-based, any crossing bucket):",
            self.topologies_total
        )?;
        for (label, keys) in [
            (
                "supported (active approval at some bucket)",
                &self.topologies_supported,
            ),
            ("unapproved at every bucket", &self.topologies_unapproved),
            ("unknown (no entry at any bucket)", &self.topologies_unknown),
        ] {
            writeln!(f, "  {label} ({}):", keys.len())?;
            for key in keys {
                writeln!(f, "    - {key}")?;
            }
        }
        writeln!(
            f,
            "Note: this gate is count-based and over-approximates settlement cycles; \
             passing it is necessary, not sufficient (the WHI-1411 liveness alarm is the \
             runtime backstop)."
        )
    }
}

/// Errors raised during universe gas profile compatibility validation (WHI-1408).
#[derive(Debug, thiserror::Error)]
pub enum UniverseGasProfileError {
    #[error("Gas profile universe intersection is empty: no topology the loaded universe can generate has an active approval at any crossing bucket in the gas profile.\n{0}Fatal: 100% of candidate paths would be rejected at the gas gate (GAS_PROFILE). Refusing to start.")]
    EmptyApprovedRouteIntersection(Box<UniverseTopologyCensus>),
    #[error(transparent)]
    ProfileSupport(#[from] ProfileSupportError),
}

fn protocol_kind_of_pool_protocol(p: PoolProtocol) -> ProtocolKind {
    match p {
        PoolProtocol::UniswapV2 => ProtocolKind::V2,
        PoolProtocol::UniswapV3 | PoolProtocol::Agni => ProtocolKind::V3,
        PoolProtocol::MoeLb => ProtocolKind::Moe,
    }
}

fn count_universe_protocols(universe: &LoadedPoolUniverse) -> HashMap<ProtocolKind, usize> {
    let mut counts = HashMap::new();
    for row in &universe.rows {
        *counts
            .entry(protocol_kind_of_pool_protocol(row.protocol))
            .or_default() += 1;
    }
    counts
}

fn count_pools_protocols(pools: &[AMM]) -> HashMap<ProtocolKind, usize> {
    let mut counts = HashMap::new();
    for pool in pools {
        *counts.entry(protocol_kind_of_amm(pool)).or_default() += 1;
    }
    counts
}

/// Enumerate every ordered protocol topology that can be formed from the available protocol
/// counts for hop lengths in 2..=max_hops. Bucket-less: crossing buckets are the shared
/// predicate's job ([`topology_profile_support`]), never a zero-bucket guess here.
fn generate_universe_topologies(
    counts: &HashMap<ProtocolKind, usize>,
    max_hops: usize,
) -> Vec<Vec<ProtocolKind>> {
    let mut topologies = Vec::new();
    let available: Vec<ProtocolKind> = [ProtocolKind::V2, ProtocolKind::V3, ProtocolKind::Moe]
        .into_iter()
        .filter(|k| counts.get(k).copied().unwrap_or(0) > 0)
        .collect();

    for hop_count in 2..=max_hops {
        let mut current = Vec::with_capacity(hop_count);
        enumerate_permutations(&available, counts, hop_count, &mut current, &mut topologies);
    }
    topologies
}

fn enumerate_permutations(
    available: &[ProtocolKind],
    counts: &HashMap<ProtocolKind, usize>,
    target_len: usize,
    current: &mut Vec<ProtocolKind>,
    out: &mut Vec<Vec<ProtocolKind>>,
) {
    if current.len() == target_len {
        out.push(current.clone());
        return;
    }

    for &proto in available {
        let needed = current.iter().filter(|&&p| p == proto).count() + 1;
        let limit = counts.get(&proto).copied().unwrap_or(0);
        if needed <= limit {
            current.push(proto);
            enumerate_permutations(available, counts, target_len, current, out);
            current.pop();
        }
    }
}

/// Classify the universe's generated topologies with the shared
/// [`topology_profile_support`] predicate (WHI-1421).
pub(crate) fn evaluate_universe_gas_profile_compatibility(
    fingerprint: B256,
    per_protocol_counts: &HashMap<ProtocolKind, usize>,
    gas_profile: &RuntimeGasProfile,
    max_hops: usize,
) -> Result<UniverseTopologyCensus, UniverseGasProfileError> {
    let topologies = generate_universe_topologies(per_protocol_counts, max_hops);
    let mut supported = Vec::new();
    let mut unapproved = Vec::new();
    let mut unknown = Vec::new();

    for topo in &topologies {
        let bucket = match topology_profile_support(gas_profile, topo)? {
            ProfileSupport::Supported => &mut supported,
            ProfileSupport::Unapproved => &mut unapproved,
            ProfileSupport::Unknown => &mut unknown,
        };
        bucket.push(topology_label(topo));
    }

    let approved_in_profile = gas_profile
        .approved_route_keys()
        .into_iter()
        .map(|k| k.key_string())
        .collect();

    let counts_vec = vec![
        (
            "agni-v2".to_string(),
            per_protocol_counts
                .get(&ProtocolKind::V2)
                .copied()
                .unwrap_or(0),
        ),
        (
            "agni-v3".to_string(),
            per_protocol_counts
                .get(&ProtocolKind::V3)
                .copied()
                .unwrap_or(0),
        ),
        (
            "moe".to_string(),
            per_protocol_counts
                .get(&ProtocolKind::Moe)
                .copied()
                .unwrap_or(0),
        ),
    ];

    Ok(UniverseTopologyCensus {
        pool_universe_fingerprint: fingerprint,
        per_protocol_counts: counts_vec,
        gas_profile_identity: gas_profile.artifact_digest().to_string(),
        approved_routes_in_profile: approved_in_profile,
        topologies_total: topologies.len(),
        topologies_supported: supported,
        topologies_unapproved: unapproved,
        topologies_unknown: unknown,
    })
}

/// Assert at least one generated topology is profile-supported (shared predicate) for the
/// given per-protocol counts; shared tail for the universe- and pools-shaped entry points.
///
/// Count-based and over-approximating, so passing is **necessary, not sufficient** for live
/// discovery (see [`UniverseTopologyCensus`]); failing is conclusive. A poisoned profile
/// state fails closed with [`UniverseGasProfileError::ProfileSupport`].
///
/// A pool set with zero pools of every protocol is deliberately treated as compatible here —
/// that is an "unloaded universe" state with its own dedicated fail-closed check elsewhere
/// (e.g. `bot.rs`'s `amms.is_empty()` bail), not a gas-profile mismatch. Any other pool set
/// that cannot generate a single hop-2+ topology (e.g. exactly one pool of one protocol) is a
/// genuine "nothing can ever be priced" condition and must fail closed exactly like a
/// non-empty topology set with zero approved resolutions.
fn assert_gas_profile_compatibility(
    fingerprint: B256,
    counts: &HashMap<ProtocolKind, usize>,
    gas_profile: &RuntimeGasProfile,
    max_hops: usize,
) -> Result<(), UniverseGasProfileError> {
    if counts.values().sum::<usize>() == 0 {
        return Ok(());
    }
    let census =
        evaluate_universe_gas_profile_compatibility(fingerprint, counts, gas_profile, max_hops)?;
    if census.topologies_supported.is_empty() {
        return Err(UniverseGasProfileError::EmptyApprovedRouteIntersection(
            Box::new(census),
        ));
    }
    Ok(())
}

/// Validate that a loaded universe can form at least one profile-supported topology
/// (necessary, not sufficient; see [`assert_gas_profile_compatibility`]).
pub fn assert_universe_gas_profile_compatibility(
    universe: &LoadedPoolUniverse,
    gas_profile: &RuntimeGasProfile,
    max_hops: usize,
) -> Result<(), UniverseGasProfileError> {
    let counts = count_universe_protocols(universe);
    assert_gas_profile_compatibility(universe.fingerprint, &counts, gas_profile, max_hops)
}

/// Validate that in-memory AMM pools can form at least one profile-supported topology
/// (necessary, not sufficient; see [`assert_gas_profile_compatibility`]).
pub fn assert_pools_gas_profile_compatibility(
    fingerprint: B256,
    pools: &[AMM],
    gas_profile: &RuntimeGasProfile,
    max_hops: usize,
) -> Result<(), UniverseGasProfileError> {
    let counts = count_pools_protocols(pools);
    assert_gas_profile_compatibility(fingerprint, &counts, gas_profile, max_hops)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::{
        ExecutorIdentity, ProtocolKind, RuntimeProfileConfig, TickCrossingBucket,
    };
    use crate::state_space::TIP_RESOLUTION_MAX_ATTEMPTS;
    use alloy::network::Ethereum;
    use alloy::primitives::B256;
    use alloy::providers::{mock::Asserter, ProviderBuilder};
    use alloy::rpc::types::Header as RpcHeader;
    use std::path::PathBuf;

    fn load_mainnet_profile() -> Arc<RuntimeGasProfile> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("config/gas_profiles/mantle_mainnet_v1.json");
        Arc::new(
            RuntimeGasProfile::load(&path, RuntimeProfileConfig::mantle_mainnet(Vec::new()))
                .expect("load mainnet profile"),
        )
    }

    fn scoring(
        priority: u128,
        reserve: u64,
        base_fee: u128,
        block_gas_limit: u64,
    ) -> MeasuredFeeScoring {
        MeasuredFeeScoring::new(
            load_mainnet_profile(),
            priority,
            reserve,
            BlockFeeContext {
                block_number: 1,
                block_hash: B256::ZERO,
                base_fee_per_gas: base_fee,
                block_gas_limit,
            },
        )
    }

    fn test_hash(byte: u8) -> B256 {
        B256::repeat_byte(byte)
    }

    fn mock_block_with_fees(
        number: u64,
        hash: B256,
        parent_hash: B256,
        base_fee: Option<u64>,
        gas_limit: u64,
    ) -> Block {
        let mut inner = alloy::consensus::Header::default();
        inner.number = number;
        inner.parent_hash = parent_hash;
        inner.timestamp = number;
        inner.base_fee_per_gas = base_fee;
        inner.gas_limit = gas_limit;
        let mut header = RpcHeader::new(inner);
        header.hash = hash;
        Block::empty(header)
    }

    #[test]
    fn zero_gas_fields_message_distinct_from_not_resolved() {
        let zero = require_nonzero_tip_fee_fields(0, 0).unwrap_err();
        let not_resolved = DiscoveryTipFeeError::NotResolved {
            detail: "Canonical tip block not found at number 42".into(),
        };
        let zero_msg = zero.to_string();
        let not_resolved_msg = not_resolved.to_string();
        assert!(
            zero_msg.contains("carried zero base_fee_per_gas or block_gas_limit"),
            "zero-fields message: {zero_msg}"
        );
        assert!(
            not_resolved_msg.contains("was never resolved"),
            "not-resolved message: {not_resolved_msg}"
        );
        assert_ne!(zero_msg, not_resolved_msg);
        assert!(matches!(
            zero,
            DiscoveryTipFeeError::ZeroGasFields {
                base_fee_per_gas: 0,
                block_gas_limit: 0
            }
        ));
    }

    /// WHI-975: first get_block_by_number returns null (race); second succeeds
    /// with non-zero fee fields — discovery tip stamp proceeds.
    #[tokio::test]
    async fn tip_fee_resolution_retries_null_then_succeeds() {
        let tip = 99u64;
        let tip_hash = test_hash(0x99);
        let parent = test_hash(0x98);
        let asserter = Asserter::new();
        // Attempt 1: number then null body (load-balanced race).
        asserter.push_success(&tip);
        asserter.push_success(&Option::<Block>::None);
        // Attempt 2: number then header with Mantle-like non-zero fees.
        asserter.push_success(&tip);
        asserter.push_success(&Some(mock_block_with_fees(
            tip,
            tip_hash,
            parent,
            Some(50_000_000_000),
            60_000_000,
        )));

        let provider = ProviderBuilder::new().connect_mocked_client(asserter.clone());
        let fields = resolve_discovery_tip_fee_fields::<Ethereum, _>(&provider)
            .await
            .expect("null tip body must be retried and then succeed");

        assert_eq!(fields.block_number, tip);
        assert_eq!(fields.block_hash, tip_hash);
        assert_eq!(fields.parent_hash, parent);
        assert_eq!(fields.base_fee_per_gas, 50_000_000_000);
        assert_eq!(fields.block_gas_limit, 60_000_000);
        assert!(asserter.read_q().is_empty());
    }

    /// WHI-975: a successfully fetched header with genuine zeros aborts with
    /// the zero-fields message, not the not-resolved message.
    #[tokio::test]
    async fn tip_fee_resolution_aborts_on_genuine_zero_fields() {
        let tip = 77u64;
        let tip_hash = test_hash(0x77);
        let parent = test_hash(0x76);
        let asserter = Asserter::new();
        asserter.push_success(&tip);
        // Default header has base_fee=None (→ 0) and gas_limit=0.
        asserter.push_success(&Some(mock_block_with_fees(tip, tip_hash, parent, None, 0)));

        let provider = ProviderBuilder::new().connect_mocked_client(asserter.clone());
        let err = resolve_discovery_tip_fee_fields::<Ethereum, _>(&provider)
            .await
            .expect_err("genuine zero gas fields must fail closed");

        let msg = err.to_string();
        assert!(
            msg.contains("carried zero base_fee_per_gas or block_gas_limit"),
            "expected zero-fields wording, got: {msg}"
        );
        assert!(
            !msg.contains("was never resolved"),
            "must not blame tip resolution when the header was fetched: {msg}"
        );
        assert!(matches!(
            err,
            DiscoveryTipFeeError::ZeroGasFields {
                base_fee_per_gas: 0,
                block_gas_limit: 0
            }
        ));
        assert!(asserter.read_q().is_empty());
    }

    /// WHI-975: exhausted null tip bodies map to NotResolved, not ZeroGasFields.
    #[tokio::test]
    async fn tip_fee_resolution_exhausted_is_not_resolved() {
        let tip = 55u64;
        let asserter = Asserter::new();
        for _ in 0..TIP_RESOLUTION_MAX_ATTEMPTS {
            asserter.push_success(&tip);
            asserter.push_success(&Option::<Block>::None);
        }

        let provider = ProviderBuilder::new().connect_mocked_client(asserter.clone());
        let err = resolve_discovery_tip_fee_fields::<Ethereum, _>(&provider)
            .await
            .expect_err("exhausted tip resolution must fail as not-resolved");

        let msg = err.to_string();
        assert!(
            msg.contains("was never resolved"),
            "expected not-resolved wording, got: {msg}"
        );
        assert!(
            !msg.contains("carried zero"),
            "must not claim zeros when the tip was never fetched: {msg}"
        );
        assert!(matches!(err, DiscoveryTipFeeError::NotResolved { .. }));
        assert!(asserter.read_q().is_empty());
    }

    #[test]
    fn discovery_and_executor_share_wei_identical_cost() {
        let priority = 100_000u128;
        let reserve = 1u64;
        let base_fee = 50_000_000u128;
        let m = scoring(priority, reserve, base_fee, 30_000_000);
        let route = RouteKey::new(vec![ProtocolKind::V2, ProtocolKind::V2]).unwrap();

        // Discovery surface: MeasuredFeeScoring::fee_plan_cost (profile lookup + cost).
        let discovery_cost = m.fee_plan_cost(&route).expect("approved v2/v2");
        // Executor surface: quote from profile + FeePolicy::build (send_path path).
        let quote = m.gas_profile.quote(&route).expect("quote");
        let executor_plan = FeePolicy::new(priority, reserve)
            .build(&quote, &m.fee_context)
            .expect("executor FeePolicy::build");

        assert_eq!(discovery_cost, executor_plan.expected_gas_cost);
        let expected = U256::from(quote.expected_gas_used)
            * U256::from(base_fee.checked_add(priority).unwrap());
        assert_eq!(discovery_cost, expected);
        assert_eq!(executor_plan.max_priority_fee_per_gas, priority);
    }

    #[test]
    fn non_zero_priority_fee_is_included_in_cost() {
        let base_fee = 40u128;
        let with_priority = scoring(10, 1, base_fee, 30_000_000);
        let zero_priority = scoring(0, 1, base_fee, 30_000_000);
        let route = RouteKey::new(vec![ProtocolKind::V2, ProtocolKind::V2]).unwrap();

        let a = with_priority.fee_plan_cost(&route).unwrap();
        let b = zero_priority.fee_plan_cost(&route).unwrap();
        assert!(a > b, "priority fee must increase expected_gas_cost");
        assert_eq!(
            a - b,
            U256::from(
                with_priority
                    .gas_profile
                    .quote(&route)
                    .unwrap()
                    .expected_gas_used
            ) * U256::from(10u64)
        );
    }

    #[test]
    fn gas_limit_exceeds_block_reserve_rejected_on_discovery() {
        // Tiny block gas limit so approved profile gas_limit exceeds available.
        let m = scoring(10, 100, 50, 200);
        let route = RouteKey::new(vec![ProtocolKind::V2, ProtocolKind::V2]).unwrap();
        let err = m.fee_plan_cost(&route).unwrap_err();
        assert!(matches!(
            err,
            DiscoveryFeeError::FeePlan(FeePlanError::GasLimitExceedsBlockReserve { .. })
        ));
        assert_eq!(
            discovery_fee_reject_reason(&err),
            crate::metrics::reject_reason::GAS_RESERVE
        );
    }

    #[test]
    fn unapproved_route_bucket_fails_closed_with_unapproved_metric_label() {
        let m = scoring(10, 1, 50, 30_000_000);
        // 3-hop pure v2 is unsupported in the pinned mainnet profile.
        let route =
            RouteKey::new(vec![ProtocolKind::V2, ProtocolKind::V2, ProtocolKind::V2]).unwrap();
        let err = m.fee_plan_cost(&route).unwrap_err();
        assert!(matches!(
            err,
            DiscoveryFeeError::Profile(RuntimeGasProfileError::UnapprovedRoute(_))
        ));
        assert_eq!(
            discovery_fee_reject_reason(&err),
            crate::metrics::reject_reason::UNAPPROVED_ROUTE
        );
    }

    #[test]
    fn unknown_route_bucket_fails_closed_with_unknown_metric_label() {
        use crate::execution::TickCrossingBucket;
        let m = scoring(10, 1, 50, 30_000_000);
        // WHI-1422: every 2..3-hop key now has an explicit entry, so use a 4-hop
        // key the profile never lists -> UnknownRoute.
        let route = RouteKey::new(vec![
            ProtocolKind::V2,
            ProtocolKind::V3,
            ProtocolKind::V2,
            ProtocolKind::V3,
        ])
        .unwrap()
        .with_v3_ticks(TickCrossingBucket::Zero);
        let err = m.fee_plan_cost(&route).unwrap_err();
        assert!(matches!(
            err,
            DiscoveryFeeError::Profile(RuntimeGasProfileError::UnknownRoute(_))
        ));
        assert_eq!(
            discovery_fee_reject_reason(&err),
            crate::metrics::reject_reason::UNKNOWN_ROUTE
        );
    }

    #[test]
    fn fee_score_key_tracks_priority_and_reserve() {
        let a = scoring(100_000, 1, 50, 30_000_000).fee_score_key();
        let b = scoring(200_000, 1, 50, 30_000_000).fee_score_key();
        let c = scoring(100_000, 2, 50, 30_000_000).fee_score_key();
        let d = scoring(100_000, 1, 50, 29_000_000).fee_score_key();
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, d);
        assert_eq!(a.base_fee_per_gas, b.base_fee_per_gas);
    }

    /// WHI-1422 AC-2/AC-3: over the committed universe's topology set (the same
    /// count-based census the startup gate uses, 2..=`DEFAULT_MAX_HOPS` hops), the
    /// pinned mainnet profile has **zero** `UnknownRoute`: the shared predicate never
    /// answers `Unknown`, and every crossing-bucket variant of every topology
    /// (including every 3-hop v3/moe class) resolves to an explicit entry —
    /// Approved, or Unsupported with a non-empty reason.
    #[tokio::test]
    async fn committed_universe_topologies_have_zero_unknown_route() {
        use crate::service::pool_universe::PoolUniverseSource;
        use crate::service::unified_universe::UnifiedPoolUniverseSource;
        use alloy::primitives::address;

        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let universe = UnifiedPoolUniverseSource::new(root.join("data/pool_universe.csv"))
            .load(5000, address!("78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8"))
            .await
            .expect("load committed universe");
        let profile = load_mainnet_profile();
        let counts = count_universe_protocols(&universe);
        let max_hops = crate::arbitrage::DEFAULT_MAX_HOPS;

        let census = evaluate_universe_gas_profile_compatibility(
            universe.fingerprint,
            &counts,
            &profile,
            max_hops,
        )
        .expect("census");
        // Not vacuous: all three protocols have >= 3 pools, so every ordered
        // 2- and 3-hop sequence over {v2, v3, moe} is generated (9 + 27).
        assert_eq!(census.topologies_total, 36, "{census}");
        assert!(census.topologies_unknown.is_empty(), "{census}");

        let mut keys_checked = 0usize;
        for topo in generate_universe_topologies(&counts, max_hops) {
            assert_ne!(
                topology_profile_support(&profile, &topo).unwrap(),
                ProfileSupport::Unknown,
                "{}",
                topology_label(&topo)
            );
            for key in route_key_candidates(&topo).unwrap() {
                keys_checked += 1;
                match profile.inspect_route(&key) {
                    RouteResolution::Approved(_) => {
                        profile.quote(&key).expect("approved entry must quote");
                    }
                    RouteResolution::Unsupported(reason) => assert!(
                        !reason.trim().is_empty(),
                        "{} is Unsupported without a reason",
                        key.key_string()
                    ),
                    other => panic!("{} has no explicit entry: {other:?}", key.key_string()),
                }
            }
        }
        // 2-hop: 1 pure-v2 + 6 single-axis x 4 + 2 mixed x 16 = 57;
        // 3-hop: 1 pure-v2 + 14 single-axis x 4 + 12 mixed x 16 = 249.
        assert_eq!(keys_checked, 57 + 249);
    }

    #[test]
    fn identity_config_still_loads() {
        // Sanity: RuntimeGasProfile identity pins stay green under G-2 wiring.
        let _ = load_mainnet_profile();
        let _ = ExecutorIdentity::mantle_mainnet();
        let _ = TickCrossingBucket::Zero;
    }

    fn make_test_universe(
        v2_count: usize,
        v3_count: usize,
        moe_count: usize,
    ) -> LoadedPoolUniverse {
        use crate::state_space::PoolUniverseRow;
        use alloy::primitives::Address;
        let mut rows = Vec::new();
        let factory = Address::repeat_byte(0x01);
        let mut addr_byte = 1u8;
        for _ in 0..v2_count {
            rows.push(PoolUniverseRow {
                protocol: PoolProtocol::UniswapV2,
                factory,
                pool: Address::repeat_byte(addr_byte),
                token0: Address::repeat_byte(0xaa),
                token1: Address::repeat_byte(0xbb),
            });
            addr_byte = addr_byte.wrapping_add(1);
        }
        for _ in 0..v3_count {
            rows.push(PoolUniverseRow {
                protocol: PoolProtocol::Agni,
                factory,
                pool: Address::repeat_byte(addr_byte),
                token0: Address::repeat_byte(0xaa),
                token1: Address::repeat_byte(0xbb),
            });
            addr_byte = addr_byte.wrapping_add(1);
        }
        for _ in 0..moe_count {
            rows.push(PoolUniverseRow {
                protocol: PoolProtocol::MoeLb,
                factory,
                pool: Address::repeat_byte(addr_byte),
                token0: Address::repeat_byte(0xaa),
                token1: Address::repeat_byte(0xbb),
            });
            addr_byte = addr_byte.wrapping_add(1);
        }
        let fingerprint = B256::repeat_byte(0x42);
        let addresses = rows.iter().map(|r| r.pool).collect();
        LoadedPoolUniverse {
            rows,
            fingerprint,
            addresses,
            snapshot_block: Some(100_000_000),
        }
    }

    #[test]
    fn v3_moe_only_universe_fails_closed_with_diagnostic() {
        let profile = load_mainnet_profile();
        let universe = make_test_universe(0, 76, 33);
        let err = assert_universe_gas_profile_compatibility(&universe, &profile, 3)
            .expect_err("v3+moe only universe must fail closed");
        let err_msg = err.to_string();

        let UniverseGasProfileError::EmptyApprovedRouteIntersection(diag) = err else {
            panic!("expected EmptyApprovedRouteIntersection error");
        };

        // WHI-1421: classified per topology over every crossing bucket. WHI-1422:
        // every v3/moe topology now has an explicit entry at every bucket, so none is
        // unknown; none is approved (fix round 1 withheld the new V3-hop approvals,
        // review PR108-F2 / DI-50, and the V3/Moe-state lever classes, PR108-F3).
        assert_eq!(diag.topologies_total, 12);
        assert!(diag.topologies_supported.is_empty());
        assert_eq!(
            diag.topologies_unapproved,
            [
                "h2:v3+v3",
                "h2:v3+moe",
                "h2:moe+v3",
                "h2:moe+moe",
                "h3:v3+v3+v3",
                "h3:v3+v3+moe",
                "h3:v3+moe+v3",
                "h3:v3+moe+moe",
                "h3:moe+v3+v3",
                "h3:moe+v3+moe",
                "h3:moe+moe+v3",
                "h3:moe+moe+moe"
            ],
            "{err_msg}"
        );
        assert!(diag.topologies_unknown.is_empty(), "{err_msg}");
        assert_eq!(diag.pool_universe_fingerprint, universe.fingerprint);
        assert_eq!(diag.gas_profile_identity, profile.artifact_digest());
        assert_eq!(diag.approved_routes_in_profile.len(), 5);

        assert!(err_msg.contains("Gas profile universe intersection is empty"));
        assert!(err_msg.contains(&universe.fingerprint.to_string()));
        assert!(err_msg.contains("agni-v2=0, agni-v3=76, moe=33"));
        assert!(err_msg.contains(profile.artifact_digest()));
        for approved in &diag.approved_routes_in_profile {
            assert!(err_msg.contains(approved));
        }
        for topology in diag
            .topologies_unapproved
            .iter()
            .chain(&diag.topologies_unknown)
        {
            assert!(err_msg.contains(&format!("- {topology}\n")), "{topology}");
        }
        // WHI-1421 AC: the WHI-1408 diagnostic states the gate's limits.
        assert!(
            err_msg.contains("necessary, not sufficient"),
            "diagnostic must say the gate is necessary, not sufficient: {err_msg}"
        );
        assert!(err_msg.contains("count-based"), "{err_msg}");
    }

    #[test]
    fn universe_with_approved_topology_succeeds() {
        let profile = load_mainnet_profile();
        let universe = make_test_universe(5, 87, 38);
        assert_universe_gas_profile_compatibility(&universe, &profile, 3)
            .expect("universe with v2 pools must succeed");
    }

    /// WHI-1408 round-2: a non-empty pool set that cannot generate a single hop-2+
    /// topology (here: exactly one V3 pool, so no [v3,v3] pair can form) must still
    /// fail closed — this is just as "nothing can ever be priced" as an explicit zero
    /// approved-topologies census, and must not be conflated with the genuinely-empty
    /// (zero pools of any protocol) universe case that Ok(())s below.
    #[test]
    fn single_pool_universe_that_cannot_form_any_topology_fails_closed() {
        let profile = load_mainnet_profile();
        let lone_pool_universe = make_test_universe(0, 1, 0);
        let err = assert_universe_gas_profile_compatibility(&lone_pool_universe, &profile, 3)
            .expect_err(
                "a lone pool that can form no topology must fail closed, not pass silently",
            );
        let UniverseGasProfileError::EmptyApprovedRouteIntersection(diag) = err else {
            panic!("expected EmptyApprovedRouteIntersection error");
        };
        assert_eq!(diag.topologies_total, 0);
        assert!(diag.topologies_supported.is_empty());
    }

    /// A truly unloaded universe (zero pools of every protocol) is a distinct failure
    /// mode with its own dedicated fail-closed check elsewhere (e.g. bot.rs's
    /// `amms.is_empty()` bail) — this helper must not also report it as a gas-profile
    /// mismatch.
    #[test]
    fn genuinely_empty_universe_is_not_reported_as_a_gas_profile_mismatch() {
        let profile = load_mainnet_profile();
        let empty_universe = make_test_universe(0, 0, 0);
        assert_universe_gas_profile_compatibility(&empty_universe, &profile, 3)
            .expect("a genuinely empty universe defers to the dedicated zero-pools check");
    }

    #[test]
    fn pools_compatibility_checks_succeed_and_fail() {
        let profile = load_mainnet_profile();
        let all_pools = crate::service::fixture::cross_protocol_fixture_pools();
        // All pools (v2+v3+moe) must pass
        assert_pools_gas_profile_compatibility(B256::ZERO, &all_pools, &profile, 3)
            .expect("all pools must succeed");

        // V3+moe subset must fail (WHI-1422 fix round 1: no V3/Moe-only class is
        // approved; PR108-F2/F3)
        let v3_moe_pools = crate::service::select::filter_pools_by_protocols(
            &all_pools,
            &[
                crate::service::select::SelectedProtocol::AgniV3,
                crate::service::select::SelectedProtocol::Moe,
            ],
        );
        let err = assert_pools_gas_profile_compatibility(B256::ZERO, &v3_moe_pools, &profile, 3)
            .expect_err("v3+moe only pools must fail closed");
        assert!(err
            .to_string()
            .contains("supported (active approval at some bucket) (0):"));
    }

    /// Moved from `path_index` (WHI-1409) with [`route_key_candidates`] (WHI-1421).
    #[test]
    fn route_key_candidates_enumerate_every_bucket_combination() {
        // Pure V2: no bucket axis at all — exactly one key.
        let v2v2 = route_key_candidates(&[ProtocolKind::V2, ProtocolKind::V2]).unwrap();
        assert_eq!(
            v2v2,
            vec![RouteKey::new(vec![ProtocolKind::V2, ProtocolKind::V2]).unwrap()]
        );

        // V3 only: varies over the 4 tick buckets.
        let v3v3 = route_key_candidates(&[ProtocolKind::V3, ProtocolKind::V3]).unwrap();
        assert_eq!(v3v3.len(), 4);
        assert!(v3v3
            .iter()
            .all(|k| k.v3_tick_crossings.is_some() && k.moe_bin_crossings.is_none()));
        for k in &v3v3 {
            k.validate_structure()
                .expect("every candidate must be structurally valid");
        }

        // Moe only: varies over the 4 bin buckets.
        let moe_moe = route_key_candidates(&[ProtocolKind::Moe, ProtocolKind::Moe]).unwrap();
        assert_eq!(moe_moe.len(), 4);
        assert!(moe_moe
            .iter()
            .all(|k| k.moe_bin_crossings.is_some() && k.v3_tick_crossings.is_none()));

        // Mixed V3+Moe: full 4x4 cross product.
        let mixed =
            route_key_candidates(&[ProtocolKind::V3, ProtocolKind::Moe, ProtocolKind::V3]).unwrap();
        assert_eq!(mixed.len(), 16);
        assert!(mixed
            .iter()
            .all(|k| k.v3_tick_crossings.is_some() && k.moe_bin_crossings.is_some()));
        for k in &mixed {
            k.validate_structure()
                .expect("every mixed candidate must be structurally valid");
        }
        // No duplicate keys.
        let mut dedup = mixed.clone();
        dedup.sort();
        dedup.dedup();
        assert_eq!(dedup.len(), mixed.len());
    }

    /// WHI-1421 AC: a poisoned invalidation lock fails closed in the shared
    /// predicate. The lock is private to `src/execution` (and cannot be poisoned
    /// through any public method), so this injects exactly what the two public
    /// reads return on a poisoned lock: `inspect_route` fails open (DI-45) and
    /// still reports the raw `Approved` entry, while `quote` returns
    /// `ProfileStatePoisoned`. The predicate must never answer `Supported`.
    #[test]
    fn poisoned_invalidation_lock_fails_closed_in_the_shared_predicate() {
        let profile = load_mainnet_profile();
        let v2_v3 = [ProtocolKind::V2, ProtocolKind::V3];
        assert_eq!(
            topology_profile_support(&profile, &v2_v3).unwrap(),
            ProfileSupport::Supported,
            "healthy baseline: [v2,v3] is approved at ticks=0"
        );

        let err = profile_support_with(
            &v2_v3,
            |key| profile.inspect_route(key),
            |_| Err(RuntimeGasProfileError::ProfileStatePoisoned),
        )
        .expect_err("a poisoned lock must fail closed, not report Supported");
        assert!(matches!(
            err,
            ProfileSupportError::Profile(RuntimeGasProfileError::ProfileStatePoisoned)
        ));
        // Both call sites treat the error as "not supported": the startup gate
        // propagates it, the pre-simulation filter rejects the path.
        assert!(UniverseGasProfileError::from(err)
            .to_string()
            .contains("failed closed"));

        // Invalidated between the two reads: an entry, never an approval.
        assert_eq!(
            profile_support_with(
                &v2_v3,
                |key| profile.inspect_route(key),
                |key| Err(RuntimeGasProfileError::UnapprovedRoute(key.key_string())),
            )
            .unwrap(),
            ProfileSupport::Unapproved
        );
    }
}
