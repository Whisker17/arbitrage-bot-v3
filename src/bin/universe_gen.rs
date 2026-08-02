//! Offline unified pool-universe generator (WHI-793).
//!
//! Operator entry point: pin a block, load candidates, value + filter, write
//! one CSV + meta. **Default** seeds from legacy per-protocol CSVs (fast,
//! deterministic topology) and re-values every pool at the pinned block.
//! **`--discover`** re-enumerates from factories (slow; Moe log scan from
//! creation block) — use when refreshing the seed lists themselves.
//!
//! ```bash
//! # Typical regenerate: seed + re-value at chain tip
//! cargo run --release --bin universe_gen
//!
//! # Pin an explicit block for deterministic reruns
//! cargo run --release --bin universe_gen -- --block 98700000
//!
//! # Full factory discovery (slow)
//! cargo run --release --bin universe_gen -- --discover
//! ```
//!
//! Read-only RPC, no signer. FusionX V3 rows in legacy Agni lists are excluded
//! (not a `SelectedProtocol`). Mantle V2 currently operated under `agni-v2`
//! uses the FusionX V2 factory as an **interim** venue label until WHI-765
//! reclassifies Mantle DEXes — never silently typed as a different protocol.

use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Instant;

use alloy::consensus::BlockHeader;
use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::network::primitives::{BlockResponse, HeaderResponse};
use alloy::primitives::{Address, B256, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::client::ClientBuilder;
use alloy::sol;
use alloy::transports::layers::{RetryBackoffLayer, ThrottleLayer};
use amms::amms::agni::AgniFactory;
use amms::amms::amm::{AutomatedMarketMaker, AMM};
use amms::amms::factory::DiscoverySync;
use amms::amms::moe::{
    default_moe_pool_list_path, discover_moe_pool_list, CANONICAL_MOE_FACTORY,
    CANONICAL_MOE_FACTORY_CREATION_BLOCK,
};
use amms::amms::uniswap_v2::UniswapV2Factory;
use amms::service::{
    apply_universe_filters, build_meta, count_by_protocol, format_funnel_report,
    protocol_label_to_pool_protocol, write_quarantine, write_unified_csv, write_unified_meta,
    CandidatePool, CsvPoolUniverseSource, DEFAULT_MIN_TVL_WMNT_WEI, DEFAULT_POOL_UNIVERSE_REL,
    DEFAULT_WMNT,
};
use amms::state_space::{pool_universe_fingerprint, PoolProtocol, PoolUniverseRow, EFFECTIVE_MAX_HOPS};
use clap::Parser;
use eyre::{bail, Context, Result};
use tracing::{info, warn};

sol! {
    #[sol(rpc)]
    interface IERC20 {
        function balanceOf(address account) external view returns (uint256);
        function decimals() external view returns (uint8);
    }
}

/// Agni V3 factory on Mantle mainnet (see list_mantle_agni_pools).
const AGNI_V3_FACTORY: Address = alloy::primitives::address!("25780dc8Fc3cfBD75F33bFDAB65e969b603b2035");
const AGNI_V3_FACTORY_CREATION_BLOCK: u64 = 110_692;

/// FusionX V2 factory — interim venue for bot `SelectedProtocol::AgniV2`
/// until WHI-765 classifies Mantle DEXes. Documented in meta; not silent.
const FUSIONX_V2_FACTORY: Address =
    alloy::primitives::address!("E5020961fA51ffd3662CDf307dEf18F9a87Cce7c");
const FUSIONX_V2_FACTORY_CREATION_BLOCK: u64 = 0;

#[derive(Debug, Parser)]
#[command(
    name = "universe_gen",
    about = "Generate the unified multi-protocol pool universe (WHI-793)"
)]
struct Args {
    /// Output CSV path (meta written alongside as `{stem}.meta.json`).
    #[arg(long, default_value = DEFAULT_POOL_UNIVERSE_REL, env = "BOT_POOL_UNIVERSE")]
    out: PathBuf,

    /// HTTP RPC URL.
    #[arg(long, env = "MANTLE_HTTP_URL")]
    rpc: Option<String>,

    /// Pin block: number, or `head`/`latest`. Default: head.
    #[arg(long, default_value = "head", env = "UNIVERSE_GEN_BLOCK")]
    block: String,

    /// Minimum TVL in WMNT wei (decimal). Default: 1000 WMNT.
    #[arg(long, env = "UNIVERSE_GEN_MIN_TVL_WMNT_WEI")]
    min_tvl_wmnt_wei: Option<String>,

    /// Max hops for settlement cycles. Must equal `EFFECTIVE_MAX_HOPS` (3);
    /// the flag exists only for explicit operator documentation, not override.
    #[arg(long, default_value_t = EFFECTIVE_MAX_HOPS)]
    max_hops: u8,

    /// Settlement asset (default WMNT).
    #[arg(long)]
    settlement: Option<String>,

    /// Seed from legacy CSVs instead of factory discovery (default true unless --discover).
    #[arg(long, default_value = "data/poolLists.csv")]
    seed_v3: PathBuf,

    #[arg(long, default_value = "data/poolLists_v2.csv")]
    seed_v2: PathBuf,

    #[arg(long, default_value = "data/poolLists_moe.csv")]
    seed_moe: PathBuf,

    /// Discover pools from factories (slow). When set, seeds are not used
    /// unless a seed file is the only source for a venue that has no factory
    /// discovery configured.
    #[arg(long, default_value_t = false)]
    discover: bool,

    /// Skip on-chain balance valuation; mark every pool as valued at U256::MAX
    /// (cycle-filter-only). For unit/fixture generation — not for production.
    #[arg(long, default_value_t = false)]
    skip_tvl: bool,

    /// Include agni-v2 (FusionX V2 interim) in enumeration. Default true.
    #[arg(long, default_value_t = true)]
    include_v2: bool,

    /// V2 factory used when --discover (default FusionX V2 interim).
    #[arg(long, env = "AGNI_V2_FACTORY_ADDRESS")]
    v2_factory: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    let min_tvl = match &args.min_tvl_wmnt_wei {
        Some(s) => U256::from_str(s).with_context(|| format!("parse min_tvl_wmnt_wei={s}"))?,
        None => U256::from(DEFAULT_MIN_TVL_WMNT_WEI),
    };
    let settlement = match &args.settlement {
        Some(s) => Address::from_str(s).context("parse --settlement")?,
        None => DEFAULT_WMNT,
    };
    if args.max_hops != EFFECTIVE_MAX_HOPS {
        bail!(
            "--max-hops must equal EFFECTIVE_MAX_HOPS ({EFFECTIVE_MAX_HOPS}); got {}. \
             Do not introduce a second hop cap (WHI-793 / pool_universe fingerprint domain).",
            args.max_hops
        );
    }
    let max_hops = EFFECTIVE_MAX_HOPS;

    let rpc = args
        .rpc
        .clone()
        .or_else(|| std::env::var("MANTLE_PROVIDER_URL").ok())
        .or_else(|| std::env::var("RPC_HTTP_URL").ok())
        .unwrap_or_else(|| "https://rpc.mantle.xyz".to_string());

    let client = ClientBuilder::default()
        .layer(ThrottleLayer::new(40))
        .layer(RetryBackoffLayer::new(8, 250, 500))
        .http(rpc.parse().context("parse RPC url")?);
    let provider = ProviderBuilder::new().connect_client(client);

    let chain_id = provider.get_chain_id().await.context("eth_chainId")?;
    let (snapshot_block, snapshot_hash, timestamp) =
        pin_block(&provider, &args.block).await.context("pin block")?;

    info!(
        chain_id,
        snapshot_block,
        %snapshot_hash,
        timestamp = ?timestamp,
        settlement = %settlement,
        min_tvl = %min_tvl,
        max_hops,
        discover = args.discover,
        "pinned universe generation block"
    );

    let started = Instant::now();
    let mut candidates = if args.discover {
        discover_all(&provider, &args, snapshot_block).await?
    } else {
        seed_from_legacy(&args)?
    };

    // Drop any zero-token shells; quarantine would also catch them later.
    let before = candidates.len();
    candidates.retain(|c| c.token0 != Address::ZERO && c.token1 != Address::ZERO);
    if candidates.len() != before {
        warn!(
            dropped = before - candidates.len(),
            "dropped candidates with zero token addresses"
        );
    }

    let enumerated_by = count_by_protocol(&candidates);
    info!(?enumerated_by, total = candidates.len(), "enumerated candidates");

    let valuations = if args.skip_tvl {
        warn!("--skip-tvl: assigning synthetic max TVL to every pool (not for production)");
        candidates
            .iter()
            .map(|c| (c.pool, Some(U256::MAX)))
            .collect()
    } else {
        value_pools_wmnt(&provider, &candidates, settlement, snapshot_block).await?
    };

    let result = apply_universe_filters(
        candidates,
        &valuations,
        min_tvl,
        settlement,
        max_hops,
    );

    let stages = vec![
        ("enumerated".into(), enumerated_by),
        ("tvl_surviving".into(), count_by_protocol(
            // Reconstruct TVL survivors: kept + cycle_rejected
            &{
                let mut v = result.kept.clone();
                v.extend(result.cycle_rejected.clone());
                v
            },
        )),
        ("emitted".into(), count_by_protocol(&result.kept)),
    ];
    print!("{}", format_funnel_report(&result.funnel, &stages));
    println!(
        "quarantine ({}):",
        result.quarantine.len()
    );
    for q in result.quarantine.iter().take(20) {
        println!("  {:?} {} — {}", q.pool, q.protocol, q.reason);
    }
    if result.quarantine.len() > 20 {
        println!("  … {} more", result.quarantine.len() - 20);
    }

    let rows_for_fp: Vec<PoolUniverseRow> = result
        .kept
        .iter()
        .map(|c| {
            let protocol = protocol_label_to_pool_protocol(&c.protocol).map_err(|e| {
                eyre::eyre!("fingerprint: bad protocol label on pool {:?}: {e}", c.pool)
            })?;
            Ok(PoolUniverseRow {
                protocol,
                factory: c.factory,
                pool: c.pool,
                token0: c.token0,
                token1: c.token1,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let fingerprint = pool_universe_fingerprint(chain_id, settlement, rows_for_fp)
        .context("pool_universe_fingerprint")?;

    write_unified_csv(&args.out, &result.kept).context("write unified csv")?;
    let meta = build_meta(
        chain_id,
        snapshot_block,
        snapshot_hash,
        timestamp,
        settlement,
        &result.kept,
        max_hops,
        min_tvl,
        Some(fingerprint),
    );
    write_unified_meta(&args.out, &meta).context("write meta")?;
    write_quarantine(&args.out, &result.quarantine).context("write quarantine")?;

    // Determinism check: re-write and compare is implied by sorted write; reload.
    let reloaded = amms::service::read_unified_csv(&args.out).context("reload")?;
    if reloaded.len() != result.kept.len() {
        bail!(
            "reload length mismatch: wrote {} reloaded {}",
            result.kept.len(),
            reloaded.len()
        );
    }

    info!(
        out = %args.out.display(),
        pools = result.kept.len(),
        fingerprint = %fingerprint,
        elapsed_secs = started.elapsed().as_secs(),
        "wrote unified pool universe + meta + quarantine"
    );
    println!(
        "OK wrote {} pools → {} (+ meta, quarantine) fingerprint={}",
        result.kept.len(),
        args.out.display(),
        fingerprint
    );
    Ok(())
}

async fn pin_block(
    provider: &impl Provider,
    spec: &str,
) -> Result<(u64, B256, Option<u64>)> {
    let number = if spec.eq_ignore_ascii_case("head") || spec.eq_ignore_ascii_case("latest") {
        provider.get_block_number().await.context("get_block_number")?
    } else {
        spec.parse::<u64>()
            .with_context(|| format!("parse --block={spec}"))?
    };
    let block = provider
        .get_block_by_number(BlockNumberOrTag::Number(number))
        .await
        .context("get_block_by_number")?
        .ok_or_else(|| eyre::eyre!("block {number} not found"))?;
    let header = block.header();
    Ok((number, header.hash(), Some(header.timestamp())))
}

fn seed_from_legacy(args: &Args) -> Result<Vec<CandidatePool>> {
    let mut out = Vec::new();

    // Agni-V3 (+ filter out FusionX rows via protocol filter)
    if args.seed_v3.exists() {
        let source = CsvPoolUniverseSource::new(&args.seed_v3, PoolProtocol::Agni, AGNI_V3_FACTORY)
            .with_protocol_filter("agni");
        let rows = source.read_rows().context("read seed v3")?;
        for r in rows {
            out.push(CandidatePool {
                protocol: "agni-v3".into(),
                factory: if r.factory == Address::ZERO {
                    AGNI_V3_FACTORY
                } else {
                    r.factory
                },
                pool: r.pool,
                token0: r.token0,
                token1: r.token1,
                fee_tier: None,
                bin_step: None,
                creation_block: None,
            });
        }
        info!(path = %args.seed_v3.display(), n = out.len(), "seeded agni-v3");
    } else {
        warn!(path = %args.seed_v3.display(), "seed v3 missing");
    }

    // Agni-V2 / interim FusionX V2
    if args.include_v2 {
        if args.seed_v2.exists() {
            let factory = args
                .v2_factory
                .as_deref()
                .map(Address::from_str)
                .transpose()
                .context("parse v2 factory")?
                .unwrap_or(FUSIONX_V2_FACTORY);
            let source = CsvPoolUniverseSource::new(&args.seed_v2, PoolProtocol::UniswapV2, factory);
            let rows = source.read_rows().context("read seed v2")?;
            let before = out.len();
            for r in rows {
                out.push(CandidatePool {
                    protocol: "agni-v2".into(),
                    factory,
                    pool: r.pool,
                    token0: r.token0,
                    token1: r.token1,
                    fee_tier: None,
                    bin_step: None,
                    creation_block: None,
                });
            }
            info!(
                path = %args.seed_v2.display(),
                n = out.len() - before,
                "seeded agni-v2 (interim FusionX V2 venue; WHI-765 pending)"
            );
        } else {
            warn!(
                path = %args.seed_v2.display(),
                "seed v2 missing — agni-v2 will be empty (bot default protocols include agni-v2)"
            );
        }
    }

    // Moe
    let moe_path = if args.seed_moe.exists() {
        args.seed_moe.clone()
    } else {
        default_moe_pool_list_path()
    };
    if moe_path.exists() {
        let list = amms::amms::moe::MoePoolList::load_path(&moe_path).context("load moe seed")?;
        let before = out.len();
        for e in list.entries {
            out.push(CandidatePool {
                protocol: "moe".into(),
                factory: e.factory,
                pool: e.pool,
                token0: e.token_x,
                token1: e.token_y,
                fee_tier: None,
                bin_step: Some(e.bin_step),
                creation_block: Some(e.creation_block),
            });
        }
        info!(path = %moe_path.display(), n = out.len() - before, "seeded moe");
    } else {
        warn!(path = %moe_path.display(), "seed moe missing");
    }

    if out.is_empty() {
        bail!("no seed candidates loaded; provide CSVs or pass --discover");
    }
    Ok(out)
}

async fn discover_all(
    provider: &impl Provider,
    args: &Args,
    to_block: u64,
) -> Result<Vec<CandidatePool>> {
    let mut out = Vec::new();
    let block_id = BlockId::Number(to_block.into());

    // Agni V3
    info!(factory = %AGNI_V3_FACTORY, "discovering Agni V3 pools");
    let factory = AgniFactory::new(AGNI_V3_FACTORY, AGNI_V3_FACTORY_CREATION_BLOCK);
    let pools = factory
        .discover::<_, _>(block_id, provider)
        .await
        .context("agni v3 discover")?;
    // Sync to fill tokens
    let pools = factory
        .sync::<_, _>(pools, block_id, provider)
        .await
        .context("agni v3 sync")?;
    for amm in pools {
        if let AMM::AgniPool(p) = amm {
            out.push(CandidatePool {
                protocol: "agni-v3".into(),
                factory: AGNI_V3_FACTORY,
                pool: p.address(),
                token0: p.token_a.address,
                token1: p.token_b.address,
                fee_tier: Some(p.fee),
                bin_step: None,
                creation_block: None,
            });
        }
    }
    info!(n = out.len(), "agni-v3 discover done");

    // V2 interim
    if args.include_v2 {
        let factory_addr = args
            .v2_factory
            .as_deref()
            .map(Address::from_str)
            .transpose()
            .context("parse v2 factory")?
            .unwrap_or(FUSIONX_V2_FACTORY);
        info!(factory = %factory_addr, "discovering V2 pools (agni-v2 interim)");
        let v2 = UniswapV2Factory::new(factory_addr, 30, FUSIONX_V2_FACTORY_CREATION_BLOCK);
        match v2.discover::<_, _>(block_id, provider).await {
            Ok(discovered) => {
                let pools = match v2.sync::<_, _>(discovered.clone(), block_id, provider).await {
                    Ok(synced) => synced,
                    Err(e) => {
                        warn!(error = %e, "v2 sync failed; using unsynced shells");
                        discovered
                    }
                };
                let before = out.len();
                for amm in pools {
                    if let AMM::UniswapV2Pool(p) = amm {
                        out.push(CandidatePool {
                            protocol: "agni-v2".into(),
                            factory: factory_addr,
                            pool: p.address(),
                            token0: p.token_a.address,
                            token1: p.token_b.address,
                            fee_tier: Some(p.fee as u32),
                            bin_step: None,
                            creation_block: None,
                        });
                    }
                }
                info!(n = out.len() - before, "agni-v2 discover done");
            }
            Err(e) => warn!(error = %e, "v2 discover failed; continuing without"),
        }
    }

    // Moe
    info!(factory = %CANONICAL_MOE_FACTORY, to_block, "discovering Moe LB pools");
    let list = discover_moe_pool_list(
        provider,
        CANONICAL_MOE_FACTORY,
        CANONICAL_MOE_FACTORY_CREATION_BLOCK,
        to_block,
    )
    .await
    .context("discover_moe_pool_list")?;
    let before = out.len();
    for e in list.entries {
        out.push(CandidatePool {
            protocol: "moe".into(),
            factory: e.factory,
            pool: e.pool,
            token0: e.token_x,
            token1: e.token_y,
            fee_tier: None,
            bin_step: Some(e.bin_step),
            creation_block: Some(e.creation_block),
        });
    }
    info!(n = out.len() - before, "moe discover done");

    Ok(out)
}

/// WMNT-equivalent TVL via ERC20 balances at the pool address (integer U256 only).
///
/// * Pool holds WMNT: `tvl = 2 * wmnt_balance` (50/50).
/// * Else: price each side via a direct WMNT pair's reserve ratio when a
///   WMNT-connected pool for that token exists among candidates; else
///   `None` (quarantine).
/// * `balanceOf` / RPC failure → `None` (quarantine), never silent zero.
async fn value_pools_wmnt(
    provider: &impl Provider,
    candidates: &[CandidatePool],
    wmnt: Address,
    block: u64,
) -> Result<HashMap<Address, Option<U256>>> {
    let block_id = BlockId::Number(block.into());
    // None in the balance map means the RPC call failed (quarantine later).
    let mut balances: HashMap<(Address, Address), Option<U256>> = HashMap::new();
    let mut decimals: HashMap<Address, u8> = HashMap::new();
    decimals.insert(wmnt, 18);

    let mut tokens = std::collections::BTreeSet::new();
    for c in candidates {
        tokens.insert(c.token0);
        tokens.insert(c.token1);
    }

    for &token in &tokens {
        if token == Address::ZERO || decimals.contains_key(&token) {
            continue;
        }
        let erc = IERC20::new(token, provider);
        let d = match erc.decimals().call().block(block_id).await {
            Ok(d) => d,
            Err(_) => 18u8,
        };
        decimals.insert(token, d);
    }

    for c in candidates {
        for token in [c.token0, c.token1] {
            if token == Address::ZERO {
                continue;
            }
            let key = (token, c.pool);
            if balances.contains_key(&key) {
                continue;
            }
            let erc = IERC20::new(token, provider);
            let bal = match erc.balanceOf(c.pool).call().block(block_id).await {
                Ok(b) => Some(b),
                Err(e) => {
                    warn!(pool = %c.pool, token = %token, error = %e, "balanceOf failed → quarantine");
                    None
                }
            };
            balances.insert(key, bal);
        }
    }

    // price_x18[token] = WMNT-wei value of 1e18 normalized token units
    // (from direct WMNT pairs). Built with integer ratio only.
    let mut price_x18: HashMap<Address, U256> = HashMap::new();
    price_x18.insert(wmnt, U256::from(10u64).pow(U256::from(18u64))); // 1e18 WMNT per 1e18 WMNT
    const WAD: u64 = 1_000_000_000_000_000_000; // 1e18

    for c in candidates {
        let other = if c.token0 == wmnt {
            c.token1
        } else if c.token1 == wmnt {
            c.token0
        } else {
            continue;
        };
        let Some(Some(bal_w)) = balances.get(&(wmnt, c.pool)) else {
            continue;
        };
        let Some(Some(bal_o)) = balances.get(&(other, c.pool)) else {
            continue;
        };
        if bal_w.is_zero() || bal_o.is_zero() {
            continue;
        }
        let d_w = *decimals.get(&wmnt).unwrap_or(&18);
        let d_o = *decimals.get(&other).unwrap_or(&18);
        let w_n = normalize_to_18(*bal_w, d_w);
        let o_n = normalize_to_18(*bal_o, d_o);
        if o_n.is_zero() {
            continue;
        }
        // WMNT-wei per 1e18 units of other: w_n * 1e18 / o_n
        let px = w_n
            .checked_mul(U256::from(WAD))
            .and_then(|v| v.checked_div(o_n));
        if let Some(px) = px {
            // Prefer the deeper WMNT side when multiple pairs exist.
            price_x18
                .entry(other)
                .and_modify(|p| {
                    if px > *p {
                        *p = px;
                    }
                })
                .or_insert(px);
        }
    }

    let mut out = HashMap::new();
    for c in candidates {
        let b0 = balances.get(&(c.token0, c.pool));
        let b1 = balances.get(&(c.token1, c.pool));
        // Any failed balanceOf for this pool → quarantine
        match (b0, b1) {
            (Some(None), _) | (_, Some(None)) => {
                out.insert(c.pool, None);
                continue;
            }
            _ => {}
        }
        let b0 = b0.and_then(|o| *o).unwrap_or(U256::ZERO);
        let b1 = b1.and_then(|o| *o).unwrap_or(U256::ZERO);

        if c.token0 == wmnt || c.token1 == wmnt {
            let wmnt_bal = if c.token0 == wmnt { b0 } else { b1 };
            out.insert(c.pool, Some(wmnt_bal.saturating_mul(U256::from(2u64))));
            continue;
        }

        let d0 = *decimals.get(&c.token0).unwrap_or(&18);
        let d1 = *decimals.get(&c.token1).unwrap_or(&18);
        let v0 = price_x18.get(&c.token0).and_then(|px| {
            let n = normalize_to_18(b0, d0);
            n.checked_mul(*px)
                .and_then(|v| v.checked_div(U256::from(WAD)))
        });
        let v1 = price_x18.get(&c.token1).and_then(|px| {
            let n = normalize_to_18(b1, d1);
            n.checked_mul(*px)
                .and_then(|v| v.checked_div(U256::from(WAD)))
        });
        match (v0, v1) {
            (Some(a), Some(b)) => out.insert(c.pool, Some(a.saturating_add(b))),
            (Some(a), None) => out.insert(c.pool, Some(a.saturating_mul(U256::from(2u64)))),
            (None, Some(b)) => out.insert(c.pool, Some(b.saturating_mul(U256::from(2u64)))),
            (None, None) => out.insert(c.pool, None),
        };
    }
    Ok(out)
}

/// Scale a raw token amount to 18-decimal fixed point (truncate if >18).
fn normalize_to_18(amount: U256, decimals: u8) -> U256 {
    if decimals == 18 {
        amount
    } else if decimals < 18 {
        amount.saturating_mul(U256::from(10u64).pow(U256::from((18 - decimals) as u64)))
    } else {
        amount / U256::from(10u64).pow(U256::from((decimals - 18) as u64))
    }
}
