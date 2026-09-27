//! WHI-1527: one-off capture of the pinned replay corpus (run at one commit only).
//!
//! Re-enacts the live watch loop's *state* handling for an explicit, recorded
//! head schedule (built from the rc2 ledger and its `block_summary` log), with
//! archive reads pinned to block hashes:
//!
//! * bootstrap: every pool batch-initialised at the bootstrap block hash, then
//!   Moe snapshots at that hash (the same loaders `StateSpaceBuilder::sync` uses);
//! * `processed` head: hash-pinned head logs → `StateSpace::sync`, small-gap
//!   dirty widening from the gap blocks' logs (addresses only, never applied),
//!   `refresh_selected_tip_state(scope)`, write-back, then one corpus pass;
//! * `pinned_header_unavailable` (inner: header loaded but base fee / gas
//!   limit unusable, inside `process_observed_head`): same state work as
//!   `processed`, no pass;
//! * `header_timeout` (outer `CanonicalHeaderLoad::TimedOut`: HTTP never served
//!   the announced hash, so `process_observed_head` never runs): no state
//!   change and the published tip does not move;
//! * `processing_failed`: `StateSpace::sync` is expected to fail as live did;
//! * `duplicate` / `pinned_logs_unavailable`: no state change;
//! * `rebaseline`: continuity tip moves, no logs are applied (live WHI-792).
//!
//! Every header in the window is fetched and parent-linked; ledger hashes are
//! checked against the chain. Each pass records only the pools whose state
//! changed since the previous pass. Nothing here signs or sends.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::Instant;

use alloy::eips::BlockNumberOrTag;
use alloy::network::BlockResponse;
use alloy::primitives::{Address, B256};
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::Log;
use amms::amms::agni::AgniFactory;
use amms::amms::amm::{AutomatedMarketMaker, Variant, AMM};
use amms::amms::batch_create::batch_create_call_count;
use amms::amms::moe::{sync_moe_snapshots_batch, MoeFactory, MoeSnapshotContext, MoeSnapshotSyncConfig};
use amms::amms::uniswap_v2::UniswapV2Factory;
use amms::amms::uniswap_v3::UniswapV3Factory;
use amms::service::block_loop::refresh_selected_tip_state;
use amms::service::pool_universe::PoolUniverseSource;
use amms::service::protocol::{
    plan_moe_tip_refresh, AgniV2Protocol, AgniV3Protocol, MoeProtocol, Protocol, TipRefreshScope,
};
use amms::service::rpc_provider::{connect_http_provider, RpcProviderConfig};
use amms::service::select::SelectedProtocol;
use amms::service::unified_universe::UnifiedPoolUniverseSource;
use amms::state_space::{
    build_block_filter, hash_pinned_logs_filter, hash_pinned_state_block_id, BlockHeaderContext,
    PoolProtocol, StateSpace,
};
use clap::Parser;
use eyre::{bail, eyre, Context, Result};
use futures::{stream, StreamExt};
use serde_json::{json, Value};

const WMNT: &str = "0x78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8";

#[derive(Parser, Debug)]
struct Args {
    /// Public archive RPC. Recorded in the manifest as a host fingerprint only.
    #[arg(long, env = "REPLAY_RPC_URL", default_value = "https://rpc.mantle.xyz")]
    rpc_url: String,
    #[arg(long, default_value_t = 8)]
    throttle_rps: u32,
    #[arg(long, default_value_t = 8)]
    concurrency: usize,
    #[arg(long, default_value_t = 120)]
    request_timeout_s: u64,
    #[arg(long, default_value = "data/pool_universe.csv")]
    universe: PathBuf,
    /// Schedule JSON (`scripts/replay_baseline.sh schedule`).
    #[arg(long)]
    schedule: PathBuf,
    /// Output directory (corpus.jsonl, emulation.jsonl, capture.json, rpc cache).
    #[arg(long)]
    out: PathBuf,
    /// Pilot: stop after this many schedule events.
    #[arg(long)]
    max_events: Option<usize>,
    /// DiscoveryConfig values written into the corpus meta (rc2 values).
    #[arg(long)]
    max_input_wei: String,
    #[arg(long)]
    min_profit_wei: String,
    #[arg(long)]
    priority_fee_wei: String,
    #[arg(long)]
    block_gas_reserve: u64,
    #[arg(long, default_value_t = 3)]
    max_hops: u64,
    #[arg(long)]
    profile_digest: String,
    /// Provenance strings copied into the meta line.
    #[arg(long, default_value = "")]
    capture_commit: String,
}

#[derive(Clone)]
struct Header {
    number: u64,
    hash: B256,
    parent: B256,
    timestamp: u64,
    base_fee: u128,
    gas_limit: u64,
}

fn header_json(h: &Header) -> Value {
    json!({"number": h.number, "hash": format!("{:#x}", h.hash), "parent": format!("{:#x}", h.parent),
           "timestamp": h.timestamp, "base_fee": h.base_fee.to_string(), "gas_limit": h.gas_limit})
}

fn header_from_json(v: &Value) -> Result<Header> {
    Ok(Header {
        number: v["number"].as_u64().ok_or_else(|| eyre!("number"))?,
        hash: v["hash"].as_str().ok_or_else(|| eyre!("hash"))?.parse()?,
        parent: v["parent"].as_str().ok_or_else(|| eyre!("parent"))?.parse()?,
        timestamp: v["timestamp"].as_u64().ok_or_else(|| eyre!("ts"))?,
        base_fee: v["base_fee"].as_str().ok_or_else(|| eyre!("bf"))?.parse()?,
        gas_limit: v["gas_limit"].as_u64().ok_or_else(|| eyre!("gl"))?,
    })
}

/// Line-per-record JSONL cache so a pilot or a retry never re-reads the chain.
fn read_cache(path: &PathBuf) -> Result<Vec<Value>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for line in BufReader::new(std::fs::File::open(path)?).lines() {
        let line = line?;
        if !line.trim().is_empty() {
            out.push(serde_json::from_str(&line)?);
        }
    }
    Ok(out)
}

async fn fetch_headers(
    provider: &DynProvider,
    numbers: Vec<u64>,
    concurrency: usize,
) -> Result<Vec<Header>> {
    let results: Vec<Result<Header>> = stream::iter(numbers)
        .map(|n| async move {
            for attempt in 0..8u32 {
                match provider.get_block_by_number(BlockNumberOrTag::Number(n)).await {
                    Ok(Some(b)) => {
                        let h = b.header();
                        return Ok(Header {
                            number: h.number,
                            hash: h.hash,
                            parent: h.parent_hash,
                            timestamp: h.timestamp,
                            base_fee: u128::from(
                                h.base_fee_per_gas.ok_or_else(|| eyre!("block {n}: no base fee"))?,
                            ),
                            gas_limit: h.gas_limit,
                        });
                    }
                    Ok(None) | Err(_) if attempt < 7 => {
                        tokio::time::sleep(std::time::Duration::from_millis(500 * (attempt as u64 + 1))).await;
                    }
                    Ok(None) => bail!("block {n} not found"),
                    Err(e) => return Err(e.into()),
                }
            }
            unreachable!()
        })
        .buffered(concurrency)
        .collect()
        .await;
    results.into_iter().collect()
}

async fn fetch_logs(
    provider: &DynProvider,
    filter: &alloy::rpc::types::Filter,
    blocks: Vec<(u64, B256)>,
    concurrency: usize,
) -> Result<Vec<(u64, Vec<Log>)>> {
    let results: Vec<Result<(u64, Vec<Log>)>> = stream::iter(blocks)
        .map(|(n, hash)| async move {
            let f = hash_pinned_logs_filter(filter.clone(), hash);
            let mut last = None;
            for attempt in 0..8u32 {
                match provider.get_logs(&f).await {
                    Ok(logs) => {
                        for l in &logs {
                            if l.block_hash != Some(hash) || l.block_number != Some(n) {
                                bail!("log for block {n} carries a different block identity");
                            }
                        }
                        return Ok((n, logs));
                    }
                    Err(e) => {
                        last = Some(e);
                        tokio::time::sleep(std::time::Duration::from_millis(500 * (attempt as u64 + 1))).await;
                    }
                }
            }
            Err(eyre!("get_logs {n} {hash:#x}: {:?}", last))
        })
        .buffered(concurrency)
        .collect()
        .await;
    results.into_iter().collect()
}

fn addr_list<'a>(it: impl IntoIterator<Item = &'a Address>) -> Vec<String> {
    let set: BTreeSet<String> = it.into_iter().map(|a| format!("{a:#x}")).collect();
    set.into_iter().collect()
}

/// Canonical, exact JSON text: object keys sorted, numbers kept verbatim.
///
/// `serde_json::Value` cannot hold the pools' `u128`/`i128` fields, so numeric
/// tokens are shielded as strings while sorting and restored afterwards. Used
/// for change detection and the round-trip proof; the output still
/// deserializes into `AMM`.
fn canonical(text: &str, sort_numeric_arrays: bool) -> Result<String> {
    const TAG: &str = "~#num:";
    let mut shielded = String::with_capacity(text.len() + 64);
    let (mut in_str, mut esc) = (false, false);
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if in_str {
            shielded.push(c);
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
        } else if c == '"' {
            in_str = true;
            shielded.push(c);
        } else if c == '-' || c.is_ascii_digit() {
            shielded.push('"');
            shielded.push_str(TAG);
            shielded.push(c);
            while let Some(&d) = chars.peek() {
                if d.is_ascii_digit() || matches!(d, '.' | 'e' | 'E' | '+' | '-') {
                    shielded.push(d);
                    chars.next();
                } else {
                    break;
                }
            }
            shielded.push('"');
        } else {
            shielded.push(c);
        }
    }
    fn sort_sets(v: &mut Value) {
        match v {
            Value::Array(items) => {
                items.iter_mut().for_each(sort_sets);
                if items.iter().all(|x| x.as_str().is_some_and(|s| s.starts_with(TAG))) {
                    items.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
                }
            }
            Value::Object(map) => map.values_mut().for_each(sort_sets),
            _ => {}
        }
    }
    let mut value: Value = serde_json::from_str(&shielded)?;
    if sort_numeric_arrays {
        sort_sets(&mut value);
    }
    let sorted = serde_json::to_string(&value)?;
    let open = format!("\"{TAG}");
    let mut out = String::with_capacity(sorted.len());
    let mut rest = sorted.as_str();
    while let Some(i) = rest.find(&open) {
        out.push_str(&rest[..i]);
        let after = &rest[i + open.len()..];
        let end = after.find('"').ok_or_else(|| eyre!("canonical: unterminated number"))?;
        out.push_str(&after[..end]);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Sorted numeric arrays: pools serialize `HashSet` fields (V3/Agni
/// `tick_bitmap_coverage`, the only all-numeric arrays in the pool encoding)
/// as arrays in hash order. Sorting them is lossless for a set, and the
/// canonical text still deserializes into `AMM`.
fn amm_canonical(a: &AMM) -> Result<String> {
    canonical(&serde_json::to_string(a)?, true)
}

#[derive(serde::Deserialize)]
struct BootCache {
    hash: String,
    pools: Vec<AMM>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).with_writer(std::io::stderr).init();
    let args = Args::parse();
    std::fs::create_dir_all(&args.out)?;
    let t_all = Instant::now();

    let schedule: Value = serde_json::from_reader(std::fs::File::open(&args.schedule)?)?;
    let bootstrap_block = schedule["bootstrap_block"].as_u64().ok_or_else(|| eyre!("bootstrap_block"))?;
    let mut events: Vec<Value> = schedule["events"].as_array().ok_or_else(|| eyre!("events"))?.clone();
    if let Some(k) = args.max_events {
        events.truncate(k);
    }
    let last_block = events.iter().filter_map(|e| e["block"].as_u64()).max().unwrap_or(bootstrap_block);

    let provider = connect_http_provider(
        &args.rpc_url,
        &RpcProviderConfig {
            throttle_rps: args.throttle_rps,
            request_timeout: std::time::Duration::from_secs(args.request_timeout_s),
            ..RpcProviderConfig::default()
        },
    )?;
    let chain_id = provider.get_chain_id().await?;
    if chain_id != 5000 {
        bail!("chain id {chain_id} != 5000");
    }
    let mut rpc_headers = 0u64;
    let mut rpc_logs = 0u64;
    let creates_before = batch_create_call_count();

    // --- Universe shells, exactly as the live bot builds them. ---
    let settlement: Address = WMNT.parse()?;
    let selected = vec![SelectedProtocol::AgniV2, SelectedProtocol::AgniV3, SelectedProtocol::Moe];
    let loaded = UnifiedPoolUniverseSource::new(&args.universe)
        .with_protocol_filter(selected.clone())
        .load(chain_id, settlement)
        .await
        .map_err(|e| eyre!("{e}"))?;
    let (v2, v3, moe) = (AgniV2Protocol::new(Address::ZERO), AgniV3Protocol::new(Address::ZERO), MoeProtocol::new());
    let mut shells = Vec::new();
    for row in &loaded.rows {
        let built = match row.protocol {
            PoolProtocol::UniswapV2 => v2.build_amm(row),
            PoolProtocol::Agni => v3.build_amm(row),
            PoolProtocol::MoeLb => moe.build_amm(row),
            other => bail!("unexpected protocol {other:?}"),
        };
        shells.push(built.map_err(|e| eyre!("build_amm: {e}"))?);
    }

    // --- Header chain bootstrap..=last, cached, parent-linked. ---
    let t_headers = Instant::now();
    let hcache = args.out.join("rpc_headers.jsonl");
    let mut headers: BTreeMap<u64, Header> = BTreeMap::new();
    for v in read_cache(&hcache)? {
        let h = header_from_json(&v)?;
        headers.insert(h.number, h);
    }
    let missing: Vec<u64> = (bootstrap_block..=last_block).filter(|n| !headers.contains_key(n)).collect();
    if !missing.is_empty() {
        rpc_headers += missing.len() as u64;
        let fetched = fetch_headers(&provider, missing, args.concurrency).await?;
        let mut w = std::fs::OpenOptions::new().create(true).append(true).open(&hcache)?;
        for h in fetched {
            writeln!(w, "{}", header_json(&h))?;
            headers.insert(h.number, h);
        }
    }
    for n in bootstrap_block + 1..=last_block {
        if headers[&n].parent != headers[&(n - 1)].hash {
            bail!("parent linkage broken at {n}");
        }
    }
    let headers_s = t_headers.elapsed().as_secs_f64();
    eprintln!("headers done in {headers_s:.1}s");
    // Ledger identities must be the canonical chain.
    let mut ledger_hash_checked = 0u64;
    for e in &events {
        if let (Some(n), Some(h)) = (e["block"].as_u64(), e["ledger_hash"].as_str()) {
            let h: B256 = h.parse()?;
            if headers[&n].hash != h {
                bail!("ledger hash for {n} is not canonical: {h:#x} vs {:#x}", headers[&n].hash);
            }
            ledger_hash_checked += 1;
        }
    }

    // --- Hash-pinned logs for every block after bootstrap, cached. ---
    let t_logs = Instant::now();
    let filter = build_block_filter(&[], &shells).map_err(|e| eyre!("{e}"))?;
    let lcache = args.out.join("rpc_logs.jsonl");
    let mut logs_by_block: HashMap<u64, Vec<Log>> = HashMap::new();
    for v in read_cache(&lcache)? {
        let n = v["block"].as_u64().ok_or_else(|| eyre!("cache block"))?;
        let hash: B256 = v["hash"].as_str().ok_or_else(|| eyre!("cache hash"))?.parse()?;
        if headers.get(&n).map(|h| h.hash) == Some(hash) {
            logs_by_block.insert(n, serde_json::from_value(v["logs"].clone())?);
        }
    }
    let need: Vec<(u64, B256)> = (bootstrap_block + 1..=last_block)
        .filter(|n| !logs_by_block.contains_key(n))
        .map(|n| (n, headers[&n].hash))
        .collect();
    if !need.is_empty() {
        rpc_logs += need.len() as u64;
        let fetched = fetch_logs(&provider, &filter, need, args.concurrency).await?;
        let mut w = std::fs::OpenOptions::new().create(true).append(true).open(&lcache)?;
        for (n, logs) in fetched {
            writeln!(w, "{}", json!({"block": n, "hash": format!("{:#x}", headers[&n].hash), "logs": logs}))?;
            logs_by_block.insert(n, logs);
        }
    }
    let logs_s = t_logs.elapsed().as_secs_f64();
    eprintln!("logs done in {logs_s:.1}s");

    // --- Bootstrap state at the pinned bootstrap hash. ---
    let t_boot = Instant::now();
    let boot = headers[&bootstrap_block].clone();
    let bcache = args.out.join("rpc_bootstrap_state.json");
    let mut pools: Vec<AMM> = Vec::new();
    if bcache.exists() {
        let c: BootCache = serde_json::from_str(&std::fs::read_to_string(&bcache)?)?;
        if c.hash == format!("{:#x}", boot.hash) {
            pools = c.pools;
        }
    }
    if pools.is_empty() {
        let block_id = hash_pinned_state_block_id(boot.hash);
        let mut by_variant: HashMap<Variant, Vec<AMM>> = HashMap::new();
        for a in shells {
            by_variant.entry(a.variant()).or_default().push(a);
        }
        for (variant, group) in by_variant {
            let synced = match variant {
                Variant::AgniPool => AgniFactory::batch_init_pools(group, block_id, provider.clone()).await?,
                Variant::UniswapV3Pool => UniswapV3Factory::batch_init_pools(group, block_id, provider.clone()).await?,
                Variant::UniswapV2Pool => UniswapV2Factory::batch_init_pools(group, block_id, provider.clone()).await?,
                Variant::MoeLbPair => MoeFactory::batch_init_pools(group, block_id, provider.clone()).await?,
            };
            pools.extend(synced);
        }
        sync_moe_snapshots_batch(
            &mut pools,
            block_id,
            provider.clone(),
            MoeSnapshotContext::new(boot.hash, boot.timestamp),
            MoeSnapshotSyncConfig::default(),
        )
        .await?;
        std::fs::write(
            &bcache,
            format!("{{\"hash\":\"{:#x}\",\"pools\":{}}}", boot.hash, serde_json::to_string(&pools)?),
        )?;
    }
    let mut space = StateSpace::default();
    for a in pools {
        space.state.insert(a.address(), a);
    }
    space.latest_block.store(bootstrap_block, Ordering::Relaxed);
    let boot_s = t_boot.elapsed().as_secs_f64();
    eprintln!("bootstrap state done in {boot_s:.1}s");
    if space.state.len() != loaded.rows.len() {
        bail!("bootstrap produced {} pools, universe has {}", space.state.len(), loaded.rows.len());
    }

    // --- Serde round-trip proof on the bootstrap state. ---
    let mut roundtrip_ok = 0usize;
    let mut roundtrip_bad: Vec<String> = Vec::new();
    for a in space.state.values() {
        let v = amm_canonical(a)?;
        let back: AMM = serde_json::from_str(&v)?;
        let again = amm_canonical(&back)?;
        if again != v || back.address() != a.address() {
            std::fs::write(
                args.out.join(format!("roundtrip_mismatch_{:#x}.json", a.address())),
                format!("{v}\n{again}\n"),
            )?;
            roundtrip_bad.push(format!("{:#x}", a.address()));
            continue;
        }
        roundtrip_ok += 1;
    }

    // --- Emulate the recorded schedule. ---
    let t_emu = Instant::now();
    let mut corpus = BufWriter::new(std::fs::File::create(args.out.join("corpus.jsonl"))?);
    let mut emu = BufWriter::new(std::fs::File::create(args.out.join("emulation.jsonl"))?);
    let meta = json!({
        "kind": "meta",
        "schema": "whi-1527/replay-corpus/v1",
        "chain_id": chain_id,
        "capture_commit": args.capture_commit,
        "universe_fingerprint": format!("{:#x}", loaded.fingerprint),
        "universe_pool_count": loaded.rows.len(),
        "profile_digest": args.profile_digest,
        "bootstrap": header_json(&boot),
        "discovery_config": {
            "settlement_asset": WMNT, "max_hops": args.max_hops,
            "max_input_wei": args.max_input_wei, "min_profit_wei": args.min_profit_wei,
            "priority_fee_wei": args.priority_fee_wei, "block_gas_reserve": args.block_gas_reserve,
        },
    });
    writeln!(corpus, "{meta}")?;

    let mut emitted: HashMap<Address, String> = HashMap::new();
    let mut touched_since_pass: HashSet<Address> = space.state.keys().copied().collect();
    let mut last_published = bootstrap_block;
    let mut pass_idx = 0u64;
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    let mut ledger_dirty_mismatch = Vec::new();
    let mut expected_failures_reproduced = 0u64;
    let mut expected_failures_not_reproduced = Vec::new();
    let mut unexpected_sync_errors = Vec::new();
    let mut updated_total = 0u64;
    let mut max_updated_bytes = 0usize;

    for e in &events {
        let n = e["block"].as_u64().ok_or_else(|| eyre!("event block"))?;
        let kind = e["kind"].as_str().ok_or_else(|| eyre!("event kind"))?;
        *counts.entry(kind.to_string()).or_default() += 1;
        let h = headers[&n].clone();
        match kind {
            "rebaseline" => {
                last_published = n;
                writeln!(emu, "{}", json!({"block": n, "kind": kind}))?;
            }
            "startup_row" | "duplicate" | "pinned_logs_unavailable" | "header_timeout" => {
                writeln!(emu, "{}", json!({"block": n, "kind": kind}))?;
            }
            "processing_failed" => {
                let logs = &logs_by_block[&n];
                for l in logs {
                    if space.state.contains_key(&l.address()) {
                        touched_since_pass.insert(l.address());
                    }
                }
                let res = if logs.is_empty() { Ok(vec![]) } else { space.sync(logs) };
                let err = res.as_ref().err().map(|e| e.to_string());
                if err.is_some() {
                    expected_failures_reproduced += 1;
                } else {
                    // Live failed here; we did not. State stays as applied; report it.
                    expected_failures_not_reproduced.push(n);
                    space.latest_block.store(n, Ordering::Relaxed);
                }
                writeln!(emu, "{}", json!({"block": n, "kind": kind, "sync_error": err}))?;
            }
            "processed" | "pinned_header_unavailable" => {
                let logs = &logs_by_block[&n];
                let affected: Vec<Address> = if logs.is_empty() {
                    Vec::new()
                } else {
                    match space.sync(logs) {
                        Ok(a) => a,
                        Err(err) => {
                            unexpected_sync_errors.push(json!({"block": n, "error": err.to_string()}));
                            for l in logs {
                                if space.state.contains_key(&l.address()) {
                                    touched_since_pass.insert(l.address());
                                }
                            }
                            writeln!(emu, "{}", json!({"block": n, "kind": kind, "unexpected_sync_error": err.to_string()}))?;
                            continue;
                        }
                    }
                };
                space.latest_block.store(n, Ordering::Relaxed);
                touched_since_pass.extend(affected.iter().copied());

                let scope_label = if kind == "processed" {
                    e["ledger_scope"].as_str().ok_or_else(|| eyre!("ledger_scope"))?
                } else {
                    "touched"
                };
                // Small gap: dirty widened by the gap blocks' log addresses (not applied).
                let mut dirty: Vec<Address> = affected.clone();
                let mut gap_range = Value::Null;
                if n > last_published + 1 {
                    gap_range = json!([last_published + 1, n]);
                    let mut seen: HashSet<Address> = dirty.iter().copied().collect();
                    for b in last_published + 1..=n {
                        for l in &logs_by_block[&b] {
                            if seen.insert(l.address()) {
                                dirty.push(l.address());
                            }
                        }
                    }
                }
                let scope = if scope_label == "full" {
                    TipRefreshScope::Full
                } else {
                    TipRefreshScope::Touched(dirty.iter().copied().collect())
                };

                let mut pool_vec: Vec<AMM> = space.state.values().cloned().collect();
                let moe_plan = plan_moe_tip_refresh(&pool_vec, &scope);
                let header_ctx = BlockHeaderContext::new(h.parent, h.timestamp);
                refresh_selected_tip_state(&provider, &mut pool_vec, &selected, h.hash, &header_ctx, &scope)
                    .await
                    .with_context(|| format!("tip refresh at {n}"))?;
                for a in pool_vec {
                    space.state.insert(a.address(), a);
                }
                touched_since_pass.extend(moe_plan.to_refresh.iter().copied());
                last_published = n;

                let ledger_dirty_ok = if kind == "processed" {
                    let ledger: BTreeSet<String> = e["ledger_dirty"]
                        .as_array()
                        .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_lowercase())).collect())
                        .unwrap_or_default();
                    let ours: BTreeSet<String> = addr_list(&affected).into_iter().collect();
                    if ledger != ours {
                        ledger_dirty_mismatch.push(json!({"block": n, "ledger": ledger, "replay": ours}));
                    }
                    Some(ledger == ours)
                } else {
                    None
                };
                writeln!(emu, "{}", json!({
                    "block": n, "kind": kind, "scope": scope_label, "affected": addr_list(&affected),
                    "gap_range": gap_range, "dirty_for_tip": dirty.len(),
                    "moe_refreshed": moe_plan.to_refresh.len(), "ledger_dirty_match": ledger_dirty_ok,
                }))?;

                if kind == "processed" {
                    let mut updated = Vec::new();
                    let mut cand: Vec<Address> = touched_since_pass.drain().collect();
                    cand.sort();
                    for a in cand {
                        // Conservative: every pool touched since the last pass is re-emitted,
                        // in canonical form (sorted keys and sets) so corpus bytes are
                        // reproducible across captures.
                        let c = amm_canonical(&space.state[&a])?;
                        emitted.insert(a, c.clone());
                        updated.push(c);
                    }
                    updated_total += updated.len() as u64;
                    let head = json!({
                        "kind": "pass", "pass": pass_idx, "block": n, "hash": format!("{:#x}", h.hash),
                        "parent": format!("{:#x}", h.parent), "timestamp": h.timestamp,
                        "base_fee": h.base_fee.to_string(), "gas_limit": h.gas_limit,
                        "scope": scope_label, "dirty": addr_list(&dirty),
                    })
                    .to_string();
                    let s = format!("{},\"updated\":[{}]}}", &head[..head.len() - 1], updated.join(","));
                    max_updated_bytes = max_updated_bytes.max(s.len());
                    writeln!(corpus, "{s}")?;
                    pass_idx += 1;
                }
            }
            other => bail!("unknown event kind {other}"),
        }
    }
    corpus.flush()?;
    emu.flush()?;
    let emu_s = t_emu.elapsed().as_secs_f64();

    // Final consistency: replaying `updated` reproduces the live-emulated state
    // for every pool that was ever emitted.
    let mut final_mismatch = 0usize;
    for (a, v) in &emitted {
        if &amm_canonical(&space.state[a])? != v && !touched_since_pass.contains(a) {
            final_mismatch += 1;
        }
    }

    let report = json!({
        "bootstrap_block": bootstrap_block, "last_block": last_block,
        "events": events.len(), "event_counts": counts, "passes": pass_idx,
        "ledger_hashes_checked": ledger_hash_checked,
        "parent_linkage_checked_blocks": last_block - bootstrap_block,
        "serde_roundtrip_pools_ok": roundtrip_ok, "serde_roundtrip_mismatch": roundtrip_bad,
        "ledger_dirty_mismatches": ledger_dirty_mismatch,
        "expected_processing_failures_reproduced": expected_failures_reproduced,
        "expected_processing_failures_not_reproduced": expected_failures_not_reproduced,
        "unexpected_sync_errors": unexpected_sync_errors,
        "updated_pool_records": updated_total, "max_pass_line_bytes": max_updated_bytes,
        "final_state_mismatch": final_mismatch,
        "rpc": {"eth_getBlockByNumber": rpc_headers, "eth_getLogs_by_hash": rpc_logs,
                "batch_create_eth_calls": batch_create_call_count() - creates_before,
                "throttle_rps": args.throttle_rps, "concurrency": args.concurrency},
        "wall_s": {"headers": headers_s, "logs": logs_s, "bootstrap_state": boot_s,
                   "emulation_incl_moe_refresh_rpc": emu_s, "total": t_all.elapsed().as_secs_f64()},
    });
    std::fs::write(args.out.join("capture.json"), serde_json::to_string_pretty(&report)?)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
