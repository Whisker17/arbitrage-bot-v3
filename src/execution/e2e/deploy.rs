//! Contract-deployment + fixture-setup transaction building for the WHI-525
//! Mantle Sepolia E2E harness (M4).
//!
//! Pure, offline helpers: read a forge full-artifact JSON's creation
//! `bytecode.object`, ABI-encode a contract's constructor arguments the same
//! way Solidity's own `abi.encode(args...)` would for a top-level argument
//! list (`SolValue::abi_encode_params`, never plain `.abi_encode()` — the
//! latter wraps the tuple as a single dynamic element and would prepend an
//! extra offset word no real constructor call ever has), and concatenate the
//! two into ready-to-broadcast contract-creation `data`. No RPC calls, no
//! signing, no broadcasting — those stay in
//! [`crate::execution::e2e::capability`] and the M4 bootstrap example itself.
//!
//! Bytecode is read from the full forge artifact (`{Contract}.full.json`) so
//! this module works identically for the three fixture contracts (exported
//! by `contracts/scripts/export_fixture_artifacts.sh` into
//! `contracts/fixtures/artifacts/`) and `ArbitrageExecutor` (already
//! committed at `contracts/executor/artifacts/ArbitrageExecutor.full.json`).

use std::fs;
use std::path::Path;

use alloy::primitives::aliases::U24;
use alloy::primitives::{Address, U256};
use alloy::sol;
use alloy::sol_types::SolValue;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DeployError {
    #[error("io: {0}")]
    Io(String),
    #[error("json: {0}")]
    Json(String),
    #[error("missing field: {0}")]
    MissingField(String),
    #[error("hex: {0}")]
    Hex(String),
}

/// Read a forge full-artifact JSON's `bytecode.object` (creation bytecode,
/// i.e. what actually runs the constructor — distinct from
/// `deployedBytecode`, which is only the post-constructor runtime code).
pub fn load_creation_bytecode(full_artifact_path: &Path) -> Result<Vec<u8>, DeployError> {
    let raw = fs::read_to_string(full_artifact_path)
        .map_err(|e| DeployError::Io(format!("{}: {e}", full_artifact_path.display())))?;
    let value: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| DeployError::Json(e.to_string()))?;
    let object = value
        .get("bytecode")
        .and_then(|b| b.get("object"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| DeployError::MissingField("bytecode.object".to_string()))?;
    let object = object.strip_prefix("0x").unwrap_or(object);
    alloy::hex::decode(object).map_err(|e| DeployError::Hex(e.to_string()))
}

// `alloy_sol_types::SolValue` deliberately has no direct impl for bare `u8`
// (it collides with the `bytes`/`bytesN` element semantics — see that
// crate's own `// TODO: u8 is specialized to encode as bytes or bytesN`
// comment), so a raw `(String, String, u8)` tuple cannot use
// `.abi_encode_params()`. A `sol!`-generated struct sidesteps this: its
// `uint8` field maps directly to `sol_data::Uint<8>` without going through
// a `SolValue` impl on `u8` itself, and — since Solidity ABI-encodes structs
// exactly like top-level tuples of their fields — its `abi_encode_params()`
// is byte-identical to the real constructor encoding.
sol! {
    struct FixtureErc20CtorArgs {
        string name_;
        string symbol_;
        uint8 decimals_;
    }
}

/// Build `FixtureERC20`'s contract-creation `data`:
/// `constructor(string name_, string symbol_, uint8 decimals_)`.
pub fn fixture_erc20_init_code(
    creation_bytecode: &[u8],
    name: &str,
    symbol: &str,
    decimals: u8,
) -> Vec<u8> {
    let ctor_args = FixtureErc20CtorArgs {
        name_: name.to_string(),
        symbol_: symbol.to_string(),
        decimals_: decimals,
    };
    let args = ctor_args.abi_encode_params();
    [creation_bytecode, args.as_slice()].concat()
}

/// Build `E2EFixturePoolV2`'s contract-creation `data`:
/// `constructor(address token0_, address token1_, uint256 fee_)`.
pub fn fixture_pool_v2_init_code(
    creation_bytecode: &[u8],
    token0: Address,
    token1: Address,
    fee: U256,
) -> Vec<u8> {
    let args = (token0, token1, fee).abi_encode_params();
    [creation_bytecode, args.as_slice()].concat()
}

/// Build `E2EFixturePoolAgniV3`'s contract-creation `data`:
/// `constructor(address token0_, address token1_, uint24 fee_)`.
pub fn fixture_pool_agni_v3_init_code(
    creation_bytecode: &[u8],
    token0: Address,
    token1: Address,
    fee: U24,
) -> Vec<u8> {
    let args = (token0, token1, fee).abi_encode_params();
    [creation_bytecode, args.as_slice()].concat()
}

/// Build `ArbitrageExecutor`'s contract-creation `data`:
/// `constructor(address wmnt_, address admin_)`.
pub fn arbitrage_executor_init_code(
    creation_bytecode: &[u8],
    wmnt: Address,
    admin: Address,
) -> Vec<u8> {
    let args = (wmnt, admin).abi_encode_params();
    [creation_bytecode, args.as_slice()].concat()
}

// Call-only interfaces for fixture setup transactions (mint / seed) not
// already covered by `super::super::contract`'s production-venue interfaces.
sol! {
    #[sol(rpc)]
    interface IFixtureErc20 {
        function mint(address to, uint256 amount) external;
        function transfer(address to, uint256 amount) external returns (bool);
        function approve(address spender, uint256 amount) external returns (bool);
    }
}

sol! {
    #[sol(rpc)]
    interface IFixturePoolV2Seed {
        function seed() external;
    }
}

sol! {
    #[sol(rpc)]
    interface IFixturePoolAgniV3Seed {
        function seed(uint160 initialSqrtPriceX96, uint128 initialLiquidity) external;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    #[test]
    fn fixture_pool_v2_init_code_matches_hand_computed_static_encoding() {
        let token0 = address!("0x0000000000000000000000000000000000000001");
        let token1 = address!("0x0000000000000000000000000000000000000002");
        let fee = U256::from(300u64);
        let bytecode = vec![0xAAu8, 0xBB];

        let init_code = fixture_pool_v2_init_code(&bytecode, token0, token1, fee);
        assert_eq!(&init_code[..2], &bytecode[..]);

        let args = &init_code[2..];
        assert_eq!(args.len(), 96);
        let mut expected = vec![0u8; 96];
        expected[31] = 0x01;
        expected[63] = 0x02;
        expected[95] = 0x2C; // 300 == 0x012C
        expected[94] = 0x01;
        assert_eq!(args, expected.as_slice());
    }

    #[test]
    fn fixture_pool_agni_v3_init_code_matches_hand_computed_static_encoding() {
        let token0 = address!("0x0000000000000000000000000000000000000003");
        let token1 = address!("0x0000000000000000000000000000000000000004");
        let fee = U24::from(2500u32);
        let bytecode = vec![0xCCu8];

        let init_code = fixture_pool_agni_v3_init_code(&bytecode, token0, token1, fee);
        let args = &init_code[1..];
        assert_eq!(args.len(), 96);
        let mut expected = vec![0u8; 96];
        expected[31] = 0x03;
        expected[63] = 0x04;
        expected[95] = 0xC4; // 2500 == 0x09C4
        expected[94] = 0x09;
        assert_eq!(args, expected.as_slice());
    }

    #[test]
    fn arbitrage_executor_init_code_matches_hand_computed_static_encoding() {
        let wmnt = address!("0x0000000000000000000000000000000000000005");
        let admin = address!("0x0000000000000000000000000000000000000006");
        let bytecode = vec![0xDDu8, 0xEE, 0xFF];

        let init_code = arbitrage_executor_init_code(&bytecode, wmnt, admin);
        assert_eq!(&init_code[..3], &bytecode[..]);
        let args = &init_code[3..];
        assert_eq!(args.len(), 64);
        let mut expected = vec![0u8; 64];
        expected[31] = 0x05;
        expected[63] = 0x06;
        assert_eq!(args, expected.as_slice());
    }

    #[test]
    fn fixture_erc20_init_code_round_trips_through_abi_decode_params() {
        let bytecode = vec![0x11u8, 0x22, 0x33];
        let init_code = fixture_erc20_init_code(&bytecode, "Fixture USD", "fUSD", 6);
        assert_eq!(&init_code[..3], &bytecode[..]);

        let args = &init_code[3..];
        let decoded =
            FixtureErc20CtorArgs::abi_decode_params(args).expect("decode ctor args");
        assert_eq!(decoded.name_, "Fixture USD");
        assert_eq!(decoded.symbol_, "fUSD");
        assert_eq!(decoded.decimals_, 6);
    }

    #[test]
    fn load_creation_bytecode_reads_object_and_strips_0x_prefix() {
        let dir = std::env::temp_dir().join(format!(
            "whi525-deploy-bytecode-test-{:?}",
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Sample.full.json");
        std::fs::write(
            &path,
            r#"{"bytecode": {"object": "0xdeadbeef"}, "deployedBytecode": {"object": "0x"}}"#,
        )
        .unwrap();

        let bytecode = load_creation_bytecode(&path).expect("load bytecode");
        assert_eq!(bytecode, vec![0xde, 0xad, 0xbe, 0xef]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_creation_bytecode_reports_missing_field() {
        let dir = std::env::temp_dir().join(format!(
            "whi525-deploy-bytecode-missing-{:?}",
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Sample.full.json");
        std::fs::write(&path, r#"{"deployedBytecode": {"object": "0x"}}"#).unwrap();

        let err = load_creation_bytecode(&path).unwrap_err();
        assert!(matches!(err, DeployError::MissingField(field) if field == "bytecode.object"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
