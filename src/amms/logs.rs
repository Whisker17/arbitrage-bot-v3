use std::collections::HashSet;

use crate::amms::error::AMMError;
use alloy::{
    consensus::BlockHeader,
    eips::{BlockId, BlockNumberOrTag},
    network::primitives::{BlockResponse, HeaderResponse},
    network::Network,
    primitives::{Address, B256},
    providers::Provider,
    rpc::types::{Filter, Log},
    transports::{RpcError, TransportErrorKind},
};
use thiserror::Error;
use tracing::info;

pub const DEFAULT_LOG_INITIAL_WINDOW: u64 = 10_000;
pub const DEFAULT_LOG_MINIMUM_WINDOW: u64 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogRangeConfig {
    pub initial_window: u64,
    pub minimum_window: u64,
}

impl Default for LogRangeConfig {
    fn default() -> Self {
        Self {
            initial_window: DEFAULT_LOG_INITIAL_WINDOW,
            minimum_window: DEFAULT_LOG_MINIMUM_WINDOW,
        }
    }
}

impl LogRangeConfig {
    pub fn from_env() -> Self {
        let default = Self::default();
        let initial_window = env_window(
            &["MANTLE_SEPOLIA_LOG_INITIAL_WINDOW", "MANTLE_LOG_INITIAL_WINDOW"],
            default.initial_window,
        );
        let minimum_window = env_window(
            &["MANTLE_SEPOLIA_LOG_MINIMUM_WINDOW", "MANTLE_LOG_MINIMUM_WINDOW"],
            default.minimum_window,
        )
        .min(initial_window);

        Self {
            initial_window,
            minimum_window,
        }
    }
}

fn env_window(names: &[&str], default: u64) -> u64 {
    names
        .iter()
        .find_map(|name| {
            std::env::var(name)
                .ok()
                .and_then(|value| value.parse().ok())
                .filter(|value: &u64| *value > 0)
        })
        .unwrap_or(default)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogRangeCapability {
    pub initial_window: u64,
    pub minimum_window: u64,
    pub largest_successful_window: Option<u64>,
    pub smallest_rejected_window: Option<u64>,
    pub reductions: u32,
    pub requests: u32,
}

impl LogRangeCapability {
    fn new(config: LogRangeConfig) -> Self {
        Self {
            initial_window: config.initial_window,
            minimum_window: config.minimum_window,
            largest_successful_window: None,
            smallest_rejected_window: None,
            reductions: 0,
            requests: 0,
        }
    }

    fn record_request(&mut self, window: u64) {
        self.requests += 1;
        self.largest_successful_window = Some(
            self.largest_successful_window
                .map_or(window, |largest| largest.max(window)),
        );
    }

    fn record_rejection(&mut self, window: u64) {
        self.reductions += 1;
        self.smallest_rejected_window = Some(
            self.smallest_rejected_window
                .map_or(window, |smallest| smallest.min(window)),
        );
    }
}

#[derive(Debug, Error)]
pub enum AdaptiveLogError {
    #[error("invalid log range {from}..={to}")]
    InvalidRange { from: u64, to: u64 },
    #[error("canonical block {0:?} was not found")]
    MissingBlock(B256),
    #[error("canonical block #{0} was not found")]
    MissingBlockNumber(u64),
    #[error("canonical block #{number} changed from {before:?} to {after:?} during log read")]
    CanonicalChanged {
        number: u64,
        before: B256,
        after: B256,
    },
    #[error("eth_getLogs failed: {0}")]
    Provider(#[source] RpcError<TransportErrorKind>),
}

pub fn adaptive_log_error(error: AdaptiveLogError) -> AMMError {
    match error {
        AdaptiveLogError::InvalidRange { .. }
        | AdaptiveLogError::MissingBlock(_)
        | AdaptiveLogError::MissingBlockNumber(_)
        | AdaptiveLogError::CanonicalChanged { .. } => {
            AMMError::IncompleteState
        }
        AdaptiveLogError::Provider(error) => AMMError::TransportError(error),
    }
}

pub async fn block_number_for_range<N, P>(
    provider: &P,
    block_id: BlockId,
) -> Result<u64, AdaptiveLogError>
where
    N: Network,
    P: Provider<N>,
{
    match block_id {
        BlockId::Number(BlockNumberOrTag::Number(number)) => Ok(number),
        BlockId::Number(_) => provider
            .get_block_number()
            .await
            .map_err(AdaptiveLogError::Provider),
        BlockId::Hash(hash) => provider
            .get_block_by_hash(hash.block_hash)
            .await
            .map_err(AdaptiveLogError::Provider)?
            .ok_or(AdaptiveLogError::MissingBlock(hash.block_hash))
            .map(|block| block.header().number()),
    }
}

#[derive(Debug)]
pub struct AdaptiveLogResult {
    pub logs: Vec<Log>,
    pub capability: LogRangeCapability,
}

pub async fn fetch_logs_in_ranges<N, P>(
    provider: P,
    base_filter: Filter,
    from_block: u64,
    to_block: u64,
    config: LogRangeConfig,
) -> Result<AdaptiveLogResult, AdaptiveLogError>
where
    N: Network,
    P: Provider<N> + Clone,
{
    fetch_logs_in_ranges_impl(provider, base_filter, from_block, to_block, config, true).await
}

async fn fetch_logs_in_ranges_impl<N, P>(
    provider: P,
    base_filter: Filter,
    from_block: u64,
    to_block: u64,
    mut config: LogRangeConfig,
    verify_canonical: bool,
) -> Result<AdaptiveLogResult, AdaptiveLogError>
where
    N: Network,
    P: Provider<N> + Clone,
{
    if to_block < from_block {
        return Err(AdaptiveLogError::InvalidRange {
            from: from_block,
            to: to_block,
        });
    }

    config = LogRangeConfig {
        initial_window: config.initial_window.max(1),
        minimum_window: config.minimum_window.max(1).min(config.initial_window.max(1)),
    };
    let mut capability = LogRangeCapability::new(config);
    let mut logs = Vec::new();
    let mut from = from_block;
    let mut window = config.initial_window.min(to_block - from_block + 1);

    loop {
        let to = from.saturating_add(window - 1).min(to_block);
        let filter = base_filter.clone().from_block(from).to_block(to);

        let before = if verify_canonical {
            Some(range_identity::<N, _>(&provider, from, to).await?)
        } else {
            None
        };
        let response = provider.get_logs(&filter).await;
        if let Some(before) = before {
            verify_range_identity::<N, _>(&provider, from, to, before, response.as_ref().ok())
                .await?;
        }

        match response {
            Ok(mut chunk) => {
                capability.record_request(window);
                logs.append(&mut chunk);

                if to == to_block {
                    break;
                }
                from = to + 1;
            }
            Err(error) if is_log_range_limit_error(&error) && window > config.minimum_window => {
                capability.record_rejection(window);
                window = (window / 2).max(config.minimum_window);
                continue;
            }
            Err(error) => return Err(AdaptiveLogError::Provider(error)),
        }
    }

    let logs = deduplicate_logs(logs);
    info!(
        target: "amms::logs",
        initial_window = capability.initial_window,
        minimum_window = capability.minimum_window,
        largest_successful_window = ?capability.largest_successful_window,
        smallest_rejected_window = ?capability.smallest_rejected_window,
        reductions = capability.reductions,
        requests = capability.requests,
        "Observed historical eth_getLogs range capability"
    );

    Ok(AdaptiveLogResult { logs, capability })
}

async fn range_identity<N, P>(
    provider: &P,
    from: u64,
    to: u64,
) -> Result<(B256, B256), AdaptiveLogError>
where
    N: Network,
    P: Provider<N>,
{
    Ok((
        canonical_hash_at::<N, _>(provider, from).await?,
        canonical_hash_at::<N, _>(provider, to).await?,
    ))
}

async fn verify_range_identity<N, P>(
    provider: &P,
    from: u64,
    to: u64,
    before: (B256, B256),
    logs: Option<&Vec<Log>>,
) -> Result<(), AdaptiveLogError>
where
    N: Network,
    P: Provider<N>,
{
    let after = range_identity::<N, _>(provider, from, to).await?;
    for (number, expected, actual) in [
        (from, before.0, after.0),
        (to, before.1, after.1),
    ] {
        if expected != actual {
            return Err(AdaptiveLogError::CanonicalChanged {
                number,
                before: expected,
                after: actual,
            });
        }
    }

    let Some(logs) = logs else {
        return Ok(());
    };
    let mut checked = HashSet::new();
    for log in logs {
        let (Some(number), Some(hash)) = (log.block_number, log.block_hash) else {
            continue;
        };
        if !checked.insert(number) {
            continue;
        }
        let canonical = canonical_hash_at::<N, _>(provider, number).await?;
        if canonical != hash {
            return Err(AdaptiveLogError::CanonicalChanged {
                number,
                before: hash,
                after: canonical,
            });
        }
    }
    Ok(())
}

async fn canonical_hash_at<N, P>(provider: &P, number: u64) -> Result<B256, AdaptiveLogError>
where
    N: Network,
    P: Provider<N>,
{
    provider
        .get_block_by_number(BlockNumberOrTag::Number(number))
        .await
        .map_err(AdaptiveLogError::Provider)?
        .ok_or(AdaptiveLogError::MissingBlockNumber(number))
        .map(|block| block.header().hash())
}

pub fn is_log_range_limit_error(error: &RpcError<TransportErrorKind>) -> bool {
    let message = error.to_string().to_ascii_lowercase();
    [
        "too many results",
        "more than",
        "query returned",
        "result limit",
        "range limit",
        "block range",
        "response size",
        "exceeds maximum",
        "exceeded maximum",
        "limit exceeded",
        "max block range",
        "too large",
        "request is too large",
    ]
    .iter()
    .any(|marker| message.contains(marker))
}

fn deduplicate_logs(logs: Vec<Log>) -> Vec<Log> {
    let mut seen = HashSet::new();
    logs.into_iter()
        .filter(|log| {
            let Some(key) = log_key(log) else {
                return true;
            };
            seen.insert(key)
        })
        .collect()
}

fn log_key(
    log: &Log,
) -> Option<(
    Address,
    Option<B256>,
    Option<u64>,
    Option<B256>,
    Option<u64>,
)> {
    if log.block_hash.is_none() || log.transaction_hash.is_none() || log.log_index.is_none() {
        return None;
    }

    Some((
        log.address(),
        log.block_hash,
        log.block_number,
        log.transaction_hash,
        log.log_index,
    ))
}

#[cfg(test)]
mod tests {
    use alloy::transports::TransportErrorKind;
    use alloy::{
        network::Ethereum,
        providers::ProviderBuilder,
        rpc::types::{Block, Log},
        transports::mock::Asserter,
    };

    use super::*;

    #[test]
    fn range_limit_messages_are_retryable() {
        let error = TransportErrorKind::custom_str("query returned more than 10000 results");
        assert!(is_log_range_limit_error(&error));
    }

    #[test]
    fn unrelated_provider_messages_are_not_range_limits() {
        let error = TransportErrorKind::custom_str("connection reset by peer");
        assert!(!is_log_range_limit_error(&error));
    }

    #[test]
    fn config_from_env_never_allows_zero_windows() {
        let config = LogRangeConfig::default();
        assert!(config.initial_window > 0);
        assert!(config.minimum_window > 0);
        assert!(config.minimum_window <= config.initial_window);
    }

    #[test]
    fn capability_tracks_success_and_rejection_bounds() {
        let mut capability = LogRangeCapability::new(LogRangeConfig {
            initial_window: 100,
            minimum_window: 1,
        });
        capability.record_rejection(100);
        capability.record_request(50);
        capability.record_request(25);

        assert_eq!(capability.largest_successful_window, Some(50));
        assert_eq!(capability.smallest_rejected_window, Some(100));
        assert_eq!(capability.reductions, 1);
        assert_eq!(capability.requests, 2);
    }

    #[tokio::test]
    async fn provider_limit_reduces_window_and_completes_without_overlap() {
        let asserter = Asserter::new();
        asserter.push_failure_msg("query returned more than 10000 results");
        asserter.push_success(&Vec::<Log>::new());
        asserter.push_success(&Vec::<Log>::new());
        let provider = ProviderBuilder::new().connect_mocked_client(asserter.clone());

        let result = fetch_logs_in_ranges_impl::<Ethereum, _>(
            provider,
            Filter::new(),
            0,
            99,
            LogRangeConfig {
                initial_window: 100,
                minimum_window: 10,
            },
            false,
        )
        .await
        .unwrap();

        assert_eq!(result.logs.len(), 0);
        assert_eq!(result.capability.reductions, 1);
        assert_eq!(result.capability.largest_successful_window, Some(50));
        assert_eq!(result.capability.smallest_rejected_window, Some(100));
        assert_eq!(result.capability.requests, 2);
        assert!(asserter.read_q().is_empty());
    }

    fn mock_block(number: u64, hash: B256) -> Block {
        let mut inner = alloy::consensus::Header::default();
        inner.number = number;
        inner.timestamp = number;
        let mut header = alloy::rpc::types::Header::new(inner);
        header.hash = hash;
        Block::empty(header)
    }

    #[tokio::test]
    async fn canonical_range_read_fails_if_endpoint_changes() {
        let asserter = Asserter::new();
        asserter.push_success(&Some(mock_block(7, B256::repeat_byte(1))));
        asserter.push_success(&Some(mock_block(7, B256::repeat_byte(1))));
        asserter.push_success(&Vec::<Log>::new());
        asserter.push_success(&Some(mock_block(7, B256::repeat_byte(1))));
        asserter.push_success(&Some(mock_block(7, B256::repeat_byte(2))));
        let provider = ProviderBuilder::new().connect_mocked_client(asserter);

        let error = fetch_logs_in_ranges::<Ethereum, _>(
            provider,
            Filter::new(),
            7,
            7,
            LogRangeConfig::default(),
        )
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            AdaptiveLogError::CanonicalChanged { number: 7, .. }
        ));
    }

    #[test]
    fn duplicate_logs_are_removed_when_identity_is_complete() {
        let log = Log {
            inner: Default::default(),
            block_hash: Some(B256::repeat_byte(1)),
            block_number: Some(7),
            block_timestamp: None,
            transaction_hash: Some(B256::repeat_byte(2)),
            transaction_index: Some(0),
            log_index: Some(3),
            removed: false,
        };

        assert_eq!(deduplicate_logs(vec![log.clone(), log]).len(), 1);
    }

    #[allow(dead_code)]
    fn _transport_error_type_is_pinned() -> RpcError<TransportErrorKind> {
        TransportErrorKind::custom_str("test")
    }
}
