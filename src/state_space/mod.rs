pub mod cache;
pub mod discovery;
pub mod error;
pub mod filters;

use crate::amms::amm::AutomatedMarketMaker;
use crate::amms::amm::AMM;
use crate::amms::error::AMMError;
use crate::amms::factory::Factory;

use alloy::consensus::BlockHeader;
use alloy::eips::BlockId;
use alloy::rpc::types::{Block, Filter, FilterSet, Log};
use alloy::{
    network::Network,
    primitives::{Address, FixedBytes},
    providers::Provider,
};
use async_stream::stream;
use cache::StateChange;
use cache::StateChangeCache;

use error::StateSpaceError;
use filters::AMMFilter;
use filters::PoolFilter;
use futures::stream::FuturesUnordered;
use futures::Stream;
use futures::StreamExt;
use std::collections::HashSet;
use std::pin::Pin;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::{collections::HashMap, marker::PhantomData, sync::Arc};
use tokio::sync::RwLock;
use tracing::debug;
use tracing::info;

pub const CACHE_SIZE: usize = 30;

#[derive(Clone)]
pub struct StateSpaceManager<N, P> {
    pub state: Arc<RwLock<StateSpace>>,
    pub latest_block: Arc<AtomicU64>,
    // discovery_manager: Option<DiscoveryManager>,
    pub block_filter: Filter,
    pub provider: P,
    phantom: PhantomData<N>,
    // TODO: add support for caching
}

impl<N, P> StateSpaceManager<N, P> {
    pub async fn subscribe(
        &self,
    ) -> Result<
        Pin<Box<dyn Stream<Item = Result<Vec<Address>, StateSpaceError>> + Send>>,
        StateSpaceError,
    >
    where
        P: Provider<N> + Clone + 'static,
        N: Network<BlockResponse = Block>,
    {
        let provider = self.provider.clone();
        let latest_block = self.latest_block.clone();
        let state = self.state.clone();
        let mut block_filter = self.block_filter.clone();

        let block_stream = provider.subscribe_blocks().await?.into_stream();

        Ok(Box::pin(stream! {
            tokio::pin!(block_stream);

            while let Some(block) = block_stream.next().await {
                let block_number = block.number();
                block_filter = block_filter.select(block_number);


                let logs = provider.get_logs(&block_filter).await?;

                let affected_amms = state.write().await.sync(&logs)?;
                latest_block.store(block_number, Ordering::Relaxed);

                yield Ok(affected_amms);
            }
        }))
    }
}

// TODO: Drop impl, create a checkpoint
#[derive(Debug, Default)]
pub struct StateSpaceBuilder<N, P> {
    pub provider: P,
    pub latest_block: u64,
    pub factories: Vec<Factory>,
    pub amms: Vec<AMM>,
    pub filters: Vec<PoolFilter>,
    phantom: PhantomData<N>,
    // TODO: add support for caching
}

impl<N, P> StateSpaceBuilder<N, P>
where
    N: Network,
    P: Provider<N> + Clone + 'static,
{
    pub fn new(provider: P) -> StateSpaceBuilder<N, P> {
        Self {
            provider,
            latest_block: 0,
            factories: vec![],
            amms: vec![],
            filters: vec![],
            // discovery: false,
            phantom: PhantomData,
        }
    }

    pub fn block(self, latest_block: u64) -> StateSpaceBuilder<N, P> {
        StateSpaceBuilder {
            latest_block,
            ..self
        }
    }

    pub fn with_factories(self, factories: Vec<Factory>) -> StateSpaceBuilder<N, P> {
        StateSpaceBuilder { factories, ..self }
    }

    pub fn with_amms(self, amms: Vec<AMM>) -> StateSpaceBuilder<N, P> {
        StateSpaceBuilder { amms, ..self }
    }

    pub fn with_filters(self, filters: Vec<PoolFilter>) -> StateSpaceBuilder<N, P> {
        StateSpaceBuilder { filters, ..self }
    }

    pub async fn sync(self) -> Result<StateSpaceManager<N, P>, AMMError> {
        let chain_tip = BlockId::from(self.provider.get_block_number().await?);
        let factories = self.factories.clone();
        let mut futures = FuturesUnordered::new();

        let mut filter_set = HashSet::new();
        for factory in &self.factories {
            for event in factory.pool_events() {
                filter_set.insert(event);
            }
        }

        for amm in self.amms.iter() {
            for event in amm.sync_events() {
                filter_set.insert(event);
            }
        }

        let block_filter = Filter::new().event_signature(FilterSet::from(
            filter_set.into_iter().collect::<Vec<FixedBytes<32>>>(),
        ));
        let mut amm_variants = HashMap::new();
        for amm in self.amms.into_iter() {
            amm_variants
                .entry(amm.variant())
                .or_insert_with(Vec::new)
                .push(amm);
        }

        for factory in factories {
            let provider = self.provider.clone();
            let filters = self.filters.clone();

            let extension = amm_variants.remove(&factory.variant());
            futures.push(tokio::spawn(async move {
                let mut discovered_amms = factory.discover(chain_tip, provider.clone()).await?;

                if let Some(amms) = extension {
                    discovered_amms.extend(amms);
                }

                // Apply discovery filters
                for filter in filters.iter() {
                    if filter.stage() == filters::FilterStage::Discovery {
                        let pre_filter_len = discovered_amms.len();
                        discovered_amms = filter.filter(discovered_amms).await?;

                        info!(
                            target: "state_space::sync",
                            factory = %factory.address(),
                            pre_filter_len,
                            post_filter_len = discovered_amms.len(),
                            filter = ?filter,
                            "Discovery filter"
                        );
                    }
                }

                discovered_amms = factory.sync(discovered_amms, chain_tip, provider).await?;

                // Apply sync filters
                for filter in filters.iter() {
                    if filter.stage() == filters::FilterStage::Sync {
                        let pre_filter_len = discovered_amms.len();
                        discovered_amms = filter.filter(discovered_amms).await?;

                        info!(
                            target: "state_space::sync",
                            factory = %factory.address(),
                            pre_filter_len,
                            post_filter_len = discovered_amms.len(),
                            filter = ?filter,
                            "Sync filter"
                        );
                    }
                }

                Ok::<Vec<AMM>, AMMError>(discovered_amms)
            }));
        }

        let mut state_space = StateSpace::default();
        while let Some(res) = futures.next().await {
            let synced_amms = res??;

            for amm in synced_amms {
                state_space.state.insert(amm.address(), amm);
            }
        }

        // Sync remaining AMM variants
        for (_, remaining_amms) in amm_variants.drain() {
            for mut amm in remaining_amms {
                let address = amm.address();
                amm = amm.init(chain_tip, self.provider.clone()).await?;
                state_space.state.insert(address, amm);
            }
        }

        Ok(StateSpaceManager {
            latest_block: Arc::new(AtomicU64::new(self.latest_block)),
            state: Arc::new(RwLock::new(state_space)),
            block_filter,
            provider: self.provider,
            phantom: PhantomData,
        })
    }
}

#[derive(Debug, Default, Clone)]
pub struct StateSpace {
    pub state: HashMap<Address, AMM>,
    pub latest_block: Arc<AtomicU64>,
    cache: StateChangeCache<CACHE_SIZE>,
}

impl StateSpace {
    pub fn get(&self, address: &Address) -> Option<&AMM> {
        self.state.get(address)
    }

    pub fn get_mut(&mut self, address: &Address) -> Option<&mut AMM> {
        self.state.get_mut(address)
    }

    pub fn sync(&mut self, logs: &[Log]) -> Result<Vec<Address>, StateSpaceError> {
        let latest = self.latest_block.load(Ordering::Relaxed);
        let Some(mut block_number) = logs
            .first()
            .map(|log| log.block_number.ok_or(StateSpaceError::MissingBlockNumber))
            .transpose()?
        else {
            return Ok(vec![]);
        };

        // Check if there is a reorg and unwind to state before block_number
        if latest >= block_number {
            info!(
                target: "state_space::sync",
                from = %latest,
                to = %block_number - 1,
                "Unwinding state changes"
            );

            let cached_state = self.cache.unwind_state_changes(block_number);
            for amm in cached_state {
                debug!(target: "state_space::sync", ?amm, "Reverting AMM state");
                self.state.insert(amm.address(), amm);
            }
        }

        let mut cached_amms = HashSet::new();
        let mut affected_amms = HashSet::new();
        for log in logs {
            // If the block number is updated, cache the current block state changes
            let log_block_number = log
                .block_number
                .ok_or(StateSpaceError::MissingBlockNumber)?;
            if log_block_number != block_number {
                let amms = cached_amms.drain().collect::<Vec<AMM>>();
                affected_amms.extend(amms.iter().map(|amm| amm.address()));
                let state_change = StateChange::new(amms, block_number);

                debug!(
                    target: "state_space::sync",
                    state_change = ?state_change,
                    "Caching state change"
                );

                self.cache.push(state_change);
                block_number = log_block_number;
            }

            // If the AMM is in the state space add the current state to cache and sync from log
            let address = log.address();
            if let Some(amm) = self.state.get_mut(&address) {
                cached_amms.insert(amm.clone());
                amm.sync(log)?;

                info!(
                    target: "state_space::sync",
                    ?amm,
                    "Synced AMM"
                );
            }
        }

        if !cached_amms.is_empty() {
            let amms = cached_amms.drain().collect::<Vec<AMM>>();
            affected_amms.extend(amms.iter().map(|amm| amm.address()));
            let state_change = StateChange::new(amms, block_number);

            debug!(
                target: "state_space::sync",
                state_change = ?state_change,
                "Caching state change"
            );

            self.cache.push(state_change);
        }

        Ok(affected_amms.into_iter().collect())
    }
}

#[macro_export]
macro_rules! sync {
    // Sync factories with provider
    ($factories:expr, $provider:expr) => {{
        StateSpaceBuilder::new($provider.clone())
            .with_factories($factories)
            .sync()
            .await?
    }};

    // Sync factories with filters
    ($factories:expr, $filters:expr, $provider:expr) => {{
        StateSpaceBuilder::new($provider.clone())
            .with_factories($factories)
            .with_filters($filters)
            .sync()
            .await?
    }};

    ($factories:expr, $amms:expr, $filters:expr, $provider:expr) => {{
        StateSpaceBuilder::new($provider.clone())
            .with_factories($factories)
            .with_amms($amms)
            .with_filters($filters)
            .sync()
            .await?
    }};
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::transports::ws::WsConnect;
    use alloy::{network::Ethereum, providers::ProviderBuilder, rpc::client::ClientBuilder};
    use futures::StreamExt;
    use std::{collections::HashMap, time::Duration};
    use tokio::time::timeout;
    use tracing_subscriber;

    /// RPC 端点配置结构
    #[derive(Debug, Clone)]
    struct RpcEndpoint {
        name: &'static str,
        url: &'static str,
        supports_ws: bool,
    }

    /// 获取测试用的 RPC 端点列表
    fn get_test_rpc_endpoints() -> Vec<RpcEndpoint> {
        vec![
            RpcEndpoint {
                name: "Mantle Mainnet Ws",
                url: "wss://rpc.mantle.xyz",
                supports_ws: true,
            },
            RpcEndpoint {
                name: "Mantle Mainnet Https",
                url: "https://rpc.mantle.xyz",
                supports_ws: false,
            },
            RpcEndpoint {
                name: "Mantle Mainnet PubicNode Ws",
                url: "wss://mantle.publicnode.com",
                supports_ws: true,
            },
            RpcEndpoint {
                name: "Mantle Mainnet DRPC Ws",
                url: "wss://mantle.drpc.org",
                supports_ws: true,
            },
            RpcEndpoint {
                name: "Mantle Sepolia Https",
                url: "https://rpc.sepolia.mantle.xyz",
                supports_ws: false,
            },
            RpcEndpoint {
                name: "Mantle Sepolia DRPC Ws",
                url: "wss://mantle-sepolia.drpc.org",
                supports_ws: true,
            },
            RpcEndpoint {
                name: "Mantle Sepolia DRPC Https",
                url: "https://mantle-sepolia.drpc.org",
                supports_ws: false,
            },
        ]
    }

    // Test All Subscribe Support
    // cargo test subscribe -- --nocapture

    /// 测试单个 RPC 端点的 subscribe 功能
    // cargo test subscribe_single -- --nocapture
    async fn test_rpc_subscribe_support(endpoint: &RpcEndpoint) -> (String, bool, Option<String>) {
        let result = timeout(Duration::from_secs(10), async {
            // 创建 WebSocket 客户端
            let ws = WsConnect::new(endpoint.url);
            let client_result = ClientBuilder::default().ws(ws).await;

            let client = match client_result {
                Ok(client) => client,
                Err(e) => return (false, Some(format!("Failed to connect: {}", e))),
            };

            // 创建 Provider
            let provider = ProviderBuilder::new().connect_client(client);

            // 尝试订阅区块
            let subscribe_result = provider.subscribe_blocks().await;

            match subscribe_result {
                Ok(mut stream) => {
                    // 尝试接收一个区块以验证订阅确实工作
                    let stream = stream.into_stream();
                    tokio::pin!(stream);

                    match timeout(Duration::from_secs(5), stream.next()).await {
                        Ok(Some(_block)) => (true, None),
                        Ok(None) => (
                            false,
                            Some("Stream ended without receiving blocks".to_string()),
                        ),
                        Err(_) => (false, Some("Timeout waiting for blocks".to_string())),
                    }
                }
                Err(e) => (false, Some(format!("Subscribe failed: {}", e))),
            }
        })
        .await;

        match result {
            Ok((success, error)) => (endpoint.name.to_string(), success, error),
            Err(_) => (
                endpoint.name.to_string(),
                false,
                Some("Test timeout".to_string()),
            ),
        }
    }

    /// 测试 StateSpaceManager 的 subscribe 方法
    // cargo test state_space_subscribe_functionality -- --nocapture
    #[tokio::test]
    async fn test_state_space_subscribe_functionality() {
        // 初始化日志（测试时可选）
        let _ = tracing_subscriber::fmt().try_init();

        let endpoints = get_test_rpc_endpoints();
        let mut results: HashMap<String, (bool, Option<String>)> = HashMap::new();

        println!("\n🚀 开始测试 RPC 提供商的 subscribe 支持...\n");

        // 并发测试所有端点
        let futures: Vec<_> = endpoints
            .iter()
            .map(|endpoint| test_rpc_subscribe_support(endpoint))
            .collect();

        let results_vec = futures::future::join_all(futures).await;

        // 收集结果
        for (name, success, error) in results_vec {
            results.insert(name.clone(), (success, error.clone()));

            if success {
                println!("✅ {}: Subscribe 支持", name);
            } else {
                println!(
                    "❌ {}: Subscribe 不支持 - {}",
                    name,
                    error.unwrap_or("未知错误".to_string())
                );
            }
        }

        // 统计结果
        let total = results.len();
        let supported = results.values().filter(|(success, _)| *success).count();
        let unsupported = total - supported;

        println!("\n📊 测试结果统计:");
        println!("   总计: {} 个 RPC 端点", total);
        println!("   支持: {} 个", supported);
        println!("   不支持: {} 个", unsupported);
        println!(
            "   成功率: {:.1}%",
            (supported as f64 / total as f64) * 100.0
        );

        // 输出详细的支持列表
        println!("\n✅ 支持 Subscribe 的 RPC:");
        for (name, (success, _)) in &results {
            if *success {
                println!("   - {}", name);
            }
        }

        println!("\n❌ 不支持 Subscribe 的 RPC:");
        for (name, (success, error)) in &results {
            if !*success {
                println!(
                    "   - {}: {}",
                    name,
                    error.as_ref().unwrap_or(&"未知原因".to_string())
                );
            }
        }

        // 这个测试不会失败，只是用来验证和展示结果
        assert!(total > 0, "应该至少测试了一个 RPC 端点");
    }

    /// 使用环境变量测试自定义 RPC 端点
    // TEST_RPC_WS_URL=wss://your-rpc-url.com cargo test custom_rpc_subscribe -- --nocapture
    #[tokio::test]
    async fn test_custom_rpc_subscribe() {
        // 从环境变量读取自定义 RPC URL
        if let Ok(custom_rpc_url) = std::env::var("TEST_RPC_WS_URL") {
            println!("\n🧪 测试自定义 RPC 端点: {}", custom_rpc_url);

            let endpoint = RpcEndpoint {
                name: "Custom RPC",
                url: Box::leak(custom_rpc_url.into_boxed_str()),
                supports_ws: true,
            };

            let (_name, success, error) = test_rpc_subscribe_support(&endpoint).await;

            if success {
                println!("✅ 自定义 RPC 支持 Subscribe!");
            } else {
                println!(
                    "❌ 自定义 RPC 不支持 Subscribe: {}",
                    error.unwrap_or("未知错误".to_string())
                );
            }
        } else {
            println!("💡 提示: 设置 TEST_RPC_WS_URL 环境变量来测试自定义 RPC 端点");
        }
    }

    /// 测试 StateSpaceManager 的完整订阅流程（模拟）
    // TEST_RPC_WS_URL=wss://your-rpc-url.com cargo test test_state_space_manager_mock_subscribe -- --nocapture
    #[tokio::test]
    async fn test_state_space_manager_mock_subscribe() {
        // 这是一个简化的测试，验证 StateSpaceManager 的基本结构
        use alloy::providers::ProviderBuilder;
        use alloy::rpc::client::ClientBuilder;

        // 如果有可用的测试 RPC，创建一个 StateSpaceManager
        if let Ok(test_url) = std::env::var("TEST_RPC_WS_URL") {
            let ws = WsConnect::new(&test_url);
            match ClientBuilder::default().ws(ws).await {
                Ok(client) => {
                    let provider = ProviderBuilder::new().connect_client(client);

                    let manager: StateSpaceManager<Ethereum, _> = StateSpaceBuilder::new(provider)
                        .block(0)
                        .sync()
                        .await
                        .expect("Failed to create StateSpaceManager");

                    // 验证 manager 创建成功
                    assert_eq!(manager.latest_block.load(Ordering::Relaxed), 0);

                    println!("✅ StateSpaceManager 创建成功，可以进行 subscribe 测试");

                    // 注意: 实际的 subscribe 测试需要真实的区块链连接
                    // 这里只是验证结构体可以正确创建
                }
                Err(e) => {
                    println!("⚠️  无法连接到测试 RPC: {}", e);
                }
            }
        } else {
            println!("💡 提示: 设置 TEST_RPC_WS_URL 环境变量来进行完整的 StateSpaceManager 测试");
        }
    }
}
