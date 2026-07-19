# M0 · Stop the Bleeding + Restore Buildability

> Linear: [Mantle Arbitrage bots v2](https://linear.app/whisker-personal/project/mantle-arbitrage-bots-v2-614934acbe12)  
> 快照来源：Linear issues + `specs/06-milestones-and-issues.md`（2026-07-19）  
> **Success** = 旧 executor 永久退役且可提 ERC-20 清零（native MNT 已盘点）；硬化 executor 已重部署；`cargo build/test` 全绿；动态 gas 交易可被 fork/testnet 接受。

| 占位 | Linear | 标题 | Pri | Labels |
| --- | --- | --- | --- | --- |
| M0-1 | [WHI-500](https://linear.app/whisker-personal/issue/WHI-500) | Drain and permanently retire the vulnerable executor | Urgent | bug, ready-for-human |
| M0-8 | [WHI-501](https://linear.app/whisker-personal/issue/WHI-501) | Enforce canonical pool provenance in the replacement executor | Urgent | bug, ready-for-human |
| M0-2 | [WHI-502](https://linear.app/whisker-personal/issue/WHI-502) | Implement dynamic gas with fail-closed estimate fallback | Urgent | bug |
| M0-3 | [WHI-503](https://linear.app/whisker-personal/issue/WHI-503) | Protect Moe principal on every resized execution | Urgent | bug |
| M0-4 | [WHI-505](https://linear.app/whisker-personal/issue/WHI-505) | Repair Cargo.toml example targets and restore green build/test | Urgent | chore |
| M0-5 | [WHI-506](https://linear.app/whisker-personal/issue/WHI-506) | Pin Rust/Solidity/Foundry toolchain | High | chore |
| M0-6 | [WHI-507](https://linear.app/whisker-personal/issue/WHI-507) | Generate and validate a dedicated Moe pool list | High | chore |
| M0-7 | [WHI-508](https://linear.app/whisker-personal/issue/WHI-508) | Remove tracked junk and confirmed dead code | Medium | chore |

> 重复项：`WHI-504`→Duplicate of WHI-503；`WHI-509`→Duplicate of WHI-508。

---

## WHI-500 · Drain / retire vulnerable executor (M0-1)

- **做什么**：立刻停发 + 扫出旧 executor 上所有 callback 可窃取 ERC-20；单独盘点 native MNT（旧 ABI 提不走）；地址永久退役并写部署记录。
- **影响**：资金安全入口；阻塞 replacement 部署（WHI-501）。不关 = 任何人仍可经 callback 掏空合约。
- **触及**：`contracts/executor/ArbitrageExecutor.sol`、部署/env、ops。

## WHI-501 · Replacement executor + pool provenance (M0-8)

- **做什么**：新 executor：callback/path 校验 canonical pool provenance、settlement cycle、可回收 native MNT；仅此地址可再注资。
- **影响**：所有后续链上执行面；阻塞 Moe 本金保护（WHI-503）与 hop min-out/deadline（WHI-523）。
- **触及**：`contracts/**`、ABI 再生、部署与注资流程。

## WHI-502 · Dynamic gas + fail-closed estimate (M0-2)

- **做什么**：去掉写死 60M gas / 50 gwei；按 live fee + `eth_estimateGas` 估费，估失败则 fail-closed 不发。
- **影响**：交易能否上链、资格门槛（WHI-515）、执行状态机（WHI-519）。不关 = 与证据块不兼容、易永久卡死。
- **触及**：`src/execution/**`、四个 `*_monitor_executor_service`。

## WHI-503 · Protect Moe principal on resize (M0-3)

- **做什么**：Moe 路径在 resize 输入时仍强制本金/min-return，防止“缩小仓位后亏本金仍发出”。
- **影响**：Moe 实盘本金安全；阻塞 Sepolia E2E / P2.5 gate（WHI-525/526）。
- **触及**：Moe 执行路径、replacement executor 调用面。

## WHI-505 · Green build/test (M0-4)

- **做什么**：修 `Cargo.toml` example targets，恢复 `cargo build/test --all-targets` 全绿。
- **影响**：整条工程可构建基线；阻塞清理（WHI-508）与 MarketSnapshot（WHI-510）。
- **触及**：`Cargo.toml`、broken examples/tests。

## WHI-506 · Pin toolchain (M0-5)

- **做什么**：钉死 Rust/Solidity/Foundry 版本与依赖初始化，保证可复现。
- **影响**：CI/本地一致性；阻塞清理与 Sepolia E2E 环境。
- **触及**：toolchain 文件、Foundry/submodule、ignore 边界。

## WHI-507 · Dedicated Moe pool list (M0-6)

- **做什么**：生成并校验专用 Moe pool 列表（非混用 V2/V3 清单）。
- **影响**：Moe 快照正确性（WHI-513）与后续静态 manifest（WHI-536）。
- **触及**：`data/poolLists_moe`、发现/校验脚本。

## WHI-508 · Safe junk / dead-code cleanup (M0-7)

- **做什么**：删除已确认无引用的 tracked junk 与死代码（在绿构建之后）。
- **影响**：降低噪音与误用入口；不改变运行语义。
- **触及**：仓库杂项、确认的 dead modules。
