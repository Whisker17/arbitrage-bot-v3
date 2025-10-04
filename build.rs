use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use serde_json::Value;
use std::{
    fs,
    hash::{DefaultHasher, Hash, Hasher},
    path::PathBuf,
    process::Command,
};

const TARGET_CONTRACTS: &[&str] = &[
    // "GetERC4626VaultDataBatchRequest", // removed
    "GetTokenDecimalsBatchRequest",
    // "GetBalancerPoolDataBatchRequest", // removed
    // "WethValueInPools", // deprecated, replaced by WmntValueInPools
    // "WethValueInPoolsBatchRequest", // deprecated, replaced by WmntValueInPoolsBatchRequest
    "WmntValueInPools",
    "WmntValueInPoolsBatchRequest",
    "GetUniswapV2PairsBatchRequest",
    "GetUniswapV2PoolDataBatchRequest",
    "GetUniswapV3PoolDataBatchRequest",
    "GetUniswapV3PoolSlot0BatchRequest",
    "GetUniswapV3PoolTickBitmapBatchRequest",
    "GetUniswapV3PoolTickDataBatchRequest",
    // Agni (Uniswap v3 compatible)
    "GetAgniPoolSlot0BatchRequest",
    "GetAgniPoolTickBitmapBatchRequest",
    "GetAgniPoolTickDataBatchRequest",
    // Moe Liquidity Book
    "GetMoeLBPairSlot0BatchRequest",
    "GetMoeLBPairBinDataBatchRequest",
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));

    let status = Command::new("forge")
        .arg("build")
        .arg("--skip")
        .arg("test")
        .arg("--force")
        .current_dir("contracts")
        .status()?;

    if !status.success() {
        panic!("forge build failed");
    }

    let forge_out_dir = manifest_dir.join("contracts/out");
    let abi_out_dir = manifest_dir.join("src/amms/abi/");
    fs::create_dir_all(&abi_out_dir)?;

    TARGET_CONTRACTS.par_iter().for_each(|contract| {
        let new_abi = forge_out_dir
            .join(format!("{contract}.sol"))
            .join(format!("{contract}.json"));
        let prev_abi = abi_out_dir.join(format!("{contract}.json"));

        // Check if new ABI exists first
        if !new_abi.exists() {
            eprintln!("Warning: {} not found, skipping", new_abi.display());
            return;
        }

        // If previous ABI doesn't exist, copy the new one
        if !prev_abi.exists() {
            if let Err(e) = fs::copy(&new_abi, &prev_abi) {
                eprintln!("Error copying {} to {}: {}", new_abi.display(), prev_abi.display(), e);
            }
            return;
        }

        // Read and compare both files
        let prev_contents = match fs::read_to_string(&prev_abi) {
            Ok(content) => match serde_json::from_str::<Value>(&content) {
                Ok(json) => json,
                Err(e) => {
                    eprintln!("Error parsing {}: {}", prev_abi.display(), e);
                    return;
                }
            },
            Err(e) => {
                eprintln!("Error reading {}: {}", prev_abi.display(), e);
                return;
            }
        };

        let new_contents = match fs::read_to_string(&new_abi) {
            Ok(content) => match serde_json::from_str::<Value>(&content) {
                Ok(json) => json,
                Err(e) => {
                    eprintln!("Error parsing {}: {}", new_abi.display(), e);
                    return;
                }
            },
            Err(e) => {
                eprintln!("Error reading {}: {}", new_abi.display(), e);
                return;
            }
        };

        let prev_bytecode = match prev_contents["bytecode"]["object"].as_str() {
            Some(bc) => bc,
            None => {
                eprintln!("Missing bytecode in {}", prev_abi.display());
                return;
            }
        };

        let new_bytecode = match new_contents["bytecode"]["object"].as_str() {
            Some(bc) => bc,
            None => {
                eprintln!("Missing bytecode in {}", new_abi.display());
                return;
            }
        };

        if hash(prev_bytecode) != hash(new_bytecode) {
            if let Err(e) = fs::copy(&new_abi, &prev_abi) {
                eprintln!("Error updating {}: {}", prev_abi.display(), e);
            }
        }
    });

    println!("cargo:rerun-if-changed=contracts");

    Ok(())
}

fn hash(value: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}
