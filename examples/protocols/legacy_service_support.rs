use alloy::network::primitives::{BlockResponse, HeaderResponse};
use alloy::network::Network;
use alloy::primitives::{Address, B256, I256, U256};
use alloy::providers::Provider;
use alloy::rpc::types::{Filter, Log};
use alloy::transports::{RpcError, TransportErrorKind};
use amms::amms::amm::{AutomatedMarketMaker, Variant, AMM};
use amms::arbitrage::gas::{
    net_profit_after_gas_cost, required_gross_for_gas_margin, DEFAULT_GAS_SAFETY_MARGIN,
};
use amms::arbitrage::pathfinder::ArbitragePath;
use amms::execution::{
    verify_pool_provenance, OnChainProvenanceSource, PoolProvenance, ProtocolKind, RouteKey,
    TickCrossingBucket,
};
use amms::state_space::{pool_universe_fingerprint, PoolProtocol, PoolUniverseRow};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::hash::Hash;
use std::io::BufReader;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::time::{sleep, Duration};

pub use amms::execution::plan_resized_execution_default_margin;

pub const TRANSIENT_FAILURE_TTL_SECS: u64 = 60;
const MAX_BLOCK_LOG_ATTEMPTS: usize = 20;

pub async fn verify_executable_pool_provenance<'a, P>(
    provider: &P,
    executor: Address,
    factory: Address,
    protocol: PoolProtocol,
    pools: impl IntoIterator<Item = &'a AMM>,
    block_hash: B256,
) -> eyre::Result<()>
where
    P: Provider + Clone,
{
    let source = OnChainProvenanceSource::new(provider.clone(), executor, block_hash);
    for pool in pools {
        let tokens = pool.tokens();
        if tokens.len() != 2 {
            return Err(eyre::eyre!(
                "pool {} does not expose exactly two venue-ordered tokens",
                pool.address()
            ));
        }
        let fee_or_bin_step = match (protocol, pool) {
            (PoolProtocol::UniswapV2, AMM::UniswapV2Pool(_)) => 0,
            (PoolProtocol::UniswapV3, AMM::UniswapV3Pool(pool)) => pool.fee,
            (PoolProtocol::Agni, AMM::AgniPool(pool)) => pool.fee,
            (PoolProtocol::MoeLb, AMM::MoeLbPair(pool)) => u32::from(pool.bin_step),
            _ => {
                return Err(eyre::eyre!(
                    "pool {} variant does not match configured protocol {:?}",
                    pool.address(),
                    protocol
                ))
            }
        };
        let expected = PoolProvenance {
            protocol,
            factory,
            pool: pool.address(),
            token0: tokens[0],
            token1: tokens[1],
            fee_or_bin_step,
        };
        verify_pool_provenance(&source, &expected)
            .await
            .map_err(|error| {
                eyre::eyre!(
                    "pool {} failed startup provenance verification: {error}",
                    pool.address()
                )
            })?;
    }
    Ok(())
}

/// Agni/V3 final re-simulation with measured tick-crossing RouteKey evidence.
pub fn agni_path_steps_with_route_key(
    path: &ArbitragePath,
    pools: &[AMM],
    amount_in: U256,
) -> eyre::Result<(Vec<U256>, I256, RouteKey)> {
    let mut current = amount_in;
    let mut outputs = Vec::with_capacity(path.hops.len());
    let mut crossings = 0u32;
    for (hop, amm) in path.hops.iter().zip(pools.iter()) {
        let AMM::AgniPool(pool) = amm else {
            return Err(eyre::eyre!("Agni V3 route contains a non-Agni pool"));
        };
        let evidence = pool.simulate_swap_with_crossing_evidence(hop.token_in, current)?;
        crossings = crossings.saturating_add(evidence.crossing_count);
        current = evidence.amount_out;
        outputs.push(current);
    }
    let route_key = RouteKey::new(vec![ProtocolKind::V3; path.hops.len()])?
        .with_v3_ticks(TickCrossingBucket::from_crossings(crossings));
    Ok((
        outputs,
        I256::from_raw(current) - I256::from_raw(amount_in),
        route_key,
    ))
}

/// Resolve the canonical hash for a block number (startup pin for provenance).
pub async fn canonical_block_hash_at_number<N, P>(
    provider: &P,
    block_number: u64,
) -> eyre::Result<B256>
where
    N: Network,
    P: Provider<N>,
{
    let block = provider
        .get_block_by_number(block_number.into())
        .await?
        .ok_or_else(|| eyre::eyre!("missing block {block_number}"))?;
    Ok(block.header().hash())
}

pub fn executable_pool_universe_fingerprint<'a>(
    chain_id: u64,
    settlement_asset: Address,
    factory: Address,
    protocol: PoolProtocol,
    pools: impl IntoIterator<Item = &'a AMM>,
) -> eyre::Result<B256> {
    let rows = pools
        .into_iter()
        .map(|pool| {
            let tokens = pool.tokens();
            if tokens.len() != 2 {
                return Err(eyre::eyre!(
                    "pool {} does not expose exactly two venue-ordered tokens",
                    pool.address()
                ));
            }
            Ok(PoolUniverseRow {
                protocol,
                factory,
                pool: pool.address(),
                token0: tokens[0],
                token1: tokens[1],
            })
        })
        .collect::<eyre::Result<Vec<_>>>()?;
    pool_universe_fingerprint(chain_id, settlement_asset, rows).map_err(Into::into)
}

pub async fn canonical_block_header<N, P>(
    provider: &P,
    block_number: u64,
    delivered_hash: B256,
) -> eyre::Result<N::BlockResponse>
where
    N: Network,
    P: Provider<N>,
{
    let target_header = provider
        .get_block_by_number(block_number.into())
        .await?
        .ok_or_else(|| eyre::eyre!("missing block {block_number}"))?;
    let canonical_hash = target_header.header().hash();
    if canonical_hash != delivered_hash {
        return Err(eyre::eyre!(
            "WS block {block_number} is not canonical: expected {canonical_hash}, got {delivered_hash}"
        ));
    }
    Ok(target_header)
}

pub async fn wait_for_block_logs<N, P>(
    provider: &P,
    filter: &Filter,
    block_number: u64,
    block_hash: B256,
) -> Result<Vec<Log>, RpcError<TransportErrorKind>>
where
    N: Network,
    P: Provider<N>,
{
    let mut attempts = 0;
    loop {
        let error = match provider.get_logs(filter).await {
            Ok(logs)
                if !logs.is_empty()
                    && logs.iter().all(|log| {
                        log.block_number == Some(block_number) && log.block_hash == Some(block_hash)
                    }) =>
            {
                return Ok(logs);
            }
            Ok(logs) if logs.is_empty() => {
                match provider.get_block_receipts(block_hash.into()).await {
                    Ok(Some(_)) => return Ok(logs),
                    Ok(None) => TransportErrorKind::custom_str(
                        "current block receipts are not queryable yet",
                    ),
                    Err(error) => error,
                }
            }
            Ok(_) => TransportErrorKind::custom_str(
                "current block logs returned mixed or incomplete block metadata",
            ),
            Err(error) => error,
        };

        attempts += 1;
        if attempts >= MAX_BLOCK_LOG_ATTEMPTS {
            return Err(error);
        }
        tracing::warn!(
            target: "legacy_service.block",
            block = block_number,
            attempt = attempts,
            max_attempts = MAX_BLOCK_LOG_ATTEMPTS,
            error = ?error,
            "Current block logs are not queryable yet; retrying"
        );
        sleep(Duration::from_millis(250)).await;
    }
}

pub fn is_on_cooldown(
    last_execution_block: Option<u64>,
    current_block: u64,
    block_cooldown: u64,
) -> bool {
    last_execution_block
        .map(|last_block| current_block.saturating_sub(last_block) < block_cooldown)
        .unwrap_or(false)
}

pub fn route_is_structurally_valid(
    wmnt: Address,
    token_path: &[Address],
    pool_addresses: &[Address],
    expected_variant: Variant,
    pools: &[AMM],
) -> bool {
    if pool_addresses.is_empty()
        || token_path.len() != pool_addresses.len() + 1
        || pools.len() != pool_addresses.len()
        || token_path.first().copied() != Some(wmnt)
        || token_path.last().copied() != Some(wmnt)
    {
        return false;
    }

    pool_addresses
        .iter()
        .zip(pools)
        .enumerate()
        .all(|(index, (address, pool))| {
            if pool.address() != *address || pool.variant() != expected_variant {
                return false;
            }
            let tokens = pool.tokens();
            tokens.len() == 2
                && ((token_path[index] == tokens[0] && token_path[index + 1] == tokens[1])
                    || (token_path[index] == tokens[1] && token_path[index + 1] == tokens[0]))
        })
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum PersistedFailureStore<T> {
    Split {
        permanent: Vec<T>,
        transient: Vec<PersistedTransientFailure<T>>,
    },
    Legacy(Vec<T>),
}

#[derive(Debug, Deserialize)]
struct PersistedTransientFailure<T> {
    signature: T,
    expires_at: u64,
}

#[derive(Debug, Serialize)]
struct PersistedFailureStoreFile<'a, T> {
    permanent: &'a HashSet<T>,
    transient: Vec<PersistedTransientFailureRef<'a, T>>,
}

#[derive(Debug, Serialize)]
struct PersistedTransientFailureRef<'a, T> {
    signature: &'a T,
    expires_at: u64,
}

pub struct FailureStore<T> {
    path: PathBuf,
    permanent: HashSet<T>,
    transient: HashMap<T, u64>,
    transient_ttl_secs: u64,
}

impl<T> FailureStore<T>
where
    T: Clone + Eq + Hash + Serialize + DeserializeOwned,
{
    pub fn new(path: &str) -> eyre::Result<Self> {
        Self::with_ttl(path, TRANSIENT_FAILURE_TTL_SECS)
    }

    pub fn with_ttl(path: &str, transient_ttl_secs: u64) -> eyre::Result<Self> {
        let path = PathBuf::from(path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut store = Self {
            path,
            permanent: HashSet::new(),
            transient: HashMap::new(),
            transient_ttl_secs,
        };
        store.load()?;
        Ok(store)
    }

    fn load(&mut self) -> eyre::Result<()> {
        if !self.path.exists() {
            return Ok(());
        }

        let file = File::open(&self.path)?;
        let reader = BufReader::new(file);
        match serde_json::from_reader::<_, PersistedFailureStore<T>>(reader)? {
            PersistedFailureStore::Split {
                permanent,
                transient,
            } => {
                self.permanent = permanent.into_iter().collect();
                self.transient = transient
                    .into_iter()
                    .map(|failure| (failure.signature, failure.expires_at))
                    .collect();
            }
            PersistedFailureStore::Legacy(signatures) => {
                let expires_at = unix_timestamp().saturating_add(self.transient_ttl_secs);
                self.transient = signatures
                    .into_iter()
                    .map(|signature| (signature, expires_at))
                    .collect();
            }
        }
        Ok(())
    }

    pub fn is_failed(&self, signature: &T) -> bool {
        self.is_failed_at(signature, unix_timestamp())
    }

    pub fn is_failed_at(&self, signature: &T, now: u64) -> bool {
        self.permanent.contains(signature)
            || self
                .transient
                .get(signature)
                .is_some_and(|expires_at| *expires_at > now)
    }

    pub fn mark_permanent(&mut self, signature: T) -> eyre::Result<()> {
        self.transient.remove(&signature);
        self.permanent.insert(signature);
        self.persist(unix_timestamp())
    }

    pub fn mark_transient(&mut self, signature: T) -> eyre::Result<()> {
        self.mark_transient_at(signature, unix_timestamp())
    }

    pub fn mark_transient_at(&mut self, signature: T, now: u64) -> eyre::Result<()> {
        if self.permanent.contains(&signature) {
            return Ok(());
        }
        self.transient
            .insert(signature, now.saturating_add(self.transient_ttl_secs));
        self.persist(now)
    }

    fn persist(&mut self, now: u64) -> eyre::Result<()> {
        self.transient.retain(|_, expires_at| *expires_at > now);
        let transient = self
            .transient
            .iter()
            .map(|(signature, expires_at)| PersistedTransientFailureRef {
                signature,
                expires_at: *expires_at,
            })
            .collect();
        let file = File::create(&self.path)?;
        serde_json::to_writer_pretty(
            file,
            &PersistedFailureStoreFile {
                permanent: &self.permanent,
                transient,
            },
        )?;
        Ok(())
    }
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Debug, Clone, Copy)]
pub struct GasConfig {
    pub gas_price_wei: u128,
}

impl Default for GasConfig {
    fn default() -> Self {
        Self {
            gas_price_wei: 25_000_000,
        }
    }
}

impl GasConfig {
    pub fn calculate_gas_cost(&self, hops: usize) -> U256 {
        U256::from(gas_limit_for_hops(hops)) * U256::from(self.gas_price_wei)
    }

    pub fn net_profit(&self, gross_profit: U256, hops: usize) -> Option<U256> {
        net_profit_after_gas_cost(gross_profit, self.calculate_gas_cost(hops))
    }

    pub fn is_profitable_after_gas(
        &self,
        gross_profit: U256,
        hops: usize,
        safety_margin: f64,
    ) -> bool {
        gross_profit >= required_gross_for_gas_margin(self.calculate_gas_cost(hops), safety_margin)
    }
}

pub const fn gas_limit_for_hops(hops: usize) -> u64 {
    match hops {
        0 | 1 => 300_000_000,
        2 => 900_000_000,
        3 => 1_500_000_000,
        4 => 2_800_000_000,
        _ => 2_800_000_000,
    }
}

pub fn max_fee_per_gas_with_headroom(
    base_fee_per_gas: u64,
    priority_fee_per_gas: u128,
) -> Option<u128> {
    u128::from(base_fee_per_gas)
        .checked_mul(2)
        .and_then(|base_fee| base_fee.checked_add(priority_fee_per_gas))
}

pub const fn default_gas_safety_margin() -> f64 {
    DEFAULT_GAS_SAFETY_MARGIN
}

#[cfg(test)]
mod tests {
    use super::{max_fee_per_gas_with_headroom, wait_for_block_logs, FailureStore};
    use alloy::network::Ethereum;
    use alloy::primitives::B256;
    use alloy::providers::ProviderBuilder;
    use alloy::rpc::types::{Filter, Log};
    use alloy::transports::mock::Asserter;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[tokio::test]
    async fn waits_for_receipts_before_accepting_empty_logs() {
        let asserter = Asserter::new();
        asserter.push_success(&Vec::<Log>::new());
        asserter.push_success(&Option::<Vec<serde_json::Value>>::None);
        asserter.push_success(&Vec::<Log>::new());
        asserter.push_success(&Vec::<serde_json::Value>::new());
        let provider = ProviderBuilder::new().connect_mocked_client(asserter.clone());

        let logs = wait_for_block_logs::<Ethereum, _>(
            &provider,
            &Filter::new(),
            7,
            B256::repeat_byte(0x42),
        )
        .await
        .unwrap();

        assert!(logs.is_empty());
        assert!(
            asserter.read_q().is_empty(),
            "empty logs must not be accepted before receipt readiness is confirmed"
        );
    }

    #[test]
    fn includes_base_fee_headroom_before_priority_fee() {
        assert_eq!(max_fee_per_gas_with_headroom(100, 3), Some(203));
    }

    #[test]
    fn rejects_base_fee_headroom_overflow() {
        assert_eq!(max_fee_per_gas_with_headroom(u64::MAX, u128::MAX), None);
    }

    #[test]
    fn transient_failures_expire_but_permanent_failures_do_not() {
        let path = std::env::temp_dir().join(format!(
            "amms-failure-store-{}-{}.json",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path_string = path.to_string_lossy().into_owned();
        let mut store = FailureStore::with_ttl(&path_string, 10).unwrap();

        store
            .mark_transient_at("transient".to_string(), 100)
            .unwrap();
        assert!(store.is_failed_at(&"transient".to_string(), 109));
        assert!(!store.is_failed_at(&"transient".to_string(), 110));

        store.mark_permanent("permanent".to_string()).unwrap();
        assert!(store.is_failed_at(&"permanent".to_string(), u64::MAX));

        let _ = std::fs::remove_file(path);
    }
}
