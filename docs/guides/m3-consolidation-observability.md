# M3 · Consolidation + Observability

> 占位 `M3-1..10` = `WHI-527..536` · 依赖 **WHI-526 approve**  
> **Success** = 单一多协议 binary；版本化 pool/path manifest；PnL/metrics/checkpoint 可验证；merged binary 独立 signerless 再认证后才给 production signer。  
> **人工门禁**：`WHI-535` approve 才可 production enable + 清旧代码。

| 占位 | Linear | 标题 | Pri | Labels |
| --- | --- | --- | --- | --- |
| M3-1 | [WHI-527](https://linear.app/whisker-personal/issue/WHI-527) | Merge four services → one multi-protocol binary | Urgent | feature, needs-triage |
| M3-2 | [WHI-528](https://linear.app/whisker-personal/issue/WHI-528) | Share CL core Agni↔UniV3 | High | chore, needs-triage |
| M3-3 | [WHI-529](https://linear.app/whisker-personal/issue/WHI-529) | Settlement semantics; kill invalid misprice paths | High | feature, needs-triage |
| M3-4 | [WHI-530](https://linear.app/whisker-personal/issue/WHI-530) | Integrity-checked checkpoints | High | feature, needs-triage |
| M3-5 | [WHI-531](https://linear.app/whisker-personal/issue/WHI-531) | Canonical-finalized PnL ↔ chain balances | High | feature, needs-triage |
| M3-6 | [WHI-532](https://linear.app/whisker-personal/issue/WHI-532) | Prometheus pipeline/execution metrics | Medium | feature, needs-triage |
| M3-7 | [WHI-533](https://linear.app/whisker-personal/issue/WHI-533) | Full reorg unwind without deep-reorg panic | Medium | feature, needs-triage |
| M3-8 | [WHI-534](https://linear.app/whisker-personal/issue/WHI-534) | Remove dead lib after merge | Medium | chore, needs-triage |
| M3-9 | [WHI-535](https://linear.app/whisker-personal/issue/WHI-535) | Requalify merged binary (signerless shadow) | Urgent | feature, needs-triage → human |
| M3-10 | [WHI-536](https://linear.app/whisker-personal/issue/WHI-536) | Versioned static pool/path manifest | High | feature, needs-triage → human |

---

## WHI-527 · One multi-protocol binary (M3-1)

- **做什么**：四个 `*_monitor_executor_service`（~5.6k 行）收成 `src/bin/bot.rs`；`Protocol` trait 并发启用 V2/V3/Moe；启动冻结的 legacy universe adapter + fingerprint。
- **影响**：跨 DEX 套利前提；阻塞 settlement、metrics、manifest、M4 bench、post-merge gate。
- **触及**：services → lib 抽取、`src/bin/bot.rs`、配置统一。

## WHI-528 · Shared concentrated-liquidity core (M3-2)

- **做什么**：去掉 Agni/UniV3 ~900 行复制，共享 tick/liquidity 数学；Agni 只留 event/ABI/factory 适配。
- **影响**：可维护性与一致 bugfix；差分测试守门；阻塞最终死代码清理。
- **触及**：`src/amms/agni`、`src/amms/uniswap_v3`。

## WHI-529 · Settlement semantics (M3-3)

- **做什么**：策略层强制 `settlement_asset` 闭环（本部署 = WMNT）；删除单位错误的 open `find_two_pool_misprices`；按有序 hop 去重。
- **影响**：利润单位正确、路径语义清晰；post-merge shadow 前置。
- **触及**：`src/arbitrage` engine/path finder。

## WHI-530 · Checkpoints (M3-4)

- **做什么**：带 digest 的原子 checkpoint；恢复前校验 canonicality + 链上关键字段；损坏/跨链/过期不发布为可报价快照。
- **影响**：冷启动速度 vs 状态安全；不削弱 M1 语义。
- **触及**：`src/state_space` 持久化。

## WHI-531 · Realized PnL reconcile (M3-5)

- **做什么**：持久化每个 nonce intent/attempt；**仅** canonical-finalized receipt 计 realized PnL；对账 executor 库存与 sender gas。
- **影响**：账本可信；shadow 期望利润不得混入 realized。
- **触及**：execution ledger / SQLite、指标。

## WHI-532 · Prometheus metrics (M3-6)

- **做什么**：暴露块处理延迟、机会数、submit/成功率、余额、累计 gas、断路器等；默认 loopback。
- **影响**：可观测与 M4 延迟测量输入；post-merge gate 证据。
- **触及**：merged binary metrics endpoint。

## WHI-533 · Reorg unwind (M3-7)

- **做什么**：`StateChangeCache` 回滚到分叉前；超深 reorg 返回错误并 resync，禁止 panic；提高 CACHE_SIZE。
- **影响**：分叉正确性；阻塞 inverse-delta 研究（WHI-544）。
- **触及**：`src/state_space` cache。

## WHI-534 · Dead code after merge (M3-8)

- **做什么**：M3-9 approve 后删除已退役 service 与零引用组件；保留仍在用的 mock/测试依赖。
- **影响**：仓库瘦身、单一生产入口；依赖 post-merge 门禁通过。
- **触及**：旧 services、确认 dead libs。

## WHI-535 · Post-merge signerless requalify (M3-9)

- **做什么**：merged binary 在无 signer 下做 replay 等价 + 跨协议/调度/manifest 风险样本 shadow；人工决定是否给 production signer。
- **影响**：**第二道总闸门**——M2-8 批准不可继承；reject 则清理与上线均阻塞。
- **触及**：shadow/replay 证据、部署策略。

## WHI-536 · Static pool/path manifest (M3-10)

- **做什么**：离线生成可审查的版本化 pool + ≤3 hop 结算环 manifest；热路径只消费批准的 universe；晋升原子、世代切换清空旧候选。
- **影响**：发现确定性与可审计拓扑；阻塞 path-index 缓存（WHI-543）与 post-merge gate。
- **触及**：manifest 工具链、`PoolUniverseSource`、allowlist 交集规则。
