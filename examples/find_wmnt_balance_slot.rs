//! WHI-557: reproducible tool for empirically discovering an ERC20 token's `balanceOf`
//! mapping storage slot on live Mantle mainnet.
//!
//! Formalizes the ad-hoc `cast call --override-state-diff` brute-force technique used
//! to originally derive [`amms::execution::mainnet_fork_harness::WMNT_BALANCE_SLOT`]
//! (see that constant's doc comment). For each candidate base slot, this writes a
//! magic value at `keccak256(pad32(holder) ++ pad32(candidate))` via a `stateOverride`
//! and calls the token's real `balanceOf(holder)`; the candidate whose override is
//! reflected back exactly is the mapping's storage slot. Read-only (`eth_call`), no
//! broadcast transaction, safe to re-run at any time against any ERC20 token.
//!
//! Usage:
//! ```text
//! cargo run --example find_wmnt_balance_slot -- \
//!   --token 0x78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8
//! ```

use alloy::primitives::{Address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use amms::execution::mainnet_fork_harness::{build_state_override, erc20_balance_override, AccountStateOverride};
use amms::execution::IWMNT;
use clap::Parser;
use eyre::{eyre, Result};
use std::process::ExitCode;

#[derive(Debug, Parser)]
#[command(about = "Empirically discover an ERC20 token's balanceOf mapping storage slot")]
struct Args {
    /// ERC20 token contract to probe (must implement `balanceOf(address)`).
    #[arg(long)]
    token: Address,
    /// Any address to use as the probed holder — need not hold a real balance, since
    /// the probe overrides the token's storage directly rather than relying on chain
    /// state.
    #[arg(long, default_value = "0x0000000000000000000000000000000000dEaD")]
    holder: Address,
    /// Highest candidate base slot to try (inclusive), starting from 0.
    #[arg(long, default_value_t = 20)]
    max_slot: u64,
    /// Mantle JSON-RPC HTTP endpoint. Falls back to the public endpoint.
    #[arg(long, env = "MANTLE_FORK_RPC_URL", default_value = "https://rpc.mantle.xyz")]
    rpc_url: String,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let args = Args::parse();
    let provider = ProviderBuilder::new().connect_http(args.rpc_url.parse()?);
    let block_number = provider.get_block_number().await?;
    let magic = U256::from(0xdeadbeefu64) << 128 | U256::from(0x1234u64);

    let token = IWMNT::new(args.token, &provider);

    for candidate in 0..=args.max_slot {
        let (slot, value) = erc20_balance_override(args.holder, magic, candidate);
        let overrides = build_state_override(vec![AccountStateOverride {
            address: args.token,
            code: None,
            balance: None,
            state_diff: vec![(slot, value)],
        }]);

        let observed = token
            .balanceOf(args.holder)
            .state(overrides)
            .block(block_number.into())
            .call()
            .await?;

        if observed == magic {
            println!(
                "found: token={} holder={} block={} balance_slot={}",
                args.token, args.holder, block_number, candidate
            );
            return Ok(());
        }
    }

    Err(eyre!(
        "no candidate slot in 0..={} reproduced the magic value for token {} at block {}",
        args.max_slot,
        args.token,
        block_number
    ))
}
