//! Operator CLI for WHI-524 pause control.
//!
//! Signs `init` / `unpause` / `recovery` with `BREAKER_OPERATOR_PRIVATE_KEY`.
//! The runtime never loads this key — commands are written to
//! `{BREAKER_STORE_DIR}/<scope>/control.inbox.json`.

use std::path::PathBuf;
use std::str::FromStr;

use alloy::primitives::Address;
use alloy::signers::local::PrivateKeySigner;
use amms::execution::breaker::{
    sign_command, ControlCommand, ControlKind, ScopeId, SignedOperatorCommand,
};
use clap::{Parser, Subcommand};
use eyre::{eyre, Result};

#[derive(Parser, Debug)]
#[command(name = "pause_control")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    Status {
        #[arg(long, env = "BREAKER_STORE_DIR")]
        store: PathBuf,
        #[arg(long)]
        chain_id: u64,
        #[arg(long)]
        executor: String,
        #[arg(long)]
        signer: String,
    },
    Init {
        #[arg(long, env = "BREAKER_STORE_DIR")]
        store: PathBuf,
        #[arg(long)]
        chain_id: u64,
        #[arg(long)]
        executor: String,
        #[arg(long)]
        signer: String,
        #[arg(long, env = "BREAKER_CONTROL_SEQ", default_value = "1")]
        control_seq: u64,
    },
    Unpause {
        #[arg(long, env = "BREAKER_STORE_DIR")]
        store: PathBuf,
        #[arg(long)]
        chain_id: u64,
        #[arg(long)]
        executor: String,
        #[arg(long)]
        signer: String,
        #[arg(long, env = "BREAKER_CONTROL_SEQ")]
        control_seq: u64,
    },
    Recovery {
        #[arg(long, env = "BREAKER_STORE_DIR")]
        store: PathBuf,
        #[arg(long)]
        chain_id: u64,
        #[arg(long)]
        executor: String,
        #[arg(long)]
        signer: String,
        #[arg(long, env = "BREAKER_CONTROL_SEQ")]
        control_seq: u64,
        #[arg(long, default_value = "manual recovery")]
        detail: String,
    },
}

fn parse_addr(s: &str) -> Result<Address> {
    Address::from_str(s.trim()).map_err(|e| eyre!("address: {e}"))
}

fn scope_dir(store: &PathBuf, chain_id: u64, executor: Address, signer: Address) -> PathBuf {
    store.join(
        ScopeId {
            chain_id,
            executor,
            signer,
        }
        .dir_name(),
    )
}

fn write_inbox(dir: &PathBuf, signed: &SignedOperatorCommand) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join("control.inbox.json");
    let json = serde_json::json!({
        "control_seq": signed.command.control_seq,
        "kind": signed.command.kind as u8,
        "chain_id": signed.command.chain_id,
        "executor": format!("{:?}", signed.command.executor),
        "signer": format!("{:?}", signed.command.signer),
        "detail": signed.command.detail,
        "signature": alloy::hex::encode(&signed.signature),
    });
    std::fs::write(&path, serde_json::to_vec_pretty(&json)?)?;
    println!("wrote {}", path.display());
    Ok(())
}

fn operator_signer() -> Result<PrivateKeySigner> {
    let pk = std::env::var("BREAKER_OPERATOR_PRIVATE_KEY")
        .map_err(|_| eyre!("BREAKER_OPERATOR_PRIVATE_KEY is required"))?;
    PrivateKeySigner::from_str(pk.trim()).map_err(|e| eyre!("operator key: {e}"))
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Status {
            store,
            chain_id,
            executor,
            signer,
        } => {
            let dir = scope_dir(&store, chain_id, parse_addr(&executor)?, parse_addr(&signer)?);
            let pause = dir.join("pause.proj");
            let wal = dir.join("wal.v1");
            println!("store={}", dir.display());
            println!(
                "pause_proj={}",
                if pause.exists() {
                    String::from_utf8_lossy(&std::fs::read(&pause)?).into_owned()
                } else {
                    "<absent>".into()
                }
            );
            println!(
                "wal_bytes={}",
                if wal.exists() {
                    std::fs::metadata(&wal)?.len()
                } else {
                    0
                }
            );
        }
        Cmd::Init {
            store,
            chain_id,
            executor,
            signer,
            control_seq,
        } => {
            let executor = parse_addr(&executor)?;
            let signer_addr = parse_addr(&signer)?;
            let sk = operator_signer()?;
            let signed = sign_command(
                &sk,
                ControlCommand {
                    control_seq,
                    kind: ControlKind::Init,
                    chain_id,
                    executor,
                    signer: signer_addr,
                    detail: "virgin init".into(),
                },
            )
            .map_err(|e| eyre!("{e}"))?;
            write_inbox(&scope_dir(&store, chain_id, executor, signer_addr), &signed)?;
        }
        Cmd::Unpause {
            store,
            chain_id,
            executor,
            signer,
            control_seq,
        } => {
            let executor = parse_addr(&executor)?;
            let signer_addr = parse_addr(&signer)?;
            let sk = operator_signer()?;
            let signed = sign_command(
                &sk,
                ControlCommand {
                    control_seq,
                    kind: ControlKind::Unpause,
                    chain_id,
                    executor,
                    signer: signer_addr,
                    detail: String::new(),
                },
            )
            .map_err(|e| eyre!("{e}"))?;
            write_inbox(&scope_dir(&store, chain_id, executor, signer_addr), &signed)?;
        }
        Cmd::Recovery {
            store,
            chain_id,
            executor,
            signer,
            control_seq,
            detail,
        } => {
            let executor = parse_addr(&executor)?;
            let signer_addr = parse_addr(&signer)?;
            let sk = operator_signer()?;
            let signed = sign_command(
                &sk,
                ControlCommand {
                    control_seq,
                    kind: ControlKind::Recovery,
                    chain_id,
                    executor,
                    signer: signer_addr,
                    detail,
                },
            )
            .map_err(|e| eyre!("{e}"))?;
            write_inbox(&scope_dir(&store, chain_id, executor, signer_addr), &signed)?;
        }
    }
    Ok(())
}
