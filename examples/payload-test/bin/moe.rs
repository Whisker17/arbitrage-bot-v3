use clap::Parser;
use eyre::Result;
#[path = "../lib.rs"]
mod payload_test;

#[derive(Parser, Debug)]
#[command(name = "moe", version, about = "Fetch Moe LB slot0 data via eth_call")] 
struct Cli {
    /// Path to CSV with Moe pairs (needs header 'Pair Address')
    #[arg(long)]
    csv: String,
    /// Mantle RPC endpoint
    #[arg(long, default_value = "https://rpc.mantle.xyz")] 
    rpc: String,
    /// Block tag (e.g. latest or 0x...)
    #[arg(long)]
    block: Option<String>,
    /// Output CSV path under output/
    #[arg(long)]
    out: String,
}

fn main() -> Result<()> {
    let args = Cli::parse();
    std::fs::create_dir_all("output")?;
    let out_path = if args.out.starts_with("output/") { args.out } else { format!("output/{}", args.out) };
    payload_test::moe_call_and_write_csv(&args.csv, &args.rpc, args.block.as_deref(), &out_path)
}

