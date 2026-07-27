# 实现执行手册(可勾选 TODO)

> 按此逐轮推进:**每一轮 = 此刻能同时开工的 agent + 具体 WHI issue**。勾掉一项代表它已 merge;每轮末尾的 **▶ 解锁** 告诉你接下来能开哪些。
>
> - Linear:project `Mantle Arbitrage bots v2`(team `WHI`)。issue 设计见 [`specs/06-milestones-and-issues.md`](../../specs/06-milestones-and-issues.md)。
> - **Git 强制规则**(全文 [`docs/GIT_WORKFLOW.md`](../GIT_WORKFLOW.md)):
>   1. 每个 issue 从最新 `origin/dev` **新开 worktree + 分支**(禁止在主 clone 的 `dev` 上直接开发)
>   2. 实现完成后 `gh pr create --base dev`,Linear → **`In Review`**
>   3. Reviewer squash-merge 后 Linear → **`Done`**,并删除 worktree
> - 一个 **Reviewer** 串行 merge;每次 merge 后跑 `cargo build --all-targets && cargo test --all-targets`(+ 差分测试 `WHI-522` 落地后)才放行下游。
> - **M2–M4 是 `needs-triage`**:开工前先把摘要展开成 canonical spec 再转 ready。
> - **全局策略:`max_hops = 3`**(依据 `ARB_PATHS_MANTLE.md`:2–3 跳=93.5%)。所有 service `MAX_HOPS`、pathfinder 默认、manifest 均为 3(现 V3=3,V2/Moe=4,pathfinder=4,待收敛)。
> - 🔴 = 只有你(人工)能做;🚦 = 人工门禁,过不了后面全停。

**同时开几个 agent**:M0 阶段 3 个 + 你;M1/M2/M3 阶段峰值 4 个 + 你。始终不超过 ~4 个实现 agent。按文件域固定分工避免冲突:
`A=execution` · `B=infra/build/bench` · `C1=amm-v3` · `C2=amm-moe` · `D=arbitrage/services` · `E=state-space` · `K=contracts` · `你=ops/门禁`。

> **合约"造一次、部署一次"**:`WHI-501` 只**实现 + 测试**最终合约(provenance + trade-local delta + 最终 minProfit + deadline),**不部署、不注资**(ready-for-agent)。首次部署在 Sepolia E2E(`WHI-525`,测试网);主网部署走 go-live 双人工门:`WHI-547`(部署+核验,**unfunded/paused**)→ `WHI-548`(限额注资 + 单笔 canary,第二次 go/no-go)。gas profile 依赖最终 codehash,故 **`501→546→502` 串行**。

---

## 🟢 Round 1 — 现在开始(开 2 个 agent + 你自己一条线)

- [ ] 🔴 **你** · `WHI-500` Drain & retire vulnerable executor  ▶完成解锁 `WHI-501`
- [ ] **B** · `WHI-505` Repair Cargo.toml & restore green build/test  ▶完成解锁 `WHI-510`
- [ ] **C2** · `WHI-507` Generate & validate a dedicated Moe pool list

## 🟢 Round 2 — 接着开工(前置勾掉即可开)

- [ ] **K** *(需 500)* · `WHI-501` **Build & verify** hardened replacement executor(**不部署、不注资**;ready-for-agent)  ▶解锁 `WHI-546`、`WHI-503`、`WHI-519`
- [ ] **B** *(接 505)* · `WHI-506` Pin Rust/Solidity/Foundry toolchain & repo init
- [ ] **E** *(需 505)* · `WHI-510` MarketSnapshot 一致性协议 ← **独占;M1 全部等它**  ▶解锁 `WHI-511…518`
- [ ] **B** *(需 505+506)* · `WHI-508` Remove tracked junk & confirmed dead code

## 🟢 Round 3 — M0 gas 链 + M1 扇出(并行;501/510 勾掉后)

**M0 gas 链(串行,合约在先):**
- [ ] **K/B** *(需 501)* · `WHI-546` Build measured gas-profile dataset(基于 501 最终 codehash)  ▶解锁 `WHI-502`
- [ ] **A** *(需 546)* · `WHI-502` Use measured gas profiles + live block fee context  ▶解锁 `WHI-515`、`WHI-519`
- [ ] **A** *(需 501)* · `WHI-503` Protect Moe principal(显式 `minProfit` + resized-input 完整重模拟)

**M1 扇出(需 510;每个 agent 列内串行):**
- [ ] **C1** · `WHI-512` Fix Agni tick coverage & verify/harden UniswapV3 → `WHI-518` Repair mutable V3 swap state
- [ ] **C2** *(需 507+510)* · `WHI-513` Build atomic, timestamped MoeSnapshot
- [ ] **D** · `WHI-511` Quote V3 from live pool state → `WHI-514` Cap optimal-input at balance → `WHI-515` Remove appearance suppression *(需 502)* → `WHI-517` Process current block N
- [ ] **E** · `WHI-516` Recover WS gaps with canonical header + log backfill

**✅ Gate M0**:500/501/502/503/505/506/507/508/546 merged;合约已 build+test(**未部署、未注资**);gas 链完成;build/test 全绿。
**✅ Gate M1**:510–518 merged;快照一致性 + 覆盖/`IncompleteState` 单测通过。

---

## 🟢 Round 4 — M2 执行/流程/差分测试(先 triage 展开 519–526,再开 4 个 agent)

> `WHI-523`(旧 per-hop min-out)已合并进 `WHI-501` 并标 Duplicate,不再单独做。

- [ ] **B(bench)** *(需 512/513/518)* · `WHI-522` Exact fixed-state AMM differential tests  ▶解锁 `WHI-528`
- [ ] **A** *(需 501/502/510/517)* · `WHI-519` Nonce-intent execution state machine  ▶解锁 `WHI-520/524/531`
- [ ] **A** *(接 519)* · `WHI-524` Authenticated circuit breakers + pending cancellation
- [ ] **D** *(需 510/519)* · `WHI-520` Version the Snapshot→Candidate→Preflight→Submit pipeline
- [ ] **D** *(接 520)* · `WHI-521` **Risk-tiered** exact-request semantic preflight(合格生产路径允许零 preflight RPC)
- [x] **A** *(需 502)* · `WHI-556` Parameterize fail-closed runtime gas-profile identity loading(**E2E profile-loader 前置**,非主网部署门禁)

## 🟢 Round 5 — Sepolia E2E(以上全绿后;合约首次部署到测试网)

- [ ] 🔴 **你 + B** *(需 503/506/511/514/515/516/520/521/522/524)* · `WHI-525` Build reusable Mantle Sepolia arbitrage E2E gate

## 🚦 Gate M2 — `WHI-526` P2.5 signerless shadow 门禁

- [ ] 🔴🚦 **你** *(需 525 + 上面全部)* · `WHI-526` Run the P2.5 signerless shadow validation gate
  **→ signerless shadow,按证据阈值 approve/reject。approve 才解锁 go-live 与 M3。**

---

## 🟢 Go-live 主网轨道(526 approve 后;真金白银,与 M3 并行)

> 用**已通过 shadow 的现有 entrypoints**上主网做限额 canary;合并后的新 binary 另需 `WHI-535` 才拿 signer。

**主网 gas 链(WHI-547 前置,`551→557` 串行):**
- [x] **K/A** *(需 501/526)* · `WHI-551` Derive & verify the immutable-patched executor runtime identity  ▶解锁 `WHI-557`
- [ ] **A/B** *(需 551)* · `WHI-557` Requalify the mainnet gas profile on a canonical Mantle fork  ▶解锁 `WHI-547`

- [ ] 🔴 **你** *(需 526/551/557)* · `WHI-547` Deploy & verify the **unfunded** production executor(部署 + 核验 role/codehash,结束时 **paused + unfunded**)  ▶解锁 `WHI-548`
- [ ] 🔴🚦 **你** *(需 547)* · `WHI-548` **第二次 go/no-go** → Fund & canary the verified executor(限额注资 + 单笔 canary)

## 🟢 Round 6 — M3 合并/可观测(526 approve 后,开 4 个 agent;与 go-live 并行)

- [ ] **Integrator** *(需 526)* · `WHI-527` Merge four services into one multi-protocol binary ← **独占**(先用冻结 legacy adapter)  ▶解锁 `WHI-529/532/536`
- [ ] **C1** *(需 522/526)* · `WHI-528` Share concentrated-liquidity core with UniV3
- [ ] **E** *(需 510/512/513/526)* · `WHI-530` Persist & verify integrity-checked checkpoints
- [ ] **E** *(需 510/526)* · `WHI-533` Implement full reorg unwinding without deep-reorg panic
- [ ] **A** *(需 519/526)* · `WHI-531` Reconcile canonical-finalized PnL with chain balances

## 🟢 Round 7 — M3 收尾(527 勾掉后)

- [ ] **D** *(接 527)* · `WHI-529` Configure settlement semantics + `max_hops=3` + remove invalid misprice paths
- [ ] **B** *(接 527)* · `WHI-532` Expose Prometheus pipeline & execution metrics  ▶解锁 `WHI-537`
- [ ] **B** *(需 507/526/527)* · `WHI-536` Generate a versioned static pool/path manifest(≤3 hop;原子替换冻结 adapter)

## 🚦 Gate M3 — `WHI-535` post-merge shadow 门禁(合并后的 binary 拿 signer 前)

- [ ] 🔴🚦 **你** *(需 527/529/531/532/536)* · `WHI-535` Requalify the merged binary in signerless shadow mode
  **→ replay 等价为主 + 风险定向 shadow。approve 才授予 merged binary 的 production signer。**
- [ ] **B** *(需 527/528/535)* · `WHI-534` Remove dead lib components superseded by the merge

---

## 🟢 Round 8 — M4 性能(数据驱动,可选;527 后开 3~4 个 agent)

- [ ] **B** *(需 527/532)* · `WHI-537` Measure p50/p95/p99 block-to-submit replay latency ← **先做,拿延迟数据**  ▶解锁 538–544
- [ ] **D** *(需 537)* · `WHI-538` Cache per-input quotes & re-optimize changing bounds
- [ ] **C2** *(需 522/537)* · `WHI-539` Wire tree_math for bin traversal
- [ ] **D** *(需 522/537)* · `WHI-540` Implement multi-peak optimal-input search
- [ ] **D** *(需 536/537)* · `WHI-543` Cache path pool-index mappings after graph construction
- [ ] **A** *(需 537)* · `WHI-541` Evaluate revm CacheDB for final-request simulation *(research)*
- [ ] **E** *(需 533/537)* · `WHI-544` Evaluate inverse deltas for the reorg buffer *(research)*

## ⚪ 随时可做(无依赖,门禁例外——不获得 signer、不改生产优先费)

- [ ] **B** · `WHI-542` Lower dev profile opt-level for faster iteration
- [ ] 🔴 **你** · `WHI-545` Verify Mantle sequencing / priority-fee / private-submission behavior *(research)*

---

## 关键路径(卡这条,整体就快)

```
正确性主干:  WHI-505 → WHI-510 → (WHI-512/513/518) → WHI-522 → 🚦WHI-526
合约/gas 主干: WHI-500 → WHI-501 → WHI-546 → WHI-502 → WHI-519 → WHI-520/521 → WHI-525 → 🚦WHI-526
主网 gas 链:   WHI-501 → 🚦WHI-526 → WHI-551 → WHI-557 → WHI-547
首笔主网:      🚦WHI-526 → WHI-551 → WHI-557 → WHI-547(部署+核验,unfunded) → 🚦WHI-548(注资+canary)
合并轨道:      🚦WHI-526 → WHI-527 → 🚦WHI-535 → merged-binary signer
```

> `WHI-523` 不在图内(已合并进 `WHI-501`)。go-live(551→557→547→548,现有 entrypoints)与 M3 合并(527→535,新 binary)是 526 之后的**两条并行轨道**,各自的 signer 授权互相独立。`WHI-556` 只是 E2E profile-loader 前置(接在 `WHI-502` 后),不在主网门禁链上。

## 只有你能做的 7 件事

- [ ] `WHI-500` 撤资 + 退役旧 executor
- [ ] `WHI-525` 驱动 Sepolia E2E(专用测试网 key,与 mainnet 强隔离)
- [ ] 🚦 `WHI-526` P2.5 shadow 门禁 approve/reject
- [ ] `WHI-547` 主网部署 + 核验 role/codehash(结束时 **unfunded/paused**;前置 `WHI-551`/`WHI-557`)
- [ ] 🚦 `WHI-548` 第二次 go/no-go → 限额注资 + 单笔 canary
- [ ] 🚦 `WHI-535` post-merge shadow 门禁(merged binary 拿 signer 前)
- [ ] production signer 保管——绝不交给自动化/定时脚本

> `WHI-501` 现在是 **ready-for-agent(build+test,不部署)**,不再是你的人工部署项。
> 本手册与 `specs/06` 依赖图同步;在 Linear 调依赖或增删 issue 后回来更新对应轮次。
