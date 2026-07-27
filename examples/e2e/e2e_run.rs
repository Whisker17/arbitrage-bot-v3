//! WHI-525 M6: Mantle Sepolia arbitrage E2E orchestrator.
//!
//! Composes the already-landed primitives (WHI-555 capability layer, WHI-556
//! non-mainnet gas-profile loading, WHI-551 runtime identity, WHI-521
//! risk-tiered preflight, production pipeline head + execute-send guards)
//! into one credential-isolated arb cycle against the fixture V2 + Agni V3
//! pools seeded by `e2e_bootstrap` / `e2e_trigger`.
//!
//! This example is *meant* to broadcast a real arb transaction when a human
//! operator runs it with real Sepolia credentials. A credentialed live run is
//! WHI-550 and out of scope for automated sessions — never execute this binary
//! from a sandbox that has no operator intent.
//!
//! Usage:
//! ```text
//! MANTLE_SEPOLIA_E2E_RPC_URL=... \
//! MANTLE_SEPOLIA_E2E_PRIVATE_KEY=... \
//! MANTLE_SEPOLIA_E2E_EXECUTOR_ADDRESS=... \
//!   cargo run --example e2e_run
//! ```

#[path = "../protocols/intent_service_support.rs"]
mod intent_service_support;

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use alloy::consensus::BlockHeader;
use alloy::eips::BlockId;
use alloy::network::primitives::{BlockResponse, HeaderResponse};
use alloy::primitives::{Address, B256, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::{BlockNumberOrTag, TransactionReceipt};
use clap::Parser;
use eyre::{bail, Context, Result};
use tokio::time::sleep;

use amms::amms::agni::AgniPool;
use amms::amms::amm::{AutomatedMarketMaker, AMM};
use amms::amms::uniswap_v2::UniswapV2Pool;
use amms::arbitrage::graph::build_graph;
use amms::arbitrage::optimizer::{pools_for_path, OptimizationConfig, PathOptimizer};
use amms::arbitrage::pathfinder::{PathConstraints, PathFinder};
use amms::execution::contract::IERC20;
use amms::execution::e2e::{
    deployment_manifest_digest, load_deployment_manifest, load_harness_config, validate_e2e_startup,
    write_evidence_bundle, E2eBootstrapAuthority, EnvSource, EvidenceBundle, EvidenceReceipt,
    ProcessEnvSource, ReconciliationRow, EVIDENCE_BUNDLE_SCHEMA_VERSION, ENV_E2E_RPC_URL,
    MANTLE_SEPOLIA_CHAIN_ID,
};
use amms::execution::gas_profile::{
    load_artifact, MarginPolicy, ProtocolKind, RouteKey, TickCrossingBucket,
};
use amms::execution::pause::AlwaysAllow;
use amms::execution::pipeline::NoopDurableHook;
use amms::execution::runtime_identity::{
    resolve_immutable_plan, verify_deployed_runtime, BuildEvidence, ImmutableInputs,
};
use amms::execution::{
    acquire_execute_send_guards, prepare_pipeline_head, BlockFeeContextCache, CanonicalBlock,
    ChainNonceView, ExecutionContext, ExecutionContextView, ExecutionParams, ExecutionStage,
    Executor, ExecutorConfig, FeePolicy, FinalRequestParams, IntentEvent, IntentStateMachine,
    LiveExecutionIdentitySource, ProviderSemanticCallExecutor, ReceiptOutcome, RiskTieredPreflight,
    RuntimeGasProfile, RuntimeProfileConfig,
};
use amms::state_space::{
    pool_universe_fingerprint, BlockHeaderContext, MarketSnapshot, PoolProtocol, PoolUniverseRow,
    ProtocolCoverage, SnapshotId, SnapshotPublisher, SnapshotStatus, StateSpace,
};

use intent_service_support::{
    candidate_ref, execution_params_inputs_from_pools, fee_context_for_candidate, finite_deadline,
    intent_policy_from_env_or_defaults, min_amount_out_from_plan,
    verified_crossing_buckets_from_route,
};

/// Fixture V2 fee in the same 1/100_000 unit as `UniswapV2Pool::fee` and
/// `E2EFixturePoolV2` (300 == 0.30%). Must match the deployed fixture.
const FIXTURE_V2_FEE: usize = 300;

const RECEIPT_POLL_INTERVAL: Duration = Duration::from_secs(3);
const RECEIPT_POLL_ATTEMPTS: u32 = 100;
const FINALITY_POLL_INTERVAL: Duration = Duration::from_secs(3);
const FINALITY_POLL_ATTEMPTS: u32 = 200;

#[derive(Debug, Parser)]
#[command(about = "WHI-525 M6: run one Mantle Sepolia fixture arb cycle through the production pipeline")]
struct Args {
    #[arg(long, default_value = "config/e2e_sepolia.json")]
    config: PathBuf,
    #[arg(long, default_value = "contracts/executor/artifacts")]
    executor_artifacts: PathBuf,
    /// Where to write the versioned evidence bundle for this run.
    #[arg(long, default_value = "config/e2e_sepolia_evidence.json")]
    evidence_out: PathBuf,
    /// Optional trigger tx hashes (from `e2e_trigger`) to pin in the evidence
    /// bundle. May be repeated.
    #[arg(long = "trigger-tx-hash")]
    trigger_tx_hashes: Vec<String>,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("e2e_run failed: {err:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let args = Args::parse();

    let env = ProcessEnvSource;
    let startup = validate_e2e_startup(&env).context("validating E2E startup env")?;

    // `validate_e2e_startup` validates the RPC URL's shape and then discards
    // it: re-read only to build the provider. Never attach the raw URL to an
    // error chain (userinfo/query tokens must not hit stderr).
    let rpc_url = env
        .get(ENV_E2E_RPC_URL)
        .ok_or_else(|| eyre::eyre!("{ENV_E2E_RPC_URL} unexpectedly absent after validation"))?;
    let rpc_http = rpc_url
        .parse()
        .map_err(|_| eyre::eyre!("invalid E2E RPC URL shape after validation"))?;
    let provider = ProviderBuilder::new().connect_http(rpc_http).erased();

    let authority = E2eBootstrapAuthority::establish(startup, provider.clone())
        .await
        .context("establishing E2E bootstrap authority")?;

    let harness_config =
        load_harness_config(&args.config).context("loading committed E2E harness config")?;
    if harness_config.chain_id != authority.chain_id() {
        bail!(
            "harness config chain id {} does not match live provider chain id {}",
            harness_config.chain_id,
            authority.chain_id()
        );
    }
    if harness_config.chain_id != MANTLE_SEPOLIA_CHAIN_ID {
        bail!(
            "e2e_run is hard-bound to Mantle Sepolia ({MANTLE_SEPOLIA_CHAIN_ID}); got {}",
            harness_config.chain_id
        );
    }

    let manifest_path = PathBuf::from(&harness_config.manifest_path);
    if !manifest_path.exists() {
        bail!(
            "no deployment manifest at {} — run `cargo run --example e2e_bootstrap` first",
            manifest_path.display()
        );
    }
    let deployment =
        load_deployment_manifest(&manifest_path).context("loading recorded deployment manifest")?;

    let signer = authority.signer_address();
    let chain_id = authority.chain_id();
    let manifest = authority.finalize();

    let wmnt = deployment
        .wmnt
        .parse::<Address>()
        .context("parsing manifest WMNT address")?;
    let executor_address = deployment
        .executor_address
        .parse::<Address>()
        .context("parsing manifest executor address")?;
    let pool_v2_addr = deployment
        .fixture_pool_v2
        .parse::<Address>()
        .context("parsing manifest fixture V2 pool address")?;
    let pool_agni_addr = deployment
        .fixture_pool_agni_v3
        .parse::<Address>()
        .context("parsing manifest fixture Agni V3 pool address")?;

    // --- Runtime identity (kept, not discarded) ---
    let evidence = BuildEvidence::load(&args.executor_artifacts)
        .context("loading executor build evidence")?;
    let plan = resolve_immutable_plan(&evidence, ImmutableInputs { wmnt }, chain_id)
        .context("resolving immutable plan for Sepolia WMNT")?;
    let on_chain_code = provider
        .get_code_at(executor_address)
        .await
        .context("reading deployed executor code")?;
    let verified_identity = verify_deployed_runtime(&on_chain_code, &plan)
        .context("verifying deployed runtime identity")?;

    // --- Sepolia gas profile via WHI-556 identity-bound loader ---
    let gas_artifact = load_artifact(std::path::Path::new(&harness_config.gas_profile_path))
        .context("loading Sepolia gas-profile artifact")?;
    let content_digest = gas_artifact.content_digest.clone();
    let required_routes = vec![
        RouteKey::new(vec![ProtocolKind::V2, ProtocolKind::V3])?
            .with_v3_ticks(TickCrossingBucket::Zero),
        RouteKey::new(vec![ProtocolKind::V3, ProtocolKind::V2])?
            .with_v3_ticks(TickCrossingBucket::Zero),
    ];
    let profile_config = RuntimeProfileConfig::from_verified_identity(
        &verified_identity,
        content_digest.clone(),
        MarginPolicy::default(),
        required_routes,
    );
    let gas_profile =
        RuntimeGasProfile::from_artifact_with_identity(gas_artifact, profile_config, &verified_identity)
            .context("loading RuntimeGasProfile against verified Sepolia identity")?;

    let block_fee_contexts = Arc::new(BlockFeeContextCache::default());
    let context = ExecutionContext::from_provider(
        provider.clone(),
        executor_address,
        wmnt,
        gas_profile.clone(),
        block_fee_contexts.clone(),
    )
    .await
    .context("building ExecutionContext (chain id + code hash + WMNT checks)")?;

    let mut executor_config = ExecutorConfig {
        chain_id,
        ..ExecutorConfig::default()
    };
    // Sepolia E2E uses the harness finality depth for SM confirmation, not the
    // mainnet default of 1 — otherwise on_new_block would finalize too early.
    let mut intent_policy = intent_policy_from_env_or_defaults()?;
    intent_policy.confirmation_depth = harness_config.finality_depth.max(1);
    if intent_policy.reorg_track_blocks < intent_policy.confirmation_depth {
        intent_policy.reorg_track_blocks = intent_policy.confirmation_depth;
    }
    executor_config.execution_deadline_secs = intent_policy.execution_deadline_secs;

    let executor = Executor::new(context, executor_config);

    // --- Fetch tip + on-chain pool state ---
    let tip_number = provider
        .get_block_number()
        .await
        .context("reading tip block number")?;
    let tip_block = provider
        .get_block_by_number(BlockNumberOrTag::Number(tip_number))
        .await
        .context("reading tip block")?
        .ok_or_else(|| eyre::eyre!("tip block {tip_number} missing"))?;
    let tip_header = tip_block.header();
    let tip_hash = tip_header.hash();
    let parent_hash = tip_header.parent_hash();
    let block_timestamp = tip_header.timestamp();
    let base_fee_per_gas = tip_header
        .base_fee_per_gas()
        .ok_or_else(|| eyre::eyre!("tip block {tip_number} has no base fee"))?
        as u128;
    let block_gas_limit = tip_header.gas_limit();

    let block_id = BlockId::number(tip_number);
    let v2_pool = UniswapV2Pool::new(pool_v2_addr, FIXTURE_V2_FEE)
        .init_fallback(provider.clone(), block_id)
        .await
        .context("initializing fixture V2 pool state")?;
    let agni_pool = AgniPool::new(pool_agni_addr)
        .init_basic(block_id, provider.clone())
        .await
        .context("initializing fixture Agni V3 pool state")?;

    let mut pools_map: HashMap<Address, AMM> = HashMap::new();
    pools_map.insert(pool_v2_addr, AMM::UniswapV2Pool(v2_pool));
    pools_map.insert(pool_agni_addr, AMM::AgniPool(agni_pool));

    // Fixture pools have no factory; use the zero address as the factory slot
    // so the fingerprint is still a pure function of (chain, settlement, rows).
    let fingerprint_rows = [
        PoolUniverseRow {
            protocol: PoolProtocol::UniswapV2,
            factory: Address::ZERO,
            pool: pool_v2_addr,
            token0: pools_map[&pool_v2_addr].tokens()[0],
            token1: pools_map[&pool_v2_addr].tokens()[1],
        },
        PoolUniverseRow {
            protocol: PoolProtocol::Agni,
            factory: Address::ZERO,
            pool: pool_agni_addr,
            token0: pools_map[&pool_agni_addr].tokens()[0],
            token1: pools_map[&pool_agni_addr].tokens()[1],
        },
    ];
    let pool_universe_fp = pool_universe_fingerprint(chain_id, wmnt, fingerprint_rows)
        .context("computing pool-universe fingerprint")?;

    let snapshot_id = SnapshotId::new(chain_id, tip_number, tip_hash);
    let header = BlockHeaderContext::new(parent_hash, block_timestamp);
    let mut coverage = ProtocolCoverage::default();
    coverage.pool_universe_fingerprint = Some(pool_universe_fp);
    let snapshot = MarketSnapshot::new(snapshot_id, header, pools_map.clone(), coverage);

    // --- Path find + optimize against the single snapshot ---
    let mut state = StateSpace::default();
    state.state = pools_map.clone();
    let graph = build_graph(&state).context("building pool graph")?;
    let constraints = PathConstraints {
        max_length: 2,
        required_start_token: Some(wmnt),
        required_end_token: Some(wmnt),
        ..PathConstraints::default()
    };
    let finder = PathFinder::new(&graph, constraints);
    let paths = finder.find_two_pool_misprices();
    if paths.is_empty() {
        bail!(
            "no two-pool misprice found between fixture V2 and Agni V3 — \
             run `cargo run --example e2e_trigger` first to create the imbalance"
        );
    }

    let state_pools: Vec<AMM> = pools_map.values().cloned().collect();
    let optimizer = PathOptimizer::new(OptimizationConfig::default());
    let mut best: Option<(amms::arbitrage::pathfinder::ArbitragePath, amms::arbitrage::optimizer::OptimizationResult, Vec<AMM>)> =
        None;
    for path in &paths {
        let path_pools = match pools_for_path(path, &state_pools) {
            Ok(p) => p,
            Err(_) => continue,
        };
        match optimizer.optimize(path, &path_pools) {
            Ok(Some(result)) => {
                let replace = match &best {
                    None => true,
                    Some((_, prev, _)) => result.expected_profit > prev.expected_profit,
                };
                if replace {
                    best = Some((path.clone(), result, path_pools));
                }
            }
            Ok(None) | Err(_) => continue,
        }
    }
    let (path, opt, path_pools) = best.ok_or_else(|| {
        eyre::eyre!(
            "path optimizer found no profitable input size — trigger a larger imbalance or reseed"
        )
    })?;

    // --- Route key + step amounts (with V3 crossing evidence) ---
    let (route_key, step_amounts_out, token_path) =
        build_route_and_steps(&path, &path_pools, opt.optimal_input, wmnt)?;

    let crossing_buckets = verified_crossing_buckets_from_route(&route_key);
    let min_amount_out =
        min_amount_out_from_plan(opt.optimal_input, opt.output_amount, &executor.config);
    let inputs = execution_params_inputs_from_pools(
        &path_pools,
        token_path,
        step_amounts_out,
        min_amount_out,
        opt.expected_profit,
    )?;

    let params = ExecutionParams::new(
        opt.optimal_input,
        route_key.clone(),
        inputs.token_path,
        inputs.pool_addresses,
        inputs.pool_types,
        inputs.pool_tokens,
        inputs.expected_reserves_u112,
        inputs.step_amounts_out,
        inputs.min_amount_out,
        inputs.expected_net_profit_mnt_wei,
        crossing_buckets,
    )
    .map_err(|e| eyre::eyre!("failed to build ExecutionParams: {e}"))?;

    // --- Fresh one-shot SnapshotPublisher → LiveExecutionIdentitySource ---
    let publisher = SnapshotPublisher::new();
    publisher.publish(snapshot).await;
    let identity_source =
        LiveExecutionIdentitySource::new(publisher, block_fee_contexts, gas_profile.clone());

    let candidate = candidate_ref(
        snapshot_id,
        header,
        pool_universe_fp,
        params.route_key.clone(),
        params.amount_in,
    )?;
    let fee_ctx = fee_context_for_candidate(&candidate, base_fee_per_gas, block_gas_limit);
    // Fee context must be published both into the executor cache (used by
    // FeePolicy) and through the identity source's barrier-guarded path.
    executor
        .context
        .block_fee_contexts()
        .publish(fee_ctx.clone())
        .map_err(|e| eyre::eyre!("publishing fee context: {e}"))?;
    identity_source
        .publish_fee_context(fee_ctx.clone())
        .await
        .map_err(|e| eyre::eyre!("identity-source fee context publish: {e}"))?;

    let quote = gas_profile
        .quote(&params.route_key)
        .map_err(|e| eyre::eyre!("gas profile quote for {}: {e}", params.route_key.key_string()))?;
    let fee_plan = FeePolicy::new(
        executor.config.default_priority_fee_wei,
        executor.config.block_gas_limit_reserve,
    )
    .build(&quote, &fee_ctx)
    .map_err(|e| eyre::eyre!("building fee plan: {e}"))?;
    let deadline = finite_deadline(&header, executor.config.execution_deadline_secs)?;
    let final_request_params = FinalRequestParams {
        params: params.clone(),
        candidate: candidate.clone(),
        fee_plan: fee_plan.clone(),
        deadline,
    };

    // --- Real chain nonces (never the hardcoded {0,0} stand-in) ---
    let latest_nonce = provider
        .get_transaction_count(signer)
        .await
        .context("fetching latest nonce")?;
    let pending_nonce = provider
        .get_transaction_count(signer)
        .pending()
        .await
        .context("fetching pending nonce")?;
    let chain = ChainNonceView {
        latest_nonce,
        pending_nonce,
    };
    if latest_nonce != pending_nonce {
        bail!(
            "signer has in-flight nonces (latest={latest_nonce}, pending={pending_nonce}); \
             wait for them to settle before e2e_run so the SM does not mark held_external"
        );
    }

    // production_send_allowed: true explicitly — capability layer is the real
    // gate; the SM field is recorded for observers / future enablement checks.
    let sm = Arc::new(
        IntentStateMachine::new(signer, chain.clone(), intent_policy, true)
            .context("constructing IntentStateMachine")?,
    );

    let status = SnapshotStatus::Ready(Arc::new(MarketSnapshot::new(
        snapshot_id,
        header,
        pools_map,
        {
            let mut c = ProtocolCoverage::default();
            c.pool_universe_fingerprint = Some(pool_universe_fp);
            c
        },
    )));

    let call_executor = ProviderSemanticCallExecutor::new(executor.context.provider());
    let preflight = RiskTieredPreflight::new(call_executor, ExecutionStage::E2e, None);

    // On-chain settlement baseline: executor WMNT balance before the arb send.
    let wmnt_token = IERC20::new(wmnt, provider.clone());
    let balance_before = wmnt_token
        .balanceOf(executor_address)
        .call()
        .await
        .context("reading executor WMNT balance before arb")?;

    // prepare_pipeline_head (not the closed wrapper): leaves Preparing open.
    let head = prepare_pipeline_head(
        Arc::clone(&sm),
        candidate,
        &status,
        fee_ctx.clone(),
        &executor,
        &identity_source,
        &preflight,
        final_request_params,
        chain.clone(),
    )
    .await
    .map_err(|e| eyre::eyre!("prepare_pipeline_head failed: {e}"))?;

    // Acquire pause + lease guards BEFORE minting the arb permit (head is
    // consumed by mint_arb_permit). Hold until after broadcast returns.
    let _send_guards = acquire_execute_send_guards(
        &AlwaysAllow,
        &identity_source,
        &executor,
        &sm,
        head.request(),
    )
    .await
    .map_err(|e| eyre::eyre!("acquire_execute_send_guards failed: {e}"))?;

    let permit = manifest
        .mint_arb_permit(head)
        .context("minting arb permit (consumes PreparedPipelineHead)")?;
    let submission = manifest
        .sign(permit, &NoopDurableHook)
        .await
        .context("signing arb permit")?;
    let arb_tx_hash = submission.tx_hash();
    manifest
        .broadcast(submission)
        .await
        .context("broadcasting arb transaction")?;
    // _send_guards drop here — after broadcast returns, matching `release` last
    // in EXECUTE_SEND_ORDER.

    println!("arb broadcast: tx_hash={arb_tx_hash}");

    // --- Finality-depth polling ---
    let receipt = wait_for_receipt(&provider, arb_tx_hash)
        .await
        .context("waiting for arb receipt")?;
    if !receipt.status() {
        bail!("arb transaction {arb_tx_hash} reverted on-chain");
    }
    let inclusion_block = receipt
        .block_number
        .ok_or_else(|| eyre::eyre!("arb receipt has no block number"))?;
    let inclusion_hash = receipt
        .block_hash
        .ok_or_else(|| eyre::eyre!("arb receipt has no block hash"))?;

    let (final_head_number, final_head_hash) = wait_for_finality_depth(
        &provider,
        inclusion_block,
        harness_config.finality_depth,
    )
    .await
    .context("waiting for finality depth")?;

    let outcome = ReceiptOutcome {
        success: receipt.status(),
        block_number: inclusion_block,
        block_hash: inclusion_hash,
        gas_used: receipt.gas_used,
        effective_gas_price: receipt.effective_gas_price,
        l1_fee: None,
        execution_layer_only: true,
    };
    let mut canonical = HashMap::new();
    canonical.insert(inclusion_block, inclusion_hash);
    let mut receipts_map = HashMap::new();
    receipts_map.insert(arb_tx_hash, Some(outcome.clone()));
    let post_latest = provider
        .get_transaction_count(signer)
        .await
        .context("fetching post-broadcast latest nonce")?;
    let post_pending = provider
        .get_transaction_count(signer)
        .pending()
        .await
        .context("fetching post-broadcast pending nonce")?;
    let events = sm
        .on_new_block(
            CanonicalBlock {
                number: final_head_number,
                hash: final_head_hash,
            },
            &canonical,
            &receipts_map,
            &HashMap::new(),
            ChainNonceView {
                latest_nonce: post_latest,
                pending_nonce: post_pending,
            },
        )
        .context("sm.on_new_block for finality")?;

    let finalized = events.iter().any(|e| {
        matches!(
            e,
            IntentEvent::Finalized {
                tx_hash,
                success: true,
                ..
            } if *tx_hash == arb_tx_hash
        )
    });
    if !finalized {
        // confirmation_depth may need more head advance relative to inclusion;
        // if the SM has not yet emitted Finalized, surface the events for the
        // operator rather than silently succeeding.
        bail!(
            "intent SM did not emit Finalized{{success:true}} for {arb_tx_hash} after \
             finality depth {}; events={events:?}",
            harness_config.finality_depth
        );
    }

    // --- Settlement assertions (on-chain WMNT delta, not simulated profit) ---
    let balance_after = wmnt_token
        .balanceOf(executor_address)
        .call()
        .await
        .context("reading executor WMNT balance after arb finality")?;
    let settlement_delta = balance_after.saturating_sub(balance_before);
    let gas_ok = receipt.gas_used <= fee_plan.gas_limit;
    let actual_cost = outcome.actual_cost();
    // Positive settlement after gas costs: on-chain WMNT increased, and the
    // increase covers more than pure noise (strictly > 0 after the arb).
    let profit_ok = balance_after > balance_before;

    let mut reconciliation = vec![
        ReconciliationRow {
            field: "receipt_success".to_string(),
            expected: "true".to_string(),
            observed: receipt.status().to_string(),
            ok: receipt.status(),
        },
        ReconciliationRow {
            field: "receipt_gas_used_vs_profile_limit".to_string(),
            expected: format!("<= {}", fee_plan.gas_limit),
            observed: receipt.gas_used.to_string(),
            ok: gas_ok,
        },
        ReconciliationRow {
            field: "on_chain_settlement_delta_wmnt".to_string(),
            expected: format!("> 0 (sim gross {})", opt.expected_profit),
            observed: settlement_delta.to_string(),
            ok: profit_ok,
        },
        ReconciliationRow {
            field: "actual_gas_cost_wei".to_string(),
            expected: "recorded".to_string(),
            observed: actual_cost.to_string(),
            ok: true,
        },
        ReconciliationRow {
            field: "pause_gate".to_string(),
            expected: "AlwaysAllow".to_string(),
            observed: "AlwaysAllow".to_string(),
            ok: true,
        },
    ];

    if !gas_ok {
        reconciliation.push(ReconciliationRow {
            field: "gas_requalification_needed".to_string(),
            expected: "within profile".to_string(),
            observed: "exceeded profile gas_limit".to_string(),
            ok: false,
        });
    }

    let mut deferrals = vec![
        "breaker not wired — AlwaysAllow (DI-12)".to_string(),
        "credentialed live Sepolia run tracked as WHI-550".to_string(),
    ];
    if args.trigger_tx_hashes.is_empty() {
        deferrals.push(
            "no --trigger-tx-hash supplied; evidence.trigger_tx_hashes is empty \
             (pass hashes printed by e2e_trigger to pin them)"
                .to_string(),
        );
    }

    let evidence_bundle = EvidenceBundle {
        schema_version: EVIDENCE_BUNDLE_SCHEMA_VERSION,
        chain_id,
        manifest_digest: deployment_manifest_digest(&deployment)
            .map_err(|e| eyre::eyre!("manifest digest: {e}"))?,
        venue_provenance: deployment.venue_provenance,
        adapters: harness_config.adapters.clone(),
        executor_address: executor_address.to_string(),
        signer_address: signer.to_string(),
        fixture_pool_v2: pool_v2_addr.to_string(),
        fixture_pool_agni_v3: pool_agni_addr.to_string(),
        identity_digest: deployment.identity_digest.clone(),
        patched_runtime_hash: deployment.patched_runtime_hash.clone(),
        gas_profile_content_digest: content_digest,
        gas_profile_identity: fee_plan.profile_identity.clone(),
        route_key: params.route_key.key_string(),
        amount_in: params.amount_in.to_string(),
        expected_net_profit_mnt_wei: params.expected_net_profit_mnt_wei.to_string(),
        min_amount_out: params.min_amount_out.to_string(),
        trigger_tx_hashes: args.trigger_tx_hashes.clone(),
        arb_tx_hash: arb_tx_hash.to_string(),
        executor_wmnt_before: balance_before.to_string(),
        executor_wmnt_after: balance_after.to_string(),
        settlement_delta_wmnt_wei: settlement_delta.to_string(),
        receipts: vec![EvidenceReceipt {
            label: "arb".to_string(),
            tx_hash: arb_tx_hash.to_string(),
            block_number: inclusion_block,
            block_hash: inclusion_hash.to_string(),
            gas_used: receipt.gas_used,
            effective_gas_price: receipt.effective_gas_price,
            success: receipt.status(),
            finality_depth: harness_config.finality_depth,
        }],
        reconciliation,
        preflight_stage: "E2e".to_string(),
        pause_gate: "AlwaysAllow".to_string(),
        snapshot_block_number: tip_number,
        snapshot_block_hash: tip_hash.to_string(),
        pool_universe_fingerprint: pool_universe_fp.to_string(),
        deferrals,
    };

    write_evidence_bundle(&args.evidence_out, &evidence_bundle)
        .map_err(|e| eyre::eyre!("writing evidence bundle: {e}"))?;

    if !gas_ok {
        bail!(
            "arb receipt gas_used {} exceeded profile gas_limit {}; evidence written to {}",
            receipt.gas_used,
            fee_plan.gas_limit,
            args.evidence_out.display()
        );
    }
    if !profit_ok {
        bail!(
            "on-chain settlement delta non-positive \
             (before={balance_before}, after={balance_after}, gas_cost={actual_cost}); \
             evidence written to {}",
            args.evidence_out.display()
        );
    }

    println!(
        "e2e_run complete: arb_tx={arb_tx_hash} settlement_delta_wmnt={settlement_delta} \
         gas_used={} evidence={}",
        receipt.gas_used,
        args.evidence_out.display()
    );
    Ok(())
}

/// Build the protocol route key, hop outputs, and WMNT-closed token path from
/// the optimized candidate. V3/Agni hops contribute tick-crossing evidence via
/// `simulate_swap_with_crossing_evidence`.
fn build_route_and_steps(
    path: &amms::arbitrage::pathfinder::ArbitragePath,
    pools: &[AMM],
    amount_in: U256,
    wmnt: Address,
) -> Result<(RouteKey, Vec<U256>, Vec<Address>)> {
    if path.hops.len() != pools.len() {
        bail!("path hops / pools length mismatch");
    }
    let mut protocols = Vec::with_capacity(path.hops.len());
    let mut step_amounts = Vec::with_capacity(path.hops.len());
    let mut current = amount_in;
    let mut total_v3_crossings = 0u32;
    let mut has_v3 = false;

    for (hop, pool) in path.hops.iter().zip(pools.iter()) {
        match pool {
            AMM::UniswapV2Pool(p) => {
                protocols.push(ProtocolKind::V2);
                let out = p
                    .simulate_swap(hop.token_in, hop.token_out, current)
                    .map_err(|e| eyre::eyre!("V2 simulate_swap: {e}"))?;
                step_amounts.push(out);
                current = out;
            }
            AMM::AgniPool(p) => {
                protocols.push(ProtocolKind::V3);
                has_v3 = true;
                let evidence = p
                    .simulate_swap_with_crossing_evidence(hop.token_in, current)
                    .map_err(|e| eyre::eyre!("Agni simulate_swap_with_crossing_evidence: {e}"))?;
                total_v3_crossings = total_v3_crossings.saturating_add(evidence.crossing_count);
                step_amounts.push(evidence.amount_out);
                current = evidence.amount_out;
            }
            AMM::UniswapV3Pool(p) => {
                protocols.push(ProtocolKind::V3);
                has_v3 = true;
                let evidence = p
                    .simulate_swap_with_crossing_evidence(hop.token_in, hop.token_out, current)
                    .map_err(|e| eyre::eyre!("V3 simulate_swap_with_crossing_evidence: {e}"))?;
                total_v3_crossings = total_v3_crossings.saturating_add(evidence.crossing_count);
                step_amounts.push(evidence.amount_out);
                current = evidence.amount_out;
            }
            AMM::MoeLbPair(_) => {
                bail!("Moe pools are out of scope for the WHI-525 fixture matrix");
            }
        }
    }

    let mut route_key = RouteKey::new(protocols)?;
    if has_v3 {
        route_key = route_key.with_v3_ticks(TickCrossingBucket::from_crossings(total_v3_crossings));
    }

    let mut token_path: Vec<Address> = path.hops.iter().map(|h| h.token_in).collect();
    if let Some(last) = path.hops.last() {
        token_path.push(last.token_out);
    }
    if token_path.first().copied() != Some(wmnt) || token_path.last().copied() != Some(wmnt) {
        bail!(
            "token path is not WMNT-closed: first={:?} last={:?}",
            token_path.first(),
            token_path.last()
        );
    }

    Ok((route_key, step_amounts, token_path))
}

async fn wait_for_receipt<P: Provider>(provider: &P, tx_hash: B256) -> Result<TransactionReceipt> {
    for _ in 0..RECEIPT_POLL_ATTEMPTS {
        if let Some(receipt) = provider
            .get_transaction_receipt(tx_hash)
            .await
            .context("polling for transaction receipt")?
        {
            return Ok(receipt);
        }
        sleep(RECEIPT_POLL_INTERVAL).await;
    }
    bail!("transaction {tx_hash} did not confirm after {RECEIPT_POLL_ATTEMPTS} poll attempts")
}

/// Poll until the tip is at least `finality_depth` blocks past `inclusion_block`.
/// Returns the tip (number, hash) that satisfies the depth requirement.
async fn wait_for_finality_depth<P: Provider>(
    provider: &P,
    inclusion_block: u64,
    finality_depth: u64,
) -> Result<(u64, B256)> {
    for _ in 0..FINALITY_POLL_ATTEMPTS {
        let tip = provider
            .get_block_number()
            .await
            .context("reading tip for finality")?;
        let depth = tip.saturating_sub(inclusion_block);
        if depth >= finality_depth {
            let block = provider
                .get_block_by_number(BlockNumberOrTag::Number(tip))
                .await
                .context("reading finality tip block")?
                .ok_or_else(|| eyre::eyre!("tip block {tip} missing"))?;
            return Ok((tip, block.header().hash()));
        }
        sleep(FINALITY_POLL_INTERVAL).await;
    }
    bail!(
        "finality depth {finality_depth} not reached for inclusion block {inclusion_block} \
         after {FINALITY_POLL_ATTEMPTS} poll attempts"
    )
}
