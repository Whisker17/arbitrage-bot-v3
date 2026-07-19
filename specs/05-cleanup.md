# 05 · 死代码 / 垃圾文件清理清单

分三档：**纯删除（零风险）** / **删除但需确认** / **修 .gitignore**。带 ✅ 的可以直接删。

> **v2 更新（经 review 修正）**：第一版把 `broadcast/`、`foundry.lock`、`mock.rs` 都归进"纯删除零风险"——**过头了**。已重新分类：
> - `broadcast/**` 是**部署溯源资产**（记录了哪个地址、哪笔 tx 部署的合约），不是垃圾。它不该被 git 跟踪（应 gitignore），但删之前要确认部署信息已另行留档（如记进 `tech-docs/deployments.md`），别直接丢历史
> - `foundry.lock` 是**可复现性资产**（锁定 forge 依赖版本），应**保留并提交**，不要删
> - `mock.rs` 是**离线测试设施**（无网络的 AMM 数学测试夹具），不是死代码。它只被测试和 `mock_arbitrage` example 用，但那是它的正常用途

---

## A. 纯删除：垃圾文件（零风险，立即可删）

### A1. 已跟踪进 git 的垃圾
```
✅ .DS_Store                                  # macOS 垃圾
✅ contracts/executor/ArbitrageExecutor.sol_bak  # 旧版合约备份（296 行，缺 swapForY/完整 _validateStates）
✅ src/amms/agni/mod.rs.bak                    # src 里的 .bak
✅ data/poolLists.csv.bak
✅ data/poolLists.csv.bak (20251007)          # 文件名带空格，尤其恶心
✅ data/poolLists.csv_bbak
```

### A2. 构建产物（不该进 git）
```
✅ contracts/executor/out/**        # ~47 个 forge 编译产物 JSON（含 forge-std 的 Vm.json 等），可删
✅ contracts/executor/cache/**      # solidity-files-cache.json 等，可删
```
**需先留档再从 git 移除（非纯垃圾）**：
```
⚠️ contracts/executor/broadcast/**  # 部署溯源，删前确认部署地址/tx 已另行留档
⚠️ contracts/broadcast/**           # 旧 5003 Sepolia 部署溯源，同上
❌ contracts/foundry.lock           # 可复现性资产，保留并提交，不要删
```
`out/`、`cache/` 删完后**必须**同步改 `.gitignore`（见 C 节）；`broadcast/` 也应 gitignore 但先把部署信息抄进 `tech-docs/deployments.md`。

---

## B. 死代码：源码层（删除但建议先确认）

### B1. execution 模块死组件（服务全部绕过，直接调合约）
```
src/execution/gas.rs               # simple_competitive_gas_price 等，executor.rs:10 已注释弃用
src/execution/nonce.rs             # NonceManager 从未接线（改造后可复用，见 04-F5，不确定就先留）
src/execution/executor.rs          # Executor struct + ArbitrageOpportunity/SwapPath/Token/Pool
                                   #   占位符全死；只有 compute_fee_plan 是活的
                                   #   ⚠️ 保留 compute_fee_plan，删其余
```
`Executor::build_params/execute` 内部还是自相矛盾的（V2 数学 executor.rs:194 + 硬编码全 V3 poolType :285 → 必 revert），留着就是雷。

死配置字段：`ExecutorConfig::global_fee_hard_cap_wei`、`fixed_gas_price_wei`（types.rs:46,57，声明了从不用）。
`SwapStep` 的 `expected_amount_out`、`zero_for_one`（types.rs:23,27）永远是 None、从不读。

### B2. arbitrage 模块死组件（服务用自己的实现）
```
src/arbitrage/optimizer.rs::PathOptimizer::optimize   # 算法错；迁移 mock.rs:260 后再删
src/arbitrage/monitor.rs::ArbitrageMonitor            # 服务不用（自己写块内扫描）
```
**[已按 review 修正] `src/arbitrage/mock.rs` 不是死代码**：它是离线测试夹具，被 `mock_arbitrage` example 和测试正常使用。保留。第一版误列。
**删除顺序约束**：`mock.rs:260` 当前仍调用 `PathOptimizer::optimize`，`mock_arbitrage` 会走到这条路径。必须先把 mock 迁到新的多峰 optimizer，并让 replay/差分测试通过，再删除旧方法；“生产服务不用”不等于“全仓库零引用”。
⚠️ `optimizer.rs` 里 `pools_for_path`、`simulate_path` 服务**有用**（service 1559 import 了 `optimizer::pools_for_path`），别整个删。`graph.rs::build_graph`、`pathfinder` 服务都在用，保留。

`ArbitrageMonitor.graph: RwLock<Option<PoolGraph>>`（monitor.rs）被 `refresh_graph` 填了但 `opportunistic_scan` 忽略它自建局部 graph——死缓存。

### B3. state_space 模块死组件
```
src/state_space/discovery.rs        # DiscoveryManager 整个文件，只在注释掉的字段里被引用（mod.rs:44）
src/state_space/filters/value.rs    # ValueFilter 被注释出 filter! 宏（filters/mod.rs:58），无法构造
```
⚠️ `StateSpaceManager`/`StateSpaceBuilder` 服务不用（自己裸写块循环），但 02 方案 A 要把它们改造成 sync 模块——**别急着删，等架构决策**。

### B4. Moe 模块死代码
```
src/amms/moe/mod.rs:40-71    # calc_base_fee/calc_variable_fee/calc_total_fee/
                             #   calc_fee_amount/calc_fee_amount_from/calc_protocol_fee  全零调用
src/amms/moe/mod.rs:406-420  # price_liquidity 零调用
src/amms/moe/mod.rs:1679-1694 # price_from_id（f64 有损版，影子 price_helper）零调用
src/amms/moe/mod.rs:1696-1719 # mul_shift_round_down / shift_div_round_down 零调用
```
**[最终 review 纠正] `pair_parameters::Parameters` 不是死代码**：`moe/mod.rs:1936` 把它 alias 为 `MoeParameters`，精确模拟在 `:1492` 调 `MoeParameters::from_pair`。除非先完成有差分测试保护的参数实现替换，否则不得删除。
Moe math 端口里**当前完全没用到**的模块（要么删、要么按 03-P4 把 tree_math 接进 bin 遍历）：
```
src/amms/moe/math/sample_math.rs           # oracle 采样，无引用
src/amms/moe/math/tree_math.rs             # ⭐ 别删，应接线（03-P4 正确性+性能）
src/amms/moe/math/liquidity_configurations.rs  # 无引用
src/amms/moe/math/safe_cast.rs             # 无引用
```
`moe/math/mod.rs:20,24` 的 ambiguous glob re-export（`decode`/`encode` 冲突）——改成显式 re-export。

### B5. Agni 死字段
```
src/amms/agni/mod.rs: fee_protocol 字段 + :339-344/:379-384 的硬编码魔数赋值   # 只写不读
src/amms/uniswap_v3/mod.rs:177-186: Tick struct                              # 定义了没用（代码用 Info）
```

### B6. Agni 数学内核（架构合并后缩减）
`src/amms/agni/mod.rs` 的 tick 穿越/流动性数学与 `uniswap_v3/mod.rs` 大量重复（02 已详述）。合并后删除的是重复数学实现，**不是整个 Agni 协议模块**：Agni 自己的 event 解码、ABI/factory/initcode 和链上 adapter 必须保留。只有在共享内核的差分测试覆盖两种协议后，才能移除对应复制代码。

### B7. 测试膨胀
```
这些测试质量弱，但不应作为"低风险纯删除"处理。在 P2 的 V2 rounding、V3 跨 tick、Moe 跨 bin 差分测试落地并覆盖同一行为后，再替换或删除；不要在当前正确性覆盖不足时先减测试数量。
src/amms/uniswap_v3/mod.rs:1663-2078   # 4 个大测试基本是 println! 硬编码值 + 断言 trivial
                                       #   （test_wmnt_value_in_pools_payload 等），非行为覆盖
```

### B8. 库未用依赖
`Cargo.toml` 里 `chrono`、`once_cell`、`serde_json` 在库中未使用（`unused_crate_dependencies` warning）。确认没有 example 依赖后移除，或标 `dev-dependencies`。

---

## C. `.gitignore` 修复（删了 A2 必须同步做，否则复发）

现在 `.gitignore` 忽略了 `contracts/cache/`、`contracts/out/`，但**漏了** `contracts/executor/` 下的对应目录，所以 executor 的产物泄漏进仓库。补：
```gitignore
contracts/executor/out/
contracts/executor/cache/
contracts/executor/broadcast/
contracts/broadcast/
.DS_Store
**/*.bak
**/*_bak
```
注意：
- **不要** ignore `foundry.lock`（可复现性资产，反而要提交）
- **[重要] 移除对 `Cargo.lock` 的忽略并提交它**。现在 `.gitignore:2` 忽略了 `Cargo.lock`，导致 `alloy="1.0.25"` 实际解析成 `1.8.3`——实盘 bot 依赖不可复现是大忌。二进制/应用型项目应提交 lockfile，升级走显式 `cargo update` + review
- **[已修正] `.gitignore` 忽略 `contracts/lib/` 不是阻断性问题**。已验证 `contracts/lib/forge-std` 在 index 里是 gitlink（mode `160000`），且 `git check-ignore` 对它返回空——gitignore **不会**阻止 `git submodule update --init`。submodule 未初始化（01-A2）只是因为没跑 init 命令，不是被 gitignore 挡住。可以移除这条 ignore 规则减少混淆，但**别把它列为初始化失败原因**

---

## D. 过时的 example / test target（01-A3 已列，这里给处置建议）

编译失败、非生产入口。**注意 `examples/subscribe.rs`、`examples/swap_calldata.rs` 是被 Cargo 自动发现的隐式 target**——它们没有 `[[example]]` 声明可摘，只能**实际删文件或移出 `examples/` 根目录**：
```
examples/subscribe.rs                        # dotenvy 导入失效（隐式 target，删文件）
examples/swap_calldata.rs                    # API 签名过时（隐式 target，删文件）
examples/test/execute_sepolia_arbitrage.rs   # 13 错误，老 API（子目录，Cargo.toml 有显式声明，摘声明+删）
tests/moe_swap.rs                            # 8 错误 + 依赖缺失 CSV + live RPC（删或修）
```
**[已修正] 根目录 `examples/*.rs` 是 Cargo 自动发现的 target**（`cargo metadata` 核实：`ut`/`subscribe`/`simulate_swap`/`filters`/`swap_calldata`/`test_mantle_rpc`/`sync_macro`/`state_space_builder` 全部 AUTO-DISCOVERED）。它们不是"游离文件"，是隐式 target——所以其中编译失败的才会拖垮 `--all-targets`。清理只能删文件或移出根目录，**没有"声明"可摘**：
```
examples/ut.rs  examples/simulate_swap.rs  examples/filters.rs
examples/test_mantle_rpc.rs  examples/sync_macro.rs  examples/state_space_builder.rs
```
**[已修正] Cargo 的自动发现规则要说准**：Cargo **会**自动发现 `examples/<name>.rs` 和 `examples/<name>/main.rs` 两种布局。`examples/payload-test/` 里是 `lib.rs + bin/moe.rs + bin/agni.rs`——**不符合** `<name>/main.rs` 布局，所以确实不被自动发现、需要显式声明才成 target：
```
examples/payload-test/**  (lib.rs + bin/moe.rs + bin/agni.rs)  # 布局不符自动发现，当前非 target
```
建议：确认无用的根目录 example 直接删。

---

## E. 文档 / 数据一致性问题（不是删除，是修）

- `tech-docs/research/**` 全是**符号链接**指向隔壁仓库 `mantle-arbitrage-system/arbitrage-analysis/`（INDEX.md 也是）——本仓库单独 checkout 时全断。要么把内容 copy 进来、要么在 README 注明依赖隔壁仓库。
- `tech-docs/Moe/TODO.md` 里的路径是旧的绝对路径 `…/Whisker17/arbitrage/arbitrage-bot-v3/…` 和 `examples/moe`（现在是 `examples/protocols/moe`）——仓库搬过位置，文档没更新。
- `tech-docs/Agni/QUICK_START.md` 还叫人跑 `cargo run --example execute_sepolia_arbitrage`（那是编译不过的遗留一次性脚本），不是真实服务入口。文档需重写指向 `*_monitor_executor_service`。
- `scripts/deploy_and_fund_mainnet.sh` 引用不存在的 `env.mainnet.example`（只有 `env.sepolia.example`）。
- `foundry.toml`（两个）硬编码机器路径，确实不可移植；但只改成 PATH 仍不可复现。应精确 pin solc 版本（收紧 pragma/Foundry 配置）、pin Foundry 版本，并增加 `rust-toolchain.toml` 固定 Rust；CI 在构建前核对 `solc --version`、`forge --version`、`rustc --version` 和 lockfile 未漂移。SVM/rustup 只是安装机制，不是版本策略。

---

## 清理量级预估

| 类别 | 文件/行数 | 风险 |
| --- | --- | --- |
| A1 垃圾文件（.bak/.DS_Store） | 6 个 | 零 |
| A2 `out/`+`cache/` 产物 | ~50 个 | 零（删+gitignore） |
| A2 `broadcast/` 溯源 | ~15 个 | 低（先留档再移除） |
| B1-B5 明确死代码 | ~15 处函数/字段/模块 | 低（确认零调用后删） |
| B6 Agni 数学合并 | ~900 行 | 中（依赖架构决策，保留 adapter） |
| B7 弱测试替换 | ~400 行 | 中（先补差分覆盖再删） |
| D 过时 target | 4 编译失败（其中 2 个是根目录隐式 target）+ payload-test 3 文件（真非 target） | 低 |

保守估计**不含 B6 就能删掉 1500+ 行源码 + ~55 个文件**，可读性和编译告警会显著改善。B6 再省 ~900 行。**注意**：`foundry.lock`、`mock.rs`、`Cargo.lock`（应新增提交）不在删除范围。
