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
    // Moe Liquidity Book - removed (contracts not present)
    // "GetMoeLBPairSlot0BatchRequest",
    // "GetMoeLBPairBinDataBatchRequest",
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    println!("cargo:rerun-if-env-changed=SKIP_FORGE");
    // Default to skipping forge build unless explicitly requested (set SKIP_FORGE=0)
    let skip_forge = std::env::var("SKIP_FORGE")
        .map(|v| v != "0")
        .unwrap_or(true);

    if !skip_forge {
        let status = Command::new("forge")
            .arg("build")
            .arg("--skip")
            .arg("test")
            .arg("--offline")
            .arg("--use")
            .arg("/opt/homebrew/bin/solc")
            .current_dir("contracts")
            .status()?;

        if !status.success() {
            panic!("forge build failed");
        }
    } else {
        println!("cargo:warning=Skipping forge build due to SKIP_FORGE env var");
        // When skipping forge build, also skip ABI refresh to avoid reading missing files
        println!("cargo:rerun-if-changed=contracts");
        return Ok(());
    }

    let forge_out_dir = manifest_dir.join("contracts/out");
    let abi_out_dir = manifest_dir.join("src/amms/abi/");
    fs::create_dir_all(&abi_out_dir)?;

    TARGET_CONTRACTS.par_iter().for_each(|contract| {
        let new_abi = forge_out_dir
            .join(format!("{contract}.sol"))
            .join(format!("{contract}.json"));
        let prev_abi = abi_out_dir.join(format!("{contract}.json"));

        if !prev_abi.exists() {
            fs::copy(&new_abi, &prev_abi).unwrap();
            return;
        }

        let prev_contents: Value =
            serde_json::from_str(&fs::read_to_string(&prev_abi).unwrap()).unwrap();
        let new_contents: Value =
            serde_json::from_str(&fs::read_to_string(&new_abi).unwrap()).unwrap();

        let prev_bytecode = prev_contents["bytecode"]["object"]
            .as_str()
            .expect("Missing prev bytecode");
        let new_bytecode = new_contents["bytecode"]["object"]
            .as_str()
            .expect("Missing new bytecode");

        if hash(prev_bytecode) != hash(new_bytecode) {
            fs::copy(&new_abi, &prev_abi).unwrap();
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
