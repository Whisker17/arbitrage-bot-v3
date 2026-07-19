//! Generate and on-chain-validate `data/poolLists_moe.csv` (+ companion `.meta.json`).
//!
//! ```bash
//! # Deterministic regen at the committed snapshot block (default)
//! cargo run --example generate_moe_pool_list
//!
//! # Override recorded end block
//! MOE_POOL_LIST_TO_BLOCK=98000000 cargo run --example generate_moe_pool_list
//!
//! # Explicitly use live chain head (non-deterministic; updates meta snapshot_block)
//! MOE_POOL_LIST_TO_BLOCK=head cargo run --example generate_moe_pool_list
//!
//! # Skip on-chain token/factory re-check after write (not recommended)
//! SKIP_ONCHAIN_VALIDATE=1 cargo run --example generate_moe_pool_list
//! ```

use alloy::{
    eips::BlockId,
    providers::{Provider, ProviderBuilder},
    rpc::client::ClientBuilder,
    transports::layers::{RetryBackoffLayer, ThrottleLayer},
};
use amms::amms::moe::{
    default_moe_pool_list_path, discover_moe_pool_list, CANONICAL_MOE_FACTORY,
    CANONICAL_MOE_FACTORY_CREATION_BLOCK, COMMITTED_MOE_POOL_LIST_SNAPSHOT_BLOCK,
};
use eyre::{bail, Context, Result};
use std::time::Instant;
use tracing::info;

fn rpc_url() -> String {
    std::env::var("MANTLE_HTTP_URL")
        .or_else(|_| std::env::var("MANTLE_PROVIDER_URL"))
        .or_else(|_| std::env::var("RPC_HTTP_URL"))
        .unwrap_or_else(|_| "https://rpc.mantle.xyz".to_string())
}

async fn resolve_to_block(provider: &impl Provider) -> Result<u64> {
    match std::env::var("MOE_POOL_LIST_TO_BLOCK") {
        Ok(raw) if raw.eq_ignore_ascii_case("head") || raw == "latest" => {
            Ok(provider.get_block_number().await.context("get_block_number")?)
        }
        Ok(raw) => raw
            .parse::<u64>()
            .with_context(|| format!("parse MOE_POOL_LIST_TO_BLOCK={raw}")),
        Err(_) => Ok(COMMITTED_MOE_POOL_LIST_SNAPSHOT_BLOCK),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let rpc = rpc_url();
    let out_path = std::env::var("MOE_POOL_LIST_OUT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| default_moe_pool_list_path());

    let client = ClientBuilder::default()
        .layer(ThrottleLayer::new(40))
        .layer(RetryBackoffLayer::new(8, 250, 500))
        .http(rpc.parse().context("parse RPC url")?);
    let provider = ProviderBuilder::new().connect_client(client);

    let to_block = resolve_to_block(&provider).await?;
    if to_block < CANONICAL_MOE_FACTORY_CREATION_BLOCK {
        bail!(
            "to_block {to_block} is before factory creation {}",
            CANONICAL_MOE_FACTORY_CREATION_BLOCK
        );
    }

    info!(
        rpc = %rpc,
        factory = ?CANONICAL_MOE_FACTORY,
        factory_creation = CANONICAL_MOE_FACTORY_CREATION_BLOCK,
        to_block,
        committed_default = COMMITTED_MOE_POOL_LIST_SNAPSHOT_BLOCK,
        out = %out_path.display(),
        "Discovering Moe LB pairs at recorded snapshot block"
    );

    let started = Instant::now();
    let list = discover_moe_pool_list(
        provider.clone(),
        CANONICAL_MOE_FACTORY,
        CANONICAL_MOE_FACTORY_CREATION_BLOCK,
        to_block,
    )
    .await
    .context("discover_moe_pool_list")?;

    info!(
        pools = list.len(),
        snapshot_block = list.snapshot_block(),
        elapsed_secs = started.elapsed().as_secs(),
        "Discovery complete"
    );

    list.write_path(&out_path)
        .with_context(|| format!("write {}", out_path.display()))?;
    info!(path = %out_path.display(), "Wrote Moe pool list + meta");

    let skip_validate = std::env::var("SKIP_ONCHAIN_VALIDATE")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    if !skip_validate {
        info!("Validating on-chain provenance at block {to_block}");
        list.validate_on_chain(
            provider,
            BlockId::Number(to_block.into()),
            CANONICAL_MOE_FACTORY,
        )
        .await
        .context("on-chain validation")?;
        info!("On-chain validation passed");
    }

    let reloaded = amms::amms::moe::MoePoolList::load_path(&out_path).context("reload written list")?;
    assert_eq!(reloaded.len(), list.len());
    assert_eq!(reloaded.entries, list.entries);
    assert_eq!(reloaded.meta, list.meta);
    info!("Reload round-trip OK (deterministic)");

    Ok(())
}
