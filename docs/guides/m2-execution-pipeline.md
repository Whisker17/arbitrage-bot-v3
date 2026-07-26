# M2 · Execution State Machine + Versioned Pipeline + Diff Tests

> 占位 `M2-1..8` = `WHI-519..526` · 当前多为 `needs-triage`（设计摘要，展开后再实现）  
> **Success** = 版本化 pipeline；执行不阻塞 `watch()`；过期/陈旧不可发；差分测试过；Sepolia E2E 对账；P2.5 shadow 零发送。  
> **人工门禁**：`WHI-526` approve 才可开 M3。

| 占位 | Linear | 标题 | Pri | Labels |
| --- | --- | --- | --- | --- |
| M2-1 | [WHI-519](https://linear.app/whisker-personal/issue/WHI-519) | Nonce-intent execution state machine | Urgent | feature, needs-triage |
| M2-2 | [WHI-520](https://linear.app/whisker-personal/issue/WHI-520) | Version Snapshot→Candidate→Preflight→Submit | Urgent | feature, needs-triage |
| M2-3 | [WHI-521](https://linear.app/whisker-personal/issue/WHI-521) | Preflight exact final request before submit | High | feature, needs-triage |
| M2-4 | [WHI-522](https://linear.app/whisker-personal/issue/WHI-522) | Exact fixed-state AMM differential tests | High | feature, needs-triage |
| M2-5 | [WHI-523](https://linear.app/whisker-personal/issue/WHI-523) | Per-hop output deltas + on-chain deadline | High | bug, needs-triage |
| M2-6 | [WHI-524](https://linear.app/whisker-personal/issue/WHI-524) | Circuit breakers + pending cancellation | High | feature, needs-triage |
| M2-7 | [WHI-525](https://linear.app/whisker-personal/issue/WHI-525) | Reusable Mantle Sepolia arbitrage E2E gate | High | feature, needs-triage → human |
| M2-8 | [WHI-526](https://linear.app/whisker-personal/issue/WHI-526) | P2.5 signerless shadow validation gate | High | feature, needs-triage → human |

---

## WHI-519 · Nonce-intent execution SM (M2-1)

- **做什么**：发现/发送/回执解耦；nonce intent 多 attempt、canonical 纳入/终局、reorg 恢复、同 nonce replace/cancel；热路径不 `watch()`。
- **影响**：执行层核心；阻塞版本化 pipeline、断路器、PnL 对账。不关 = 串行堵死、丢单/重用 nonce。
- **触及**：`src/execution/**`、全部 active services。

## WHI-520 · Versioned pipeline (M2-2)

- **做什么**：把完整 `SnapshotId` + universe/manifest 世代贯穿 Candidate→最终请求→preflight→submit；状态/拓扑不一致则丢弃重算。
- **影响**：杜绝跨块/跨拓扑静默串台；阻塞 exact preflight 与两道 gate。
- **触及**：arbitrage/pipeline 组件、各 entrypoint。

## WHI-521 · Risk-tiered exact final-request preflight (M2-3)

- **做什么**：广播前对**最终**请求字节做风险分级的零或一次 `eth_call`，绝不 `eth_estimateGas`。
  `Mandatory`（e2e/shadow/canary，或生产环境无有效签名审批）恰好一次调用，RPC 失败或
  revert 都拒绝且不签名；`ApprovedStable`（仅生产环境）持有效签名审批时，`disabled`
  模式零调用直接跳过，`sampled` 模式按请求摘要确定性抽样，未命中同样零调用。审批记录
  过期/吊销/域名或 principal 不符/scope 不匹配一律回退到 `Mandatory`。
- **影响**：链下模拟与链上请求一致性；每次尝试的 outcome/摘要/block tag/延迟均被记录，
  E2E/shadow 证据质量的基础。
- **触及**：`src/execution/preflight.rs`（`RiskTieredPreflight`/`SemanticCallExecutor`）,
  `examples/protocols/intent_service_support.rs` 唯一接线点。

## WHI-522 · AMM differential tests (M2-4)

- **做什么**：固定块上 V2 舍入、Agni/UniV3 跨 tick、Moe 跨 bin 与链上精确比对；不支持的 hook fail-closed。
- **影响**：数学正确性门禁；阻塞 shared-core、Moe tree_math、multi-peak 优化。
- **触及**：`tests/` differential harness、各 AMM 入口。

## WHI-523 · Hop min-out + deadline (M2-5)

- **做什么**：链上按 hop balance delta 校验 min-out + `deadline`；保留 M0-8 provenance/settlement 不变量。
- **影响**：夹心/过期交易可在链上硬失败；阻塞执行 SM 与 gates。
- **触及**：`contracts/executor/**`、Rust ABI 调用方、重部署。

## WHI-524 · Circuit breakers + cancel (M2-6)

- **做什么**：连续 revert 停机、窗口最大亏损、单笔仓位帽、鉴权 kill switch；pause 时 best-effort cancel pending。
- **影响**：运营风险上限；E2E/shadow 前必备。
- **触及**：execution SM 计数器与 pause 路径。

## WHI-525 · Mantle Sepolia E2E gate (M2-7)

- **做什么**：隔离凭证的可复用 Sepolia 环境；一键制造价差；跑通真实 pipeline 到 canonical finality 并对账。
- **影响**：证明“修完的 bot 能在测试网闭环”；阻塞 P2.5 人工门。
- **触及**：E2E 脚本/配置、`MANTLE_SEPOLIA_E2E_*`、fixture 部署。

## WHI-526 · P2.5 signerless shadow (M2-8)

- **做什么**：无 signer/broadcast 的完整观察流水线 + 预声明证据阈值；人工 approve/reject。
- **影响**：**整条 M3 的总闸门**——仅 approve 解锁合并/观测后续；reject 则 M3 停。approve 不继承给 merged binary（还需 WHI-535）。
- **触及**：shadow runtime、ledger、决策记录。
