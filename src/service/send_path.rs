//! Production send path for the multi-protocol bot (WHI-860).
//!
//! Default remains **fail-closed**: [`production_send_allowed`] is `false` until
//! [`arm_production_send_path`] succeeds after explicit opt-in and on-chain
//! precondition checks. Offline / signerless behaviour is unchanged.
//!
//! ## Enablement
//!
//! * CLI: `--enable-sends` (or env `BOT_ENABLE_SENDS=1`)
//! * Hot signer: env `BOT_HOT_EXECUTOR_PRIVATE_KEY` only (never argv; never logged)
//! * Breaker caps: `BreakerConfig::from_env` (`MAX_*` WMNT caps)
//! * Intent fee caps: `IntentPolicy::from_env`
//!
//! ## Invariant (replaces hard-false)
//!
//! It is impossible to send without: a configured hot signer, a verified chain
//! id, a non-paused executor with the hot-executor role registered on-chain
//! (and hot ≠ admin), armed breakers, and an inventory/per-tx cap check before
//! signing.
//!
//! ## Kill switch
//!
//! 1. **Process**: SIGTERM / SIGINT (existing watch-loop shutdown)
//! 2. **In-process**: [`SendRuntime::kill`] / `BOT_SENDS_KILLED=1` → pause breakers
//! 3. **On-chain**: admin `pause()` on the executor (startup and role reads fail closed)

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use alloy::network::primitives::{BlockResponse, HeaderResponse};
use alloy::network::EthereumWallet;
use alloy::primitives::aliases::U112;
use alloy::primitives::{Address, B256, U256};
use alloy::providers::Provider;
use alloy::signers::local::PrivateKeySigner;
use eyre::{bail, eyre, Context, Result};
use tracing::{info, warn};

use crate::amms::amm::{AutomatedMarketMaker, AMM};
use crate::execution::breaker::{
    BreakerConfig, BreakerRuntime, PauseController, ScopeId, TracingAndMetricsAlertSink,
};
use crate::execution::{
    acquire_execute_send_guards, deadline_from_header_timestamp, execution_signer_roles_ok,
    prepare_pipeline_head, record_signed_submission, sign_under_guards, verify_execution_signer_roles,
    BlockFeeContext, BlockFeeContextCache, CandidateRef, ChainNonceView, DurableSubmissionHook,
    ExecutionContext, ExecutionContextView, ExecutionIdentity, ExecutionIdentityLease,
    ExecutionIdentitySource, ExecutionParams, ExecutionStage, Executor, ExecutorConfig, FeePolicy,
    FinalRequestParams, IArbitrageExecutor, IERC20, IdentityError, IntentPolicy, IntentStateMachine,
    PreparedPipelineHead, ProtocolKind, ProviderSemanticCallExecutor, RiskTieredPreflight,
    RouteKey, RuntimeGasProfile, RuntimeProfileConfig, VerifiedCrossingBuckets, WalDurableHook,
};
use crate::service::discovery::{
    simulate_mixed_path_with_route_key, AttemptJobContext, DiscoveredOpportunity,
};
use crate::service::protocol::ExecutionAttempt;
use crate::state_space::{
    hash_pinned_state_block_id, BlockHeaderContext, IdentityBarrier, MarketSnapshot,
    ProtocolCoverage, SnapshotId, SnapshotStatus,
};

// ---------------------------------------------------------------------------
// Gate
// ---------------------------------------------------------------------------

/// Process-global send arm. Starts false; only [`arm_production_send_path`] sets true.
static PRODUCTION_SEND_ARMED: AtomicBool = AtomicBool::new(false);

/// Serialises arm/disarm mutations in tests (and prevents dual arm races).
static GATE_MUTEX: Mutex<()> = Mutex::new(());

/// Production broadcast is allowed only after a successful arm.
///
/// Default `false` (byte-identical to the historical hard-false). Never flipped by
/// env alone — callers must pass explicit opt-in through [`arm_production_send_path`].
pub fn production_send_allowed() -> bool {
    PRODUCTION_SEND_ARMED.load(Ordering::SeqCst)
}

/// Force-disarm (kill switch + tests). Subsequent attempts hit the closed gate.
pub fn disarm_production_sends() {
    let _lock = GATE_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    PRODUCTION_SEND_ARMED.store(false, Ordering::SeqCst);
}

fn store_armed(value: bool) {
    let _lock = GATE_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    PRODUCTION_SEND_ARMED.store(value, Ordering::SeqCst);
}

// ---------------------------------------------------------------------------
// Config / env
// ---------------------------------------------------------------------------

/// Explicit hot-executor key material. Never accepted via argv.
pub const ENV_HOT_EXECUTOR_PRIVATE_KEY: &str = "BOT_HOT_EXECUTOR_PRIVATE_KEY";

/// Explicit opt-in for production sends (`1` / `true`).
pub const ENV_ENABLE_SENDS: &str = "BOT_ENABLE_SENDS";

/// In-process kill switch checked before each send (`1` / `true`).
pub const ENV_SENDS_KILLED: &str = "BOT_SENDS_KILLED";

/// Durable breaker store root (default `data/breaker`).
pub const ENV_BREAKER_STORE_DIR: &str = "BREAKER_STORE_DIR";

/// Whether CLI/env requested send enablement (opt-in only).
pub fn sends_opt_in_requested(cli_flag: bool) -> bool {
    if cli_flag {
        return true;
    }
    match std::env::var(ENV_ENABLE_SENDS) {
        Ok(v) => v == "1" || v.eq_ignore_ascii_case("true"),
        Err(_) => false,
    }
}

/// Whether the process-level kill-switch env var is set.
pub fn sends_killed_env() -> bool {
    match std::env::var(ENV_SENDS_KILLED) {
        Ok(v) => v == "1" || v.eq_ignore_ascii_case("true"),
        Err(_) => false,
    }
}

/// Load the hot executor private key from the dedicated env var.
///
/// Values are never logged. Rejects empty / invalid hex. Does **not** fall back to
/// loose aliases (`PRIVATE_KEY`, `EXECUTION_PRIVATE_KEY`, …) — those remain denylisted
/// for shadow mode and must not become silent production sources.
pub fn load_hot_executor_signer() -> Result<PrivateKeySigner> {
    let raw = std::env::var(ENV_HOT_EXECUTOR_PRIVATE_KEY).map_err(|_| {
        eyre!(
            "{ENV_HOT_EXECUTOR_PRIVATE_KEY} is required when --enable-sends / {ENV_ENABLE_SENDS}=1"
        )
    })?;
    load_hot_executor_signer_from_str(&raw)
}

/// Testable core of [`load_hot_executor_signer`].
pub fn load_hot_executor_signer_from_str(raw: &str) -> Result<PrivateKeySigner> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("{ENV_HOT_EXECUTOR_PRIVATE_KEY} is empty");
    }
    // Reject accidental argv-style embedding markers (never expected in env values).
    if trimmed.contains(' ') || trimmed.contains('\n') {
        bail!("{ENV_HOT_EXECUTOR_PRIVATE_KEY} must be a single hex private key (no whitespace)");
    }
    PrivateKeySigner::from_str(trimmed)
        .map_err(|_| eyre!("{ENV_HOT_EXECUTOR_PRIVATE_KEY} is not a valid private key"))
}

// ---------------------------------------------------------------------------
// Preconditions (pure where possible)
// ---------------------------------------------------------------------------

/// Failures that block arming the send path at startup.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SendPathArmError {
    #[error("sends not opted in (--enable-sends / {ENV_ENABLE_SENDS}=1 required)")]
    NotOptedIn,
    #[error("hot signer is not configured ({ENV_HOT_EXECUTOR_PRIVATE_KEY})")]
    MissingHotSigner,
    #[error("chain id is not verified (expected non-zero observed id matching declaration)")]
    ChainIdUnverified,
    #[error("executor is paused on-chain")]
    ExecutorPaused,
    #[error("hot signer is not registered as isHotExecutor on the executor")]
    HotExecutorUnregistered,
    #[error("hot signer must not equal executor admin")]
    HotEqualsAdmin,
    #[error("breakers are unarmed: {0}")]
    BreakersUnarmed(String),
    #[error("shadow mode cannot enable production sends")]
    ShadowModeConflict,
    #[error("offline mode cannot enable production sends")]
    OfflineConflict,
    #[error("{0}")]
    Other(String),
}

/// Pure role / config checks before any broadcast path is armed.
pub fn validate_send_preconditions(
    opted_in: bool,
    has_hot_signer: bool,
    chain_id_verified: bool,
    executor_paused: bool,
    is_hot_executor: bool,
    hot_signer: Address,
    admin: Address,
    guardian: Address,
    breakers_armed: bool,
    shadow_mode: bool,
    offline: bool,
) -> Result<(), SendPathArmError> {
    if offline {
        return Err(SendPathArmError::OfflineConflict);
    }
    if shadow_mode {
        return Err(SendPathArmError::ShadowModeConflict);
    }
    if !opted_in {
        return Err(SendPathArmError::NotOptedIn);
    }
    if !has_hot_signer {
        return Err(SendPathArmError::MissingHotSigner);
    }
    if !chain_id_verified {
        return Err(SendPathArmError::ChainIdUnverified);
    }
    if executor_paused {
        return Err(SendPathArmError::ExecutorPaused);
    }
    if !is_hot_executor {
        return Err(SendPathArmError::HotExecutorUnregistered);
    }
    if hot_signer == admin {
        return Err(SendPathArmError::HotEqualsAdmin);
    }
    execution_signer_roles_ok(admin, guardian, hot_signer, is_hot_executor)
        .map_err(|e| SendPathArmError::Other(e.to_string()))?;
    if !breakers_armed {
        return Err(SendPathArmError::BreakersUnarmed(
            "pause controller must be initialized and not paused".into(),
        ));
    }
    Ok(())
}

/// Inventory / per-tx cap: refuse oversized attempts before signing.
pub fn enforce_inventory_caps(
    amount_in: U256,
    max_input_per_tx_wmnt_wei: U256,
    executor_balance: U256,
    max_total_inventory_wmnt_wei: U256,
) -> Result<()> {
    if amount_in > max_input_per_tx_wmnt_wei {
        bail!(
            "attempt amount_in {amount_in} exceeds MAX_INPUT_PER_TX_WMNT_WEI {max_input_per_tx_wmnt_wei}"
        );
    }
    if executor_balance > max_total_inventory_wmnt_wei {
        bail!(
            "executor inventory {executor_balance} exceeds MAX_TOTAL_INVENTORY_WMNT_WEI {max_total_inventory_wmnt_wei}"
        );
    }
    if amount_in > executor_balance {
        bail!("attempt amount_in {amount_in} exceeds executor WMNT balance {executor_balance}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Bound identity source (send-capable)
// ---------------------------------------------------------------------------

/// Status-bound identity source that **can** grant send leases (unlike the
/// closed-gate `StatusBoundIdentitySource` in the monitor examples).
pub struct BoundSendIdentitySource {
    status: SnapshotStatus,
    fee_contexts: Arc<BlockFeeContextCache>,
    barrier: IdentityBarrier,
}

impl BoundSendIdentitySource {
    pub fn new(status: SnapshotStatus, fee_contexts: Arc<BlockFeeContextCache>) -> Self {
        Self {
            status,
            fee_contexts,
            barrier: IdentityBarrier::default(),
        }
    }
}

impl ExecutionIdentitySource for BoundSendIdentitySource {
    async fn validate(&self, identity: &ExecutionIdentity) -> Result<(), IdentityError> {
        let SnapshotStatus::Ready(snapshot) = &self.status else {
            return Err(IdentityError::PublisherNotReady);
        };
        if snapshot.id != identity.snapshot_id || snapshot.header != identity.header {
            return Err(IdentityError::StaleHeader);
        }
        if snapshot.coverage.pool_universe_fingerprint != Some(identity.pool_universe_fingerprint) {
            return Err(IdentityError::StaleTopology);
        }
        self.fee_contexts
            .matching(&identity.fee_context)
            .map_err(|e| IdentityError::StaleFeeContext(e.to_string()))?;
        Ok(())
    }

    async fn acquire_send_lease(
        &self,
        identity: &ExecutionIdentity,
    ) -> Result<ExecutionIdentityLease, IdentityError> {
        self.validate(identity).await?;
        let lease = self.barrier.acquire_lease().await;
        self.validate(identity).await?;
        Ok(ExecutionIdentityLease::new(identity.clone(), lease))
    }
}

// ---------------------------------------------------------------------------
// SendRuntime
// ---------------------------------------------------------------------------

/// Live send machinery held by the bot after a successful arm.
pub struct SendRuntime {
    wallet: EthereumWallet,
    signer_address: Address,
    executor: Executor,
    sm: Arc<IntentStateMachine>,
    pause: PauseController,
    breaker_cfg: BreakerConfig,
    durable: Arc<dyn DurableSubmissionHook + Send + Sync>,
    /// Optional coordinator for metrics / WAL (kept for kill/metrics; durable hook holds Arc).
    #[allow(dead_code)]
    breaker: Option<BreakerRuntime>,
    #[allow(dead_code)]
    chain_id: u64,
    executor_contract: Address,
    wmnt: Address,
}

impl SendRuntime {
    pub fn signer_address(&self) -> Address {
        self.signer_address
    }

    pub fn pause(&self) -> &PauseController {
        &self.pause
    }

    pub fn breaker_config(&self) -> &BreakerConfig {
        &self.breaker_cfg
    }

    /// In-process kill switch: pause breakers so further Execute attempts fail closed.
    pub fn kill(&self, reason: impl Into<String>) {
        let reason = reason.into();
        warn!(target: "bot.send", reason = %reason, "kill switch: pausing send path");
        self.pause.pause(reason);
        store_armed(false);
    }

    /// True when breakers admit Execute and the process gate is still armed.
    pub fn sends_live(&self) -> bool {
        production_send_allowed()
            && !self.pause.is_paused()
            && self.pause.is_initialized()
            && !sends_killed_env()
    }

    /// Submit one pure-protocol opportunity end-to-end (prepare → sign → broadcast).
    pub async fn submit_opportunity(
        &self,
        opp: &DiscoveredOpportunity,
        block_timestamp: u64,
        job_ctx: AttemptJobContext,
        header: BlockHeaderContext,
        pool_universe_fingerprint: B256,
    ) -> Result<ExecutionAttempt> {
        if sends_killed_env() {
            self.kill("BOT_SENDS_KILLED");
            bail!("send path killed via {ENV_SENDS_KILLED}");
        }
        if !self.sends_live() {
            bail!("send path is not live (disarmed or breaker paused)");
        }
        if opp.is_cross_protocol {
            bail!("production send path not enabled for mixed routes (canary is pure-protocol only)");
        }

        // Cap check before any signing material is touched.
        let balance = self
            .executor_wmnt_balance(opp.candidate.snapshot_id)
            .await
            .context("reading executor WMNT balance for inventory cap")?;
        enforce_inventory_caps(
            opp.candidate.input,
            U256::from(self.breaker_cfg.max_input_per_tx_wmnt_wei),
            balance,
            U256::from(self.breaker_cfg.max_total_inventory_wmnt_wei),
        )?;

        let (step_amounts_out, _final_out, route_key) = simulate_mixed_path_with_route_key(
            &opp.candidate.path,
            &opp.candidate.pools,
            opp.candidate.input,
            block_timestamp,
        )
        .map_err(|e| eyre!("re-simulate before send: {e}"))?;

        let inputs = execution_params_inputs_from_pools(
            &opp.candidate.pools,
            opp.candidate.token_path.clone(),
            step_amounts_out,
            // min_amount_out floor: use simulated output with no extra haircut here;
            // on-chain minProfit remains the principal backstop.
            opp.candidate.output,
            opp.candidate.net_profit,
        )?;

        let crossing = verified_crossing_buckets_from_route(&route_key);
        let params = ExecutionParams::new(
            opp.candidate.input,
            route_key.clone(),
            inputs.token_path,
            inputs.pool_addresses,
            inputs.pool_types,
            inputs.pool_tokens,
            inputs.expected_reserves_u112,
            inputs.step_amounts_out,
            inputs.min_amount_out,
            inputs.expected_net_profit_mnt_wei,
            crossing,
        )
        .map_err(|e| eyre!("ExecutionParams: {e}"))?;

        let candidate = CandidateRef {
            snapshot_id: opp.candidate.snapshot_id,
            header,
            pool_universe_fingerprint,
            route_key: route_key.clone(),
            amount_in: opp.candidate.input,
        };

        let fee_ctx = BlockFeeContext {
            block_number: candidate.snapshot_id.block_number,
            block_hash: candidate.snapshot_id.block_hash,
            base_fee_per_gas: job_ctx.base_fee_per_gas,
            block_gas_limit: job_ctx.block_gas_limit,
        };
        self.executor
            .context
            .block_fee_contexts()
            .publish(fee_ctx.clone())
            .map_err(|e| eyre!("publish fee context: {e}"))?;

        let quote = self
            .executor
            .context
            .gas_profile()
            .quote(&params.route_key)
            .map_err(|e| eyre!("gas profile quote: {e}"))?;
        let fee_plan = FeePolicy::new(
            self.executor.config.default_priority_fee_wei,
            self.executor.config.block_gas_limit_reserve,
        )
        .build(&quote, &fee_ctx)
        .map_err(|e| eyre!("fee plan: {e}"))?;

        let deadline = deadline_from_header_timestamp(
            header.block_timestamp,
            self.executor.config.execution_deadline_secs,
        )
        .map_err(|_| eyre!("deadline overflow"))?;

        let final_params = FinalRequestParams {
            params,
            candidate: candidate.clone(),
            fee_plan,
            deadline,
        };

        let mut coverage = ProtocolCoverage::default();
        coverage.pool_universe_fingerprint = Some(pool_universe_fingerprint);
        let status = SnapshotStatus::Ready(Arc::new(MarketSnapshot::new(
            candidate.snapshot_id,
            header,
            std::collections::HashMap::new(),
            coverage,
        )));

        let chain = self.fetch_chain_nonces().await?;
        let identity = BoundSendIdentitySource::new(
            status.clone(),
            Arc::clone(&self.executor.context.block_fee_contexts),
        );
        let call_executor =
            ProviderSemanticCallExecutor::new(self.executor.context.provider().clone());
        let preflight = RiskTieredPreflight::new(call_executor, ExecutionStage::Canary, None);

        let head = prepare_pipeline_head(
            Arc::clone(&self.sm),
            candidate,
            &status,
            fee_ctx,
            &self.executor,
            &identity,
            &preflight,
            final_params,
            chain.clone(),
        )
        .await
        .map_err(|e| eyre!("prepare_pipeline_head: {e}"))?;

        let tx_hash = self
            .sign_and_broadcast(head, &identity, chain)
            .await
            .context("sign_and_broadcast")?;

        info!(
            target: "bot.send",
            tx = %tx_hash,
            signature = %opp.candidate.signature,
            "production send submitted"
        );
        Ok(ExecutionAttempt::Submitted(tx_hash))
    }

    async fn sign_and_broadcast(
        &self,
        head: PreparedPipelineHead,
        identity: &BoundSendIdentitySource,
        chain: ChainNonceView,
    ) -> Result<B256> {
        let (sm, _chain_from_head, nonce, request) = head.into_open_send_parts();

        let guards = match acquire_execute_send_guards(
            &self.pause,
            identity,
            &self.executor,
            &sm,
            &request,
        )
        .await
        {
            Ok(g) => g,
            Err(e) => {
                let _ = sm.abort_prepare(nonce);
                let _ = sm.reconcile(chain);
                return Err(eyre!("acquire_execute_send_guards: {e}"));
            }
        };

        let (signed, meta) =
            match sign_under_guards(&self.executor, request, &self.wallet, &guards).await {
                Ok(v) => v,
                Err(e) => {
                    let _ = sm.abort_prepare(nonce);
                    let _ = sm.reconcile(chain);
                    return Err(eyre!("sign: {e}"));
                }
            };

        if let Err(e) = record_signed_submission(&sm, &signed, meta, &*self.durable) {
            let _ = sm.abort_prepare(nonce);
            let _ = sm.reconcile(chain);
            return Err(eyre!("record_signed_submission: {e}"));
        }

        let tx_hash = self
            .executor
            .broadcast(&signed)
            .await
            .context("broadcast")?;
        // guards drop after handoff (lease then pause).
        drop(guards);
        let _ = signed;
        Ok(tx_hash)
    }

    async fn fetch_chain_nonces(&self) -> Result<ChainNonceView> {
        let provider = self.executor.context.provider();
        let latest_nonce = provider
            .get_transaction_count(self.signer_address)
            .await
            .context("latest nonce")?;
        let pending_nonce = provider
            .get_transaction_count(self.signer_address)
            .pending()
            .await
            .context("pending nonce")?;
        Ok(ChainNonceView {
            latest_nonce,
            pending_nonce,
        })
    }

    async fn executor_wmnt_balance(&self, snapshot_id: SnapshotId) -> Result<U256> {
        let provider = self.executor.context.provider();
        let token = IERC20::new(self.wmnt, provider.clone());
        let amount = token
            .balanceOf(self.executor_contract)
            .call()
            .block(hash_pinned_state_block_id(snapshot_id.block_hash))
            .await
            .context("WMNT balanceOf")?;
        Ok(amount)
    }
}

// ---------------------------------------------------------------------------
// Arm
// ---------------------------------------------------------------------------

/// Inputs gathered by the bot after chain-id assertion and config load.
pub struct ArmSendPathRequest<'a, P> {
    pub provider: &'a P,
    pub chain_id: u64,
    pub executor_contract: Address,
    pub wmnt: Address,
    pub executor_config: ExecutorConfig,
    pub opted_in: bool,
    pub offline: bool,
    pub shadow_mode: bool,
    pub breaker_store: PathBuf,
}

/// Validate on-chain preconditions, construct [`SendRuntime`], and arm the gate.
pub async fn arm_production_send_path<P>(
    req: ArmSendPathRequest<'_, P>,
) -> Result<Arc<SendRuntime>, SendPathArmError>
where
    P: Provider + Clone + 'static,
{
    if !req.opted_in {
        return Err(SendPathArmError::NotOptedIn);
    }
    if req.offline {
        return Err(SendPathArmError::OfflineConflict);
    }
    if req.shadow_mode {
        return Err(SendPathArmError::ShadowModeConflict);
    }
    if req.chain_id == 0 {
        return Err(SendPathArmError::ChainIdUnverified);
    }

    let signer = load_hot_executor_signer().map_err(|e| {
        if e.to_string().contains("required") {
            SendPathArmError::MissingHotSigner
        } else {
            SendPathArmError::Other(e.to_string())
        }
    })?;
    let signer_address = signer.address();

    // On-chain reads at tip (arming is startup-only; per-block sends pin later).
    let tip = req
        .provider
        .get_block_number()
        .await
        .map_err(|e| SendPathArmError::Other(format!("eth_blockNumber: {e}")))?;
    let block = req
        .provider
        .get_block_by_number(alloy::eips::BlockNumberOrTag::Number(tip))
        .await
        .map_err(|e| SendPathArmError::Other(format!("get_block: {e}")))?
        .ok_or_else(|| SendPathArmError::Other(format!("tip block {tip} missing")))?;
    let block_hash = block.header().hash();

    let contract = IArbitrageExecutor::new(req.executor_contract, req.provider);
    let block_id = hash_pinned_state_block_id(block_hash);
    let paused = contract
        .paused()
        .call()
        .block(block_id)
        .await
        .map_err(|e| SendPathArmError::Other(format!("paused(): {e}")))?;
    let admin = contract
        .admin()
        .call()
        .block(block_id)
        .await
        .map_err(|e| SendPathArmError::Other(format!("admin(): {e}")))?;
    let guardian = contract
        .guardian()
        .call()
        .block(block_id)
        .await
        .map_err(|e| SendPathArmError::Other(format!("guardian(): {e}")))?;
    let is_hot = contract
        .isHotExecutor(signer_address)
        .call()
        .block(block_id)
        .await
        .map_err(|e| SendPathArmError::Other(format!("isHotExecutor(): {e}")))?;

    // Role policy (also covers guardian hygiene).
    if let Err(e) = execution_signer_roles_ok(admin, guardian, signer_address, is_hot) {
        let msg = e.to_string();
        if msg.contains("must not equal admin") {
            return Err(SendPathArmError::HotEqualsAdmin);
        }
        if msg.contains("not a hot executor") {
            return Err(SendPathArmError::HotExecutorUnregistered);
        }
        return Err(SendPathArmError::Other(msg));
    }
    // Redundant hash-pinned re-read path used by production services.
    verify_execution_signer_roles(
        req.provider,
        req.executor_contract,
        signer_address,
        block_hash,
    )
    .await
    .map_err(|e| SendPathArmError::Other(e.to_string()))?;

    if paused {
        return Err(SendPathArmError::ExecutorPaused);
    }

    let breaker_cfg = BreakerConfig::from_env().map_err(|e| {
        SendPathArmError::BreakersUnarmed(format!("BreakerConfig::from_env: {e}"))
    })?;
    let intent_policy =
        IntentPolicy::from_env().map_err(|e| SendPathArmError::Other(format!("IntentPolicy: {e}")))?;

    let alerts = Arc::new(TracingAndMetricsAlertSink);
    let scope = ScopeId {
        chain_id: req.chain_id,
        executor: req.executor_contract,
        signer: signer_address,
    };
    // Operator address for signed control commands is the hot signer for canary
    // (distinct cold operator key is WHI-861 / pause_control CLI).
    let breaker = BreakerRuntime::open(
        &req.breaker_store,
        scope,
        breaker_cfg.clone(),
        alerts,
        signer_address,
    )
    .map_err(|e| SendPathArmError::BreakersUnarmed(format!("BreakerRuntime::open: {e}")))?;

    // Arm breakers for canary: initialize + unpause after successful on-chain checks.
    // Trip conditions (consecutive reverts / loss window) re-pause via the coordinator.
    if !breaker.pause.is_initialized() {
        breaker.pause.mark_initialized();
    }
    if breaker.pause.is_paused() {
        breaker
            .pause
            .unpause("whi-860-send-path-arm")
            .map_err(|_| {
                SendPathArmError::BreakersUnarmed("failed to unpause after init".into())
            })?;
    }
    if !breaker.pause.is_initialized() || breaker.pause.is_paused() {
        return Err(SendPathArmError::BreakersUnarmed(
            "pause controller not ready after arm".into(),
        ));
    }

    let mut executor_config = req.executor_config;
    executor_config.chain_id = req.chain_id;

    let executor = build_executor_for_send(
        req.provider.clone(),
        req.executor_contract,
        req.wmnt,
        executor_config,
    )
    .await
    .map_err(|e| SendPathArmError::Other(e.to_string()))?;

    let latest_nonce = req
        .provider
        .get_transaction_count(signer_address)
        .await
        .map_err(|e| SendPathArmError::Other(format!("latest nonce: {e}")))?;
    let pending_nonce = req
        .provider
        .get_transaction_count(signer_address)
        .pending()
        .await
        .map_err(|e| SendPathArmError::Other(format!("pending nonce: {e}")))?;
    let chain = ChainNonceView {
        latest_nonce,
        pending_nonce,
    };

    let sm = Arc::new(
        IntentStateMachine::new(signer_address, chain, intent_policy, true)
            .map_err(|e| SendPathArmError::Other(format!("IntentStateMachine: {e}")))?,
    );
    sm.attach_accounting(Arc::clone(&breaker.coordinator) as _)
        .map_err(|e| SendPathArmError::Other(format!("attach_accounting: {e}")))?;

    let durable: Arc<dyn DurableSubmissionHook + Send + Sync> =
        Arc::new(WalDurableHook::new(Arc::clone(&breaker.coordinator)));
    let pause = breaker.pause.clone();

    let runtime = Arc::new(SendRuntime {
        wallet: EthereumWallet::from(signer),
        signer_address,
        executor,
        sm,
        pause,
        breaker_cfg,
        durable,
        breaker: Some(breaker),
        chain_id: req.chain_id,
        executor_contract: req.executor_contract,
        wmnt: req.wmnt,
    });

    // Final pure check (mirrors validate_send_preconditions for documentation).
    validate_send_preconditions(
        true,
        true,
        true,
        false,
        true,
        signer_address,
        admin,
        guardian,
        true,
        false,
        false,
    )
    .map_err(|e| SendPathArmError::Other(e.to_string()))?;

    store_armed(true);
    info!(
        target: "bot.send",
        signer = %signer_address,
        executor = %req.executor_contract,
        chain_id = req.chain_id,
        "production send path ARMED (hot executor; breakers initialized)"
    );
    Ok(runtime)
}

async fn build_executor_for_send<P: Provider + Clone + 'static>(
    provider: P,
    executor_contract: Address,
    wmnt_address: Address,
    executor_config: ExecutorConfig,
) -> Result<Executor> {
    let profile_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("config/gas_profiles/mantle_mainnet_v1.json");
    let gas_profile = RuntimeGasProfile::load(
        &profile_path,
        RuntimeProfileConfig::mantle_mainnet(Vec::new()),
    )
    .map_err(|e| eyre!("gas profile: {e}"))?;
    let block_fee_contexts = Arc::new(BlockFeeContextCache::default());
    let context = ExecutionContext::from_provider(
        provider,
        executor_contract,
        wmnt_address,
        gas_profile,
        block_fee_contexts,
    )
    .await?;
    Ok(Executor::new(context, executor_config))
}

pub fn default_breaker_store() -> PathBuf {
    std::env::var_os(ENV_BREAKER_STORE_DIR)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("data/breaker"))
}

// ---------------------------------------------------------------------------
// Params helpers (library-side copy of intent_service_support conversion)
// ---------------------------------------------------------------------------

struct ExecutionParamsInputs {
    token_path: Vec<Address>,
    pool_addresses: Vec<Address>,
    pool_types: Vec<u8>,
    pool_tokens: Vec<(Address, Address)>,
    expected_reserves_u112: Vec<U112>,
    step_amounts_out: Vec<U256>,
    min_amount_out: U256,
    expected_net_profit_mnt_wei: U256,
}

fn execution_params_inputs_from_pools(
    pools: &[AMM],
    token_path: Vec<Address>,
    step_amounts_out: Vec<U256>,
    min_amount_out: U256,
    expected_net_profit_mnt_wei: U256,
) -> Result<ExecutionParamsInputs> {
    let mut pool_addresses = Vec::with_capacity(pools.len());
    let mut pool_types = Vec::with_capacity(pools.len());
    let mut pool_tokens = Vec::with_capacity(pools.len());
    let mut expected_reserves_u112 = Vec::with_capacity(pools.len() * 2);

    for pool in pools {
        pool_addresses.push(pool.address());
        let (pool_type, token0, token1, reserve0, reserve1) = match pool {
            AMM::UniswapV2Pool(p) => (
                0u8,
                p.token_a.address,
                p.token_b.address,
                U112::try_from(p.reserve_0)
                    .map_err(|e| eyre!("pool {} reserve_0 exceeds uint112: {e}", p.address()))?,
                U112::try_from(p.reserve_1)
                    .map_err(|e| eyre!("pool {} reserve_1 exceeds uint112: {e}", p.address()))?,
            ),
            AMM::UniswapV3Pool(p) => (
                1u8,
                p.token_a.address,
                p.token_b.address,
                U112::ZERO,
                U112::ZERO,
            ),
            AMM::AgniPool(p) => (
                1u8,
                p.token_a.address,
                p.token_b.address,
                U112::ZERO,
                U112::ZERO,
            ),
            AMM::MoeLbPair(p) => (
                2u8,
                p.token_x.address,
                p.token_y.address,
                U112::ZERO,
                U112::ZERO,
            ),
        };
        pool_types.push(pool_type);
        pool_tokens.push((token0, token1));
        expected_reserves_u112.push(reserve0);
        expected_reserves_u112.push(reserve1);
    }

    Ok(ExecutionParamsInputs {
        token_path,
        pool_addresses,
        pool_types,
        pool_tokens,
        expected_reserves_u112,
        step_amounts_out,
        min_amount_out,
        expected_net_profit_mnt_wei,
    })
}

fn verified_crossing_buckets_from_route(route_key: &RouteKey) -> Option<VerifiedCrossingBuckets> {
    let has_v3_or_moe = route_key
        .protocols
        .iter()
        .any(|p| matches!(p, ProtocolKind::V3 | ProtocolKind::Moe));
    if !has_v3_or_moe {
        return None;
    }
    Some(VerifiedCrossingBuckets::new(
        route_key.v3_tick_crossings,
        route_key.moe_bin_crossings,
    ))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    struct ArmGuard;
    impl Drop for ArmGuard {
        fn drop(&mut self) {
            disarm_production_sends();
        }
    }

    #[test]
    fn production_send_allowed_defaults_false() {
        disarm_production_sends();
        assert!(!production_send_allowed());
    }

    #[test]
    fn store_armed_flips_gate() {
        let _g = ArmGuard;
        disarm_production_sends();
        assert!(!production_send_allowed());
        store_armed(true);
        assert!(production_send_allowed());
        store_armed(false);
        assert!(!production_send_allowed());
    }

    #[test]
    fn validate_preconditions_each_fail_closed_case() {
        let hot = address!("00000000000000000000000000000000000000a1");
        let admin = address!("00000000000000000000000000000000000000a2");
        let guardian = address!("00000000000000000000000000000000000000a3");

        assert_eq!(
            validate_send_preconditions(
                false, true, true, false, true, hot, admin, guardian, true, false, false
            ),
            Err(SendPathArmError::NotOptedIn)
        );
        assert_eq!(
            validate_send_preconditions(
                true, false, true, false, true, hot, admin, guardian, true, false, false
            ),
            Err(SendPathArmError::MissingHotSigner)
        );
        assert_eq!(
            validate_send_preconditions(
                true, true, false, false, true, hot, admin, guardian, true, false, false
            ),
            Err(SendPathArmError::ChainIdUnverified)
        );
        assert_eq!(
            validate_send_preconditions(
                true, true, true, true, true, hot, admin, guardian, true, false, false
            ),
            Err(SendPathArmError::ExecutorPaused)
        );
        assert_eq!(
            validate_send_preconditions(
                true, true, true, false, false, hot, admin, guardian, true, false, false
            ),
            Err(SendPathArmError::HotExecutorUnregistered)
        );
        assert_eq!(
            validate_send_preconditions(
                true, true, true, false, true, admin, admin, guardian, true, false, false
            ),
            Err(SendPathArmError::HotEqualsAdmin)
        );
        assert_eq!(
            validate_send_preconditions(
                true, true, true, false, true, hot, admin, guardian, false, false, false
            ),
            Err(SendPathArmError::BreakersUnarmed(
                "pause controller must be initialized and not paused".into()
            ))
        );
        assert_eq!(
            validate_send_preconditions(
                true, true, true, false, true, hot, admin, guardian, true, true, false
            ),
            Err(SendPathArmError::ShadowModeConflict)
        );
        assert_eq!(
            validate_send_preconditions(
                true, true, true, false, true, hot, admin, guardian, true, false, true
            ),
            Err(SendPathArmError::OfflineConflict)
        );
        assert!(validate_send_preconditions(
            true, true, true, false, true, hot, admin, guardian, true, false, false
        )
        .is_ok());
    }

    #[test]
    fn inventory_cap_refuses_oversized_before_sign() {
        let max_tx = U256::from(1000u64);
        let max_inv = U256::from(5000u64);
        let bal = U256::from(4000u64);
        assert!(enforce_inventory_caps(U256::from(500u64), max_tx, bal, max_inv).is_ok());
        let err = enforce_inventory_caps(U256::from(1001u64), max_tx, bal, max_inv)
            .unwrap_err()
            .to_string();
        assert!(err.contains("MAX_INPUT_PER_TX"), "{err}");
        let err = enforce_inventory_caps(U256::from(100u64), max_tx, U256::from(6000u64), max_inv)
            .unwrap_err()
            .to_string();
        assert!(err.contains("MAX_TOTAL_INVENTORY"), "{err}");
    }

    #[test]
    fn hot_signer_loader_rejects_empty_and_whitespace() {
        assert!(load_hot_executor_signer_from_str("").is_err());
        assert!(load_hot_executor_signer_from_str("   ").is_err());
        assert!(load_hot_executor_signer_from_str("dead beef").is_err());
        // Valid anvil #0 key
        let sk = load_hot_executor_signer_from_str(
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
        )
        .expect("anvil key");
        // Address must never appear as the secret; just check construction works.
        assert_ne!(sk.address(), Address::ZERO);
    }

    #[test]
    fn hot_signer_errors_never_echo_key_material() {
        let secret = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
        // Whitespace rejection path must not paste the secret into the error.
        let err = load_hot_executor_signer_from_str(&format!("{secret} trailing"))
            .unwrap_err()
            .to_string();
        assert!(
            !err.contains("ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"),
            "error must not contain key material: {err}"
        );
        // Invalid hex: reject without echoing the garbage value.
        let garbage = "0xnot-a-valid-private-key-material-zzzz";
        let err = load_hot_executor_signer_from_str(garbage)
            .unwrap_err()
            .to_string();
        assert!(
            !err.contains("zzzz") && !err.contains(garbage),
            "invalid-key error must not echo the value: {err}"
        );
        // Debug of a constructed signer must not print the raw secret.
        let sk = load_hot_executor_signer_from_str(secret).expect("anvil key");
        let dbg = format!("{sk:?}");
        assert!(
            !dbg.contains("ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"),
            "PrivateKeySigner Debug must not leak key: {dbg}"
        );
    }

    /// One test case per pure fail-closed arm precondition (AC: one test per case).
    #[test]
    fn arm_precondition_missing_hot_signer() {
        let hot = address!("00000000000000000000000000000000000000a1");
        let admin = address!("00000000000000000000000000000000000000a2");
        let guardian = address!("00000000000000000000000000000000000000a3");
        assert_eq!(
            validate_send_preconditions(
                true, false, true, false, true, hot, admin, guardian, true, false, false
            ),
            Err(SendPathArmError::MissingHotSigner)
        );
    }

    #[test]
    fn arm_precondition_chain_id_unverified() {
        let hot = address!("00000000000000000000000000000000000000a1");
        let admin = address!("00000000000000000000000000000000000000a2");
        let guardian = address!("00000000000000000000000000000000000000a3");
        assert_eq!(
            validate_send_preconditions(
                true, true, false, false, true, hot, admin, guardian, true, false, false
            ),
            Err(SendPathArmError::ChainIdUnverified)
        );
    }

    #[test]
    fn arm_precondition_executor_paused() {
        let hot = address!("00000000000000000000000000000000000000a1");
        let admin = address!("00000000000000000000000000000000000000a2");
        let guardian = address!("00000000000000000000000000000000000000a3");
        assert_eq!(
            validate_send_preconditions(
                true, true, true, true, true, hot, admin, guardian, true, false, false
            ),
            Err(SendPathArmError::ExecutorPaused)
        );
    }

    #[test]
    fn arm_precondition_hot_unregistered() {
        let hot = address!("00000000000000000000000000000000000000a1");
        let admin = address!("00000000000000000000000000000000000000a2");
        let guardian = address!("00000000000000000000000000000000000000a3");
        assert_eq!(
            validate_send_preconditions(
                true, true, true, false, false, hot, admin, guardian, true, false, false
            ),
            Err(SendPathArmError::HotExecutorUnregistered)
        );
    }

    #[test]
    fn arm_precondition_breakers_unarmed() {
        let hot = address!("00000000000000000000000000000000000000a1");
        let admin = address!("00000000000000000000000000000000000000a2");
        let guardian = address!("00000000000000000000000000000000000000a3");
        assert_eq!(
            validate_send_preconditions(
                true, true, true, false, true, hot, admin, guardian, false, false, false
            ),
            Err(SendPathArmError::BreakersUnarmed(
                "pause controller must be initialized and not paused".into()
            ))
        );
    }

    #[test]
    fn arm_precondition_hot_equals_admin() {
        let admin = address!("00000000000000000000000000000000000000a2");
        let guardian = address!("00000000000000000000000000000000000000a3");
        assert_eq!(
            validate_send_preconditions(
                true, true, true, false, true, admin, admin, guardian, true, false, false
            ),
            Err(SendPathArmError::HotEqualsAdmin)
        );
    }

    #[test]
    fn kill_switch_disarms_gate() {
        let _g = ArmGuard;
        store_armed(true);
        assert!(production_send_allowed());
        // Lightweight kill without full runtime: disarm is the gate half of the kill switch.
        disarm_production_sends();
        assert!(!production_send_allowed());
    }

    #[test]
    fn sends_opt_in_defaults_false_without_env() {
        // CLI false and no env → false. We cannot safely clear env in parallel tests;
        // only assert the pure CLI path.
        assert!(!sends_opt_in_requested(false) || std::env::var(ENV_ENABLE_SENDS).is_ok());
        assert!(sends_opt_in_requested(true));
    }

    /// Grep-style guard: Debug/Display of SendRuntime must not include key material.
    #[test]
    fn send_runtime_type_does_not_expose_raw_key_in_public_api() {
        // Compile-time / source guard: the production non-test portion of this
        // module must not format the private key or put it in error strings.
        let src = include_str!("send_path.rs");
        let production = src
            .split("#[cfg(test)]")
            .next()
            .expect("send_path has tests");
        for needle in [
            "format!(\"{raw}\"",
            "format!(\"{trimmed}\"",
            "println!(\"{raw}\"",
            "tracing::info!(key",
            "tracing::debug!(key",
            "error!(key",
        ] {
            assert!(
                !production.contains(needle),
                "send_path must not log key material ({needle})"
            );
        }
        // Env var name is fine; values must not appear in error messages beyond "invalid".
        assert!(production.contains(ENV_HOT_EXECUTOR_PRIVATE_KEY));
    }

    #[test]
    fn default_breaker_store_is_data_breaker_when_unset() {
        // When BREAKER_STORE_DIR is unset, default path is data/breaker.
        if std::env::var_os(ENV_BREAKER_STORE_DIR).is_none() {
            assert_eq!(default_breaker_store(), PathBuf::from("data/breaker"));
        }
    }

    #[test]
    fn path_helpers_accept_empty_v2_only_route() {
        assert!(verified_crossing_buckets_from_route(
            &RouteKey::new(vec![ProtocolKind::V2]).unwrap()
        )
        .is_none());
    }
}
