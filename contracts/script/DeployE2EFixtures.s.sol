// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import "forge-std/Script.sol";
import "../src/fixtures/FixtureERC20.sol";
import "../src/fixtures/E2EFixturePoolV2.sol";
import "../src/fixtures/E2EFixturePoolAgniV3.sol";

interface IWMNT {
    function deposit() external payable;
    function transfer(address to, uint256 amount) external returns (bool);
    function balanceOf(address account) external view returns (uint256);
}

/**
 * @title DeployE2EFixtures
 * @notice Deploys the WHI-525 Mantle Sepolia E2E fixture counter token and
 *         pools: one V2-style pair and one Agni/V3-style pool, each pairing
 *         the real Mantle Sepolia WMNT against one fresh fixture counter
 *         token, so a same-protocol price imbalance in either pool creates a
 *         two-hop arbitrage cycle through the other.
 * @dev Chain-id-guarded to Mantle Sepolia (5003) only — these are repo-owned
 *      test fixtures, never a production deployment target.
 *
 *      Pairs against the real Sepolia WMNT (the same constant
 *      `FundExecutor.s.sol` / `DeployArbitrageExecutor.s.sol` already use)
 *      rather than a freshly-minted fixture token: `derive_runtime_identity`
 *      (WHI-525 M2) is a pure offline derivation keyed on a fixed `--wmnt`
 *      address, so `config/executor_identity.sepolia.json` must bind to a
 *      WMNT address that is known *before* any bootstrap deploy runs — which
 *      only the real, already-deployed WMNT satisfies.
 *
 * Env:
 *   - MANTLE_SEPOLIA_PRIVATE_KEY
 *   - optional V2_FEE (default 300, i.e. 0.30% in 1/100_000 units)
 *   - optional V3_FEE (default 3000, i.e. 0.30% in 1/1_000_000 units)
 *   - optional INITIAL_LIQUIDITY_WMNT (default 1 ether, per pool) /
 *     INITIAL_LIQUIDITY_TOKEN (default 1_000_000e18, per pool)
 *
 * Requires the deployer to hold at least `2 * INITIAL_LIQUIDITY_WMNT` of
 * native MNT, wrapped into WMNT here via `deposit()` (mirrors FundExecutor.s.sol).
 */
contract DeployE2EFixtures is Script {
    uint256 internal constant CHAIN_ID_MANTLE_SEPOLIA = 5003;
    uint160 internal constant Q96 = 0x1000000000000000000000000;
    address internal constant WMNT = 0x67A1f4A939b477A6b7c5BF94D97E45dE87E608eF;

    function run() external {
        require(block.chainid == CHAIN_ID_MANTLE_SEPOLIA, "DeployE2EFixtures: wrong chain");

        uint256 deployerPrivateKey = vm.envUint("MANTLE_SEPOLIA_PRIVATE_KEY");
        uint256 v2Fee = vm.envOr("V2_FEE", uint256(300));
        uint256 v3Fee = vm.envOr("V3_FEE", uint256(3000));
        uint256 initialWmntPerPool = vm.envOr("INITIAL_LIQUIDITY_WMNT", uint256(1 ether));
        uint256 initialTokenPerPool = vm.envOr("INITIAL_LIQUIDITY_TOKEN", uint256(1_000_000 ether));

        vm.startBroadcast(deployerPrivateKey);
        address deployer = vm.addr(deployerPrivateKey);

        FixtureERC20 tokenFixture = new FixtureERC20("E2E Fixture Token", "eTOK", 18);

        (address token0, address token1) =
            WMNT < address(tokenFixture) ? (WMNT, address(tokenFixture)) : (address(tokenFixture), WMNT);
        (uint256 initial0, uint256 initial1) =
            token0 == WMNT ? (initialWmntPerPool, initialTokenPerPool) : (initialTokenPerPool, initialWmntPerPool);

        E2EFixturePoolV2 v2Pool = new E2EFixturePoolV2(token0, token1, v2Fee);
        E2EFixturePoolAgniV3 v3Pool = new E2EFixturePoolAgniV3(token0, token1, uint24(v3Fee));

        IWMNT wmnt = IWMNT(WMNT);
        wmnt.deposit{value: initialWmntPerPool * 2}();

        FixtureERC20(tokenFixture).mint(address(v2Pool), initialTokenPerPool);
        require(wmnt.transfer(address(v2Pool), initialWmntPerPool), "WMNT transfer to v2Pool failed");
        v2Pool.seed();

        FixtureERC20(tokenFixture).mint(address(v3Pool), initialTokenPerPool);
        require(wmnt.transfer(address(v3Pool), initialWmntPerPool), "WMNT transfer to v3Pool failed");
        // sqrtPriceX96 for an even 1:1 initial price; liquidity = sqrt(initial0 * initial1).
        uint160 initialSqrtPriceX96 = uint160(Q96);
        uint128 initialLiquidity = uint128(_sqrt(initial0 * initial1));
        v3Pool.seed(initialSqrtPriceX96, initialLiquidity);

        console.log("WMNT (Mantle Sepolia):", WMNT);
        console.log("FixtureERC20 (counter token):", address(tokenFixture));
        console.log("E2EFixturePoolV2:", address(v2Pool));
        console.log("E2EFixturePoolAgniV3:", address(v3Pool));
        console.log("token0:", token0);
        console.log("token1:", token1);
        console.log("deployer:", deployer);
        vm.stopBroadcast();
    }

    function _sqrt(uint256 x) internal pure returns (uint256 y) {
        if (x == 0) return 0;
        uint256 z = (x + 1) / 2;
        y = x;
        while (z < y) {
            y = z;
            z = (x / z + z) / 2;
        }
    }
}
