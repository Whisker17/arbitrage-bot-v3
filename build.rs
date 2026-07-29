use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use serde_json::Value;
use std::{
    fs,
    hash::{DefaultHasher, Hash, Hasher},
    path::{Path, PathBuf},
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

    for git_path in ["HEAD", "packed-refs"] {
        watch_git_path(&manifest_dir, git_path);
    }
    if let Some(symbolic_ref) = git_output(&manifest_dir, &["symbolic-ref", "--quiet", "HEAD"]) {
        watch_git_path(&manifest_dir, symbolic_ref.trim());
    }

    // Needed by the shadow runtime's ledger (`RunMetadata::git_commit`) regardless of
    // whether the forge/ABI regen below runs, so this must happen before the
    // `skip_forge` early return. Falls back to "unknown" rather than failing the build
    // in a shallow-clone/no-git environment (e.g. some CI checkouts).
    let git_commit = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&manifest_dir)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|hash| hash.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=GIT_COMMIT_HASH={git_commit}");

    println!("cargo:rerun-if-env-changed=SKIP_FORGE");
    // Default to skipping forge build unless explicitly requested (set SKIP_FORGE=0)
    let skip_forge = std::env::var("SKIP_FORGE")
        .map(|v| v != "0")
        .unwrap_or(true);

    // solc version comes from contracts/foundry.toml (`solc_version`), which must
    // match toolchain.toml [solidity].version (enforced by scripts/check_toolchain.sh).
    // Do not hardcode a host path or a second solc pin here.
    if !skip_forge {
        let status = Command::new("forge")
            .arg("build")
            .arg("--skip")
            .arg("test")
            .arg("--offline")
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

        // Check if new ABI exists first
        if !new_abi.exists() {
            eprintln!("Warning: {} not found, skipping", new_abi.display());
            return;
        }

        // If previous ABI doesn't exist, copy the new one
        if !prev_abi.exists() {
            if let Err(e) = fs::copy(&new_abi, &prev_abi) {
                eprintln!(
                    "Error copying {} to {}: {}",
                    new_abi.display(),
                    prev_abi.display(),
                    e
                );
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

fn git_output(manifest_dir: &Path, args: &[&str]) -> Option<String> {
    Command::new("git")
        .args(args)
        .current_dir(manifest_dir)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
}

fn watch_git_path(manifest_dir: &Path, git_path: &str) {
    if let Some(output) = git_output(manifest_dir, &["rev-parse", "--git-path", git_path]) {
        let path = PathBuf::from(output.trim());
        let path = if path.is_absolute() {
            path
        } else {
            manifest_dir.join(path)
        };
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

fn hash(value: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}
