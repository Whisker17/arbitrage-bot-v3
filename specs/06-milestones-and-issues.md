# 06 · Milestone 与 Issue 规划（Linear）

> 本文件是**本地规划稿**，覆盖 spec 00–06 与 `TODOs.md`。M0/M1 提供 canonical-template 完整正文；M2–M4 默认是设计摘要。**已按用户指示写入 Linear**：这些摘要作为 `needs-triage` 占位 issue 创建（正文即设计摘要 + 顶部"展开后再实现"提示），用于承载依赖图和路线图；它们在展开成 `docs/agents/issue-template.md` 的完整 headings/metadata 并重新核对依赖与验收之前，**不得转出 `needs-triage`、不得交给 agent 执行**。例外是 M2-9/M2-10：它们是已展开的 canonical 人工上线 runbook，使用 `ready-for-human`。
>
> - **Team**：`Whisker-Personal`（key `WHI`）。本文用占位 id `M0-1`、`M1-2` … 做交叉引用。**占位 id 永久保留**（不回填替换）；占位 → 真实 `WHI-NN` 映射见 §6 第 5 条。
> - **语言**：按模板要求，**所有 issue 正文用英文**；本规划稿的说明用中文，issue body 用英文。
> - **来源映射**：每个 issue 标注它对应 spec 里的编号（A1–A5、R1–R8、B1–B9、C1–C3、D1–D5、F1–F15、性能 P1–P5），方便 GPT 对照 00–05 与 TODOs 核查覆盖度。

---

## 0. 约定回顾（自检用）

| 维度 | 取值 |
| --- | --- |
| 标题 | `[Mn] [Component] <imperative English>`，可加 `(scope/技术)` 后缀 |
| Component | `[amms]` `[state-space]` `[arbitrage]` `[execution]` `[Infra]` `[Contracts]` `[Bench]` `[Docs]` `[CI]`；协议限定可用 `[amms/agni]` `[amms/v3]` `[amms/moe]` |
| Priority | Urgent（挡 milestone/关键路径）/ High（milestone 核心交付）/ Medium（有价值不挡）/ Low（打磨） |
| Type label | `bug` / `feature` / `research` / `chore`。**Linear 实际标签大小写**：`bug`→`Bug`、`feature`→`Feature`（workspace-global 既有标签，大写）；`research` / `chore` 保持小写。自动化按此实际字符串查找，勿新建小写重复标签 |
| Triage label | `ready-for-agent`（完全 spec 化可交 AFK agent）/ `ready-for-human`（需人）/ `needs-triage` / `needs-info` / `wontfix`（不处理） |
| Body 段落 | Objective → Context(可选) → Blocked By / Blocks → Implementation → Out of scope(可选) → Acceptance criteria → Testing(可选) → References(可选) |
| 关系 | 用 Linear 原生 `blocks`/`blocked-by`；body 里镜像一份人读版 |

---

## 1. Milestone 总览

路线图直接对应 README 的 P0–P4。每个 milestone 一句 `Success = <可验证>`。

> **全局策略参数 · `max_hops = 3`**(依据 [`ARB_PATHS_MANTLE.md`](/Users/whisker/Work/src/personal/mantle-arbitrage-system/arbitrage-analysis/ARB_PATHS_MANTLE.md) §4:2–3 池占 **93.5%**,4+ 仅 6.5% 且利润可忽略)。**专注 2/3 跳**:所有 service 的 `MAX_HOPS`、库 `pathfinder` 默认 `max_length`、以及 manifest 生成的 settlement cycle 上限统一为 **3**(当前 V3=3、V2/Moe=4、pathfinder 默认=4,需收敛)。>3 跳不做,长尾利润不值优化。将来若 LST(mETH/cmETH/WETH)环路变厚,再考虑加 WETH 次级 settlement asset,与本约束正交。

| Milestone | Theme | Success（可验证） |
| --- | --- | --- |
| **M0** | Stop the Bleeding + Restore Buildability | 旧 executor 所有 callback 可窃取 ERC-20 已提走、native MNT 已核对并处置；feature-complete replacement executor 的源码、ABI、Rust 调用方与安全测试就绪但不部署；全量 build/test 绿；最终 replacement codehash 的 measured gas profile + 当前 block fee context 可在 fork 使用 |
| **M1** | State Freshness + Snapshot Consistency | 每一次报价都基于单一 canonical snapshot（完整 identity/header、V3/Moe coverage 与时间上下文、无落后一块、number/hash/parent 分叉检测）；仓位受 executor 余额约束 |
| **M2** | Execution State Machine + Versioned Pipeline + Diff Tests | 版本化 `MarketSnapshot→Candidate→Preflight→Submit`；执行与确认解耦、替换/取消；差分测试、断路器和可复用 Mantle Sepolia E2E 就绪；P2.5 影子批准后依次通过“主网部署/核验但不注资”和“二次批准后限额注资/canary”两个人工 issue |
| **M3** | Consolidation + Observability | 版本化静态 pool/path manifest；单一多协议 binary；PnL/指标/checkpoint 可验证；merged binary 通过独立 signerless shadow gate |
| **M4** | Data-Driven Performance | 测得 p50/p95/p99「块→发单」延迟；所有性能优化由 benchmark 数据驱动 |

### Milestone 描述（写进 Linear milestone 的 description）

**M0: Stop the Bleeding + Restore Buildability**
The bot is currently unsafe and its gas constants are incompatible with the evidence block. This milestone drains every callback-exposed ERC-20 from the old executor, accounts for any native MNT that the old ABI cannot withdraw, restores a green build/test, and delivers a feature-complete but undeployed replacement executor plus reproducible measured profiles for its final runtime bytecode. The 60M/50-gwei values are observations, not new constants. `Success = the old address is permanently retired; all withdrawable ERC-20 balances are zero and native MNT is explicitly accounted for; the replacement source, ABI, Rust callers, security suite, and final-codehash gas profile are ready without any network deployment or funding; cargo build/test pass.`

**M1: State Freshness + Snapshot Consistency**
Even when it runs, the bot quotes off stale/startup state and inconsistent reads. This milestone guarantees every quote derives from one canonical snapshot identified by chain/number/hash and carrying parent/timestamp: live pool state, V3/Moe coverage invariants, no deliberate one-block lag, and complete number/hash/parent fork detection. `Success = every candidate input comes from one block hash and header timestamp; uncovered tick/bin state returns IncompleteState, never a silent partial quote.`

**M2: Execution State Machine + Versioned Pipeline + Differential Tests**
Execution today is a serial queue that blocks on confirmation and can lose money on Moe. This milestone builds a versioned pipeline and execution state machine with risk-tiered exact-request preflight, same-nonce replace/cancel, differential tests and circuit breakers. A reusable, credential-isolated Mantle Sepolia environment deploys the replacement to testnet and proves one triggered arbitrage through canonical finality before the signerless P2.5 shadow gate. An approved shadow outcome first unblocks a human mainnet deploy-and-verify operation that must end paused and unfunded; its evidence then gates a separate human funding-and-canary decision. `Success = differential tests pass for V2 rounding / both V3 implementations / Moe cross-bin; execution no longer blocks on watch(); stale candidates and expired transactions cannot execute; the Sepolia E2E reconciles settlement and gas; shadow mode produces no sends; mainnet deployment, funding, and canary remain explicit post-approval operations with an unfunded verification checkpoint.`

**M3: Consolidation + Observability**
After the P2.5 shadow gate, one multi-protocol binary can first merge V2/V3/Moe behind a validated, startup-frozen legacy universe adapter. A reviewed static pool/path manifest then replaces that adapter before post-merge signer qualification. PnL, metrics, and integrity-checked checkpoints make the binary observable and recoverable; the separate post-merge signerless gate prevents the rewrite from inheriting old approval. `Success = deterministic manifest promotion is atomic; one binary runs all protocols concurrently; realized PnL reconciles to canonical-finalized chain state; metrics expose latency/hit-rate/balance/gas; a verified checkpoint restores without publishing untrusted state; replay equivalence plus targeted signerless evidence receives human approval before signer access.`

**M4: Data-Driven Performance**
Only after correctness and observability: measure the block→submit latency distribution, then spend effort where the data says. `Success = a replay benchmark reports p50/p95/p99 latency; each optimization issue is justified by a measured win.`

---

## 2. Issue 索引（at-a-glance）

> 优先级：🔴 Urgent · 🟠 High · 🟡 Medium · ⚪ Low。M0/M1 完整正文在无显式 human 标记时可评为 `ready-for-agent`。M2–M4 除 M2-9/M2-10 外都是摘要，在展开并复核前一律为 `needs-triage`；表内 `target: ready-for-human` 只表示展开后的目标标签，不覆盖当前状态。M2-9/M2-10 已是 canonical `ready-for-human`。

### M0 — Stop the Bleeding + Restore Buildability
| id | 标题 | Pri | Labels | Blocked by | Spec |
| --- | --- | --- | --- | --- | --- |
| M0-1 | `[M0] [Contracts] Drain and permanently retire the vulnerable executor` | 🔴 | bug, ready-for-human | — | C1 |
| M0-8 | `[M0] [Contracts] Build and verify the hardened replacement executor` | 🔴 | bug, ready-for-agent | M0-1 | C1, C2, R5 |
| M0-9 | `[M0] [Bench] Build a measured Mantle arbitrage gas-profile dataset and generator` | 🔴 | research, ready-for-agent | M0-8 | R1, B8, F10 |
| M0-2 | `[M0] [execution] Use measured gas profiles and live block fee context` | 🔴 | bug, ready-for-agent | M0-9 | R1, B8, B9, F10 |
| M0-3 | `[M0] [execution] Protect Moe principal on every resized execution` | 🔴 | bug | M0-8 | R5 |
| M0-4 | `[M0] [Infra] Repair Cargo.toml example targets and restore green build/test` | 🔴 | chore | — | A1, A3, overflow |
| M0-5 | `[M0] [Infra] Pin the Rust/Solidity/Foundry toolchain and repair dependency initialization` | 🟠 | chore | — | A2, A5, repro |
| M0-6 | `[M0] [Infra] Generate and validate a dedicated Moe pool list` | 🟠 | chore | — | A4 |
| M0-7 | `[M0] [Infra] Remove tracked junk and confirmed dead code (safe deletions)` | 🟡 | chore | M0-4, M0-5 | 05 A/B |

### M1 — State Freshness + Snapshot Consistency
| id | 标题 | Pri | Labels | Blocked by | Spec |
| --- | --- | --- | --- | --- | --- |
| M1-1 | `[M1] [state-space] Implement the MarketSnapshot consistency and readiness protocol` | 🔴 | feature | M0-4 | D1, D3, F7, README P1 |
| M1-2 | `[M1] [arbitrage] Quote V3 from live pool state and wire the inverted index` | 🔴 | bug | M1-1 | R2 |
| M1-3 | `[M1] [amms/v3] Fix Agni tick coverage and verify/harden UniswapV3 coverage` | 🔴 | bug | M1-1 | R3 |
| M1-4 | `[M1] [amms/moe] Build an atomic, timestamped MoeSnapshot with explicit coverage` | 🔴 | bug | M0-6, M1-1 | R6, B4-B6, D5 |
| M1-5 | `[M1] [arbitrage] Cap optimal-input search at executor WMNT balance` | 🟠 | bug | M1-1 | B1 |
| M1-6 | `[M1] [arbitrage] Remove appearance-count suppression while preserving execution gates` | 🟠 | bug | M0-2, M1-1 | R7 |
| M1-7 | `[M1] [state-space] Recover WS gaps with canonical header and log backfill` | 🟠 | feature | M1-1 | D3, D4, F7 |
| M1-8 | `[M1] [execution] Process the current block N (remove deliberate N-1 lag)` | 🟠 | bug | M1-1 | R4 |
| M1-9 | `[M1] [amms/agni] Repair mutable V3 swap state transitions` | 🔴 | bug | M1-1 | B7 |

### M2 — Execution State Machine + Versioned Pipeline + Diff Tests
| id | 标题 | Pri | Labels | Blocked by | Spec |
| --- | --- | --- | --- | --- | --- |
| M2-1 | `[M2] [execution] Implement the nonce-intent execution state machine` | 🔴 | feature | M0-2, M0-8, M1-1, M1-8 | R8, C3, F5, F6 |
| M2-2 | `[M2] [arbitrage] Version the MarketSnapshot→Candidate→Preflight→Submit pipeline` | 🔴 | feature | M1-1, M2-1 | README P2, 02 Exec |
| M2-3 | `[M2] [execution] Apply risk-tiered exact-request semantic preflight` | 🟠 | feature | M2-2 | F1, C2 |
| M2-4 | `[M2] [Bench] Add exact fixed-state AMM differential tests` | 🟠 | feature | M1-3, M1-4, M1-9 | R3, R6, B6, B7, README P2 |
| M2-6 | `[M2] [execution] Add authenticated circuit breakers and pending cancellation` | 🟠 | feature | M2-1 | F2 |
| M2-7 | `[M2] [Infra] Build a reusable Mantle Sepolia arbitrage E2E gate` | 🟠 | feature; target: ready-for-human | M0-3, M0-5, M1-2, M1-5, M1-6, M1-7, M2-2, M2-3, M2-4, M2-6 | F15, TODOs |
| M2-8 | `[M2] [execution] Run the P2.5 signerless shadow validation gate` | 🟠 | feature; target: ready-for-human | M0-3, M1-2, M1-5, M1-6, M1-7, M2-2, M2-3, M2-4, M2-6, M2-7 | F4 |
| M2-9 | `[M2] [Contracts] Deploy and verify the unfunded production executor` | 🔴 | chore, ready-for-human | M2-8 (approve) | production deploy |
| M2-10 | `[M2] [Contracts] Fund and canary the verified production executor` | 🔴 | chore, ready-for-human | M2-9 + second go/no-go | production go-live |

### M3 — Consolidation + Observability
| id | 标题 | Pri | Labels | Blocked by | Spec |
| --- | --- | --- | --- | --- | --- |
| M3-1 | `[M3] [Infra] Merge four services into one multi-protocol binary (concurrent V2/V3/Moe)` | 🔴 | feature | M2-8 | 02 |
| M3-2 | `[M3] [amms/agni] Share concentrated-liquidity core with UniV3, keep Agni adapters` | 🟠 | chore | M2-4, M2-8 | B7 |
| M3-3 | `[M3] [arbitrage] Configure settlement semantics and remove invalid misprice paths` | 🟠 | feature | M3-1 | B3, B9, 02 Engine |
| M3-4 | `[M3] [state-space] Persist and verify integrity-checked checkpoints` | 🟠 | feature | M1-1, M1-3, M1-4, M2-8 | F9 |
| M3-5 | `[M3] [Infra] Reconcile canonical-finalized PnL with chain balances` | 🟠 | feature | M2-1, M2-8 | F3 |
| M3-6 | `[M3] [Infra] Expose Prometheus pipeline and execution metrics` | 🟡 | feature | M3-1 | F8 |
| M3-7 | `[M3] [state-space] Implement full reorg unwinding without deep-reorg panic` | 🟡 | feature | M1-1, M2-8 | D1, D2 |
| M3-8 | `[M3] [Infra] Remove dead lib components superseded by the merge` | 🟡 | chore | M3-1, M3-2, M3-9 | 05 B1-B5 |
| M3-9 | `[M3] [execution] Requalify the merged binary in signerless shadow mode` | 🔴 | feature; target: ready-for-human | M3-1, M3-3, M3-5, M3-6, M3-10 | F4, README P3 |
| M3-10 | `[M3] [Infra] Generate a versioned static pool and path manifest` | 🟠 | feature; target: ready-for-human | M0-6, M2-8, M3-1 | A4, F14, TODOs, 02 pool universe |

### M4 — Data-Driven Performance
| id | 标题 | Pri | Labels | Blocked by | Spec |
| --- | --- | --- | --- | --- | --- |
| M4-1 | `[M4] [Bench] Measure p50/p95/p99 block-to-submit replay latency` | 🟠 | feature | M3-1, M3-6 | 03 |
| M4-2 | `[M4] [arbitrage] Cache per-input quotes and re-optimize changing bounds` | 🟡 | feature | M4-1 | P1 |
| M4-3 | `[M4] [amms/moe] Wire tree_math for bin traversal (replace ±1 linear scan)` | 🟡 | feature | M2-4, M4-1 | P4 |
| M4-4 | `[M4] [arbitrage] Implement multi-peak optimal-input search` | 🟡 | feature | M2-4, M4-1 | B2, P3 |
| M4-5 | `[M4] [execution] Evaluate revm CacheDB for final-request simulation` | ⚪ | research | M4-1 | 03 |
| M4-6 | `[M4] [Infra] Lower dev profile opt-level for faster iteration` | ⚪ | chore | — | 03 |
| M4-7 | `[M4] [arbitrage] Cache path pool-index mappings after graph construction` | ⚪ | feature | M3-10, M4-1 | P2 |
| M4-8 | `[M4] [state-space] Evaluate inverse deltas for the reorg buffer` | ⚪ | research | M3-7, M4-1 | P5 |
| M4-9 | `[M4] [Docs] Verify Mantle sequencing, priority-fee, and private-submission behavior` | 🟡 | research; target: ready-for-human | — | F12 |

覆盖自检：R1–R8、A1–A5、B1–B9、C1–C3、D1–D5 均有修复或清理落点（C1 的人工撤资/退役为 M0-1，replacement executor 为 M0-8）；F1–F10、F12、F14、F15 有明确 issue。F11（新增 DEX/更长路径）和 F13（flash loan）明确推迟到现有三协议能在 P2.5 证明可信机会之后，不把 M3-1 的“并发启用现有协议”冒充为 F11。性能 P1–P5 分别落到 M4-2/M4-7/M4-4/M4-3/M4-8。

---

## 3. 完整 Issue Body（英文，模板对齐）

> 以下 M0/M1 正文已粘入 Linear（占位 id 永久保留，真实 id 见 §6 第 5 条）；M2–M4 默认在第 4 节保留设计摘要并作为 `needs-triage` 占位 issue 创建。M2-9/M2-10 在第 4 节内单独提供完整 canonical 正文并已标 `ready-for-human`。

### M0-1 · `[M0] [Contracts] Drain and permanently retire the vulnerable executor`
**Milestone** M0 · **Priority** Urgent · **Labels** `bug`, `ready-for-human`

```markdown
## Objective
Remove funds from the vulnerable executor before an arbitrary contract can drain them,
account explicitly for native MNT the old ABI cannot withdraw, and permanently retire the
old address without waiting for the replacement implementation.

## Context
`contracts/executor/ArbitrageExecutor.sol:109` (`agniSwapCallback`) guards only with
`require(msg.sender != tx.origin)` (:115). Because the callback is reachable without going
through `onlyOwner executeArbitrage`, the attack does not need the owner. Stopping local
senders does not close this entry point. The old contract has token withdrawals at :363/:373
and a payable `receive()`
at :383, but no native-MNT withdrawal. Therefore "executor balance is zero" is not a valid
blanket acceptance claim: native MNT can be stranded and must be reported explicitly.

## Blocked By
None (entry point) — this is the first action of the whole plan.

## Blocks
- M0-8 — replacement implementation and artifact qualification may proceed only after retirement is irrevocable.

## Implementation
1. **Emergency sweep (ops, do first):** stop every V2/V3/Moe sender, but do not mistake that
   for an on-chain pause—the callback remains externally exploitable. Immediately withdraw
   known high-value tokens (especially WMNT). In parallel, complete the ERC-20 inventory from
   funding records, historical path tokens, and Transfer logs; sweep every additional token,
   verify balances, and create/update `tech-docs/deployments.md` with all transaction hashes.
2. Query the old address's native MNT balance separately. If it is non-zero, record it as
   stranded (the old ABI cannot withdraw it) and permanently retire the address. Never use
   "all balances are zero" to hide this case.
3. Mark the old address retired in deployment/config records and remove it from every sender
   environment. Do not send it more ERC-20 or use it for any protocol at any later stage.

## Out of scope
- Flash-loan sourcing of inventory (tracked as a later feature).
- Replacement contract implementation/artifact qualification (M0-8), deployment (M2-7/M2-9),
  and production funding/canary (M2-10).

## Acceptance criteria
- [ ] All callback-exposed ERC-20s in the documented inventory are withdrawn and verify zero;
      the old native-MNT balance is either zero or recorded as stranded with the address retired.
- [ ] Every sender is disabled or points to no executor; repository/deployment records mark the
      old address permanently retired and prohibit re-funding it.
- [ ] Withdrawal and balance evidence (including native MNT) is recorded with chain/tx ids.

## Testing / Verification
Reconcile the ERC-20 inventory and native-MNT balance from chain state; independently verify
the recorded withdrawal receipts and that no runtime configuration can send through the old address.

## References
- spec `specs/01-current-issues.md` §C1; `ArbitrageExecutor.sol:109-170`.
```

### M0-8 · `[M0] [Contracts] Build and verify the hardened replacement executor`
**Milestone** M0 · **Priority** Urgent · **Labels** `bug`, `ready-for-agent`

```markdown
## Objective
Deliver a feature-complete, fully Foundry-tested replacement executor and migrate every active
Rust caller to its final ABI. The contract authenticates callbacks and route pools, isolates
trade-local balances, enforces a protocol-independent final profit plus an on-chain deadline,
uses safe token transfers, and separates cold administration from hot execution. This issue
performs no testnet/mainnet deployment and moves no funds.

## Context
The retired contract's `agniSwapCallback` trusts any contract exposing `token0/token1`, while
`executeArbitrage` validates array lengths but not settlement endpoints, ordered token pairs,
or pool provenance. It also overloads `_amountsOut[last]` as its final principal check, uses
the whole intermediate-token balance as the next input, assumes ERC-20 `transfer` returns a
boolean, and spends gas comparing `_expectedStates` on chain. Once provenance, trade-local
accounting and `balanceAfter >= balanceBefore + minProfit` are enforced, exact reserve/slot/bin
equality adds gas/calldata and false rejects but no additional principal protection.

## Blocked By
- M0-1 — the vulnerable address must be drained and permanently retired first.

## Blocks
- M0-3 — Moe principal protection lands against the replacement execution surface.
- M0-9 — final gas-profile qualification must use this exact runtime bytecode and ABI.
- M2-1 — the execution state machine signs only the final deadline-bearing ABI.

## Implementation
1. Define a cold `admin`, one or more revocable hot executors, and an optional pause-only
   guardian. Hot executors may call only `executeArbitrage`; only the cold admin may manage
   executors/venue trust, unpause, and withdraw ERC-20/native MNT. A guardian may pause but not
   unpause or move funds. Do not give the production signing key withdrawal authority.
2. Authenticate every supported callback with that venue's actual selector and explicit
   `(factory, init-code-hash, pool key)` semantics. Recompute the canonical caller from callback
   data or use a cold-admin-controlled canonical allowlist where address derivation is not
   available. Unsupported callback/fork variants fail closed; never trust caller token getters.
3. In both the Rust final-calldata builder and the contract, enforce `path[0] == WMNT`,
   `path[last] == WMNT`, each ordered token direction, the registered venue/pool type, and pool
   provenance. A caller-supplied `poolType` cannot select an interface inconsistent with the
   registered venue.
4. Replace the ABI's `_expectedStates` and overloaded final `_amountsOut` check with explicit
   `minProfit` and `deadline`. Delete `_validateStates`; require `block.timestamp <= deadline`
   before the first external call and `balanceAfter >= balanceBefore + minProfit` at the end.
   Do not add a generic per-hop min-out array. Keep venue-native bounds only where the execution
   interface requires them or their semantics are independently proven. For canonical pools in an
   atomic transaction, provenance + trade-local deltas + final `minProfit` preserve inventory;
   a per-hop bound is an optional early-abort/gas optimization, not a separate principal invariant.
5. Derive each next-hop input from this transaction's received balance delta or a verified
   protocol return value, never total executor balance. Preserve all pre-trade intermediate
   balances. Route output through the executor for the correctness baseline; direct-to-next-pool
   routing remains a measured later optimization.
6. Use one audited safe-transfer helper for swaps and withdrawals that accepts either a true
   return value or empty return data and rejects false/malformed returns. Do not add router
   approvals, generic `call`/`delegatecall`, per-transaction native wrap funding, flash loans, or
   packed-calldata/address-book machinery in this issue.
7. Regenerate the ABI and migrate V2, both V3 services, Moe, tests and scripts to the final
   request shape. Fix deployment-script credential naming so documentation and code use the
   same environment variable, but do not broadcast the script.
8. Add Foundry unit/fuzz tests for callback forgery and every supported venue; malformed paths;
   provenance/direction/type mismatches; expired deadlines; zero/positive `minProfit`; profitable
   state drift succeeding without `_validateStates`; adverse drift reverting atomically; dust
   preservation; no-return/false-return tokens; role separation; pause semantics; and native/ERC-20
   recovery. Include an intermediate-hop adverse-move fixture that fails only at final `minProfit`
   and proves every token balance rolls back except sender gas, plus a cross-hop redistribution
   fixture that still clears final profit and must not be falsely rejected by a stale hop bound.
   Fork-replay the old callback drain against the new bytecode.
9. Produce reproducible optimized runtime bytecode/codehash plus ABI artifacts for M0-9 and M2-7.
   Do not deploy to any network and do not transfer inventory in this issue.

## Out of scope
- Testnet deployment (M2-7), mainnet deploy/verify (M2-9), and funding/canary (M2-10).
- Flash-loan sourcing (deferred F13).
- Per-transaction `msg.value` funding, multi-EOA scheduling policy, chain-adaptive sizing,
  calldata golf, and direct-to-next-pool routing.

## Acceptance criteria
- [ ] Unauthorized/forged callback callers and non-canonical path pools revert for every
      protocol; canonical pools and well-formed WMNT cycles pass.
- [ ] Non-WMNT endpoints and mismatched ordered token directions revert in Rust and Solidity tests.
- [ ] The ABI has explicit `minProfit` and `deadline`, has no `_expectedStates`, and the runtime
      contains no reserve/slot/bin equality precheck. Profitable state drift can execute, while
      an under-minimum final balance reverts atomically.
- [ ] A fixture preloads intermediate-token dust; execution consumes only this trade's hop delta,
      leaves the preloaded balance unchanged, and feeds the next hop exactly the prior hop output.
- [ ] An adverse intermediate hop followed by final-profit failure atomically restores all executor
      token balances; a route whose per-hop outputs drift but whose final delta still clears
      `minProfit` succeeds. No generic per-hop min-out array is required for principal safety.
- [ ] Empty-return ERC-20 transfers succeed; false/malformed returns revert. Hot executors cannot
      withdraw, change trust, or unpause; cold admin/guardian permissions match the specified model.
- [ ] All active Rust callers compile against the final ABI and the security/fork suite is green.
      No network deployment or funding occurs; the old address remains retired and unfunded.

## Testing / Verification
Run the security-focused Foundry unit/fuzz/fork suite, ABI drift check, every active Rust caller's
targeted build/tests, and deterministic optimized bytecode generation. Verify no broadcast or
funding artifact is produced.

## References
- spec `specs/01-current-issues.md` §C1/C2/R5; `ArbitrageExecutor.sol:109-242,363-383`.
- `CONTRACT_REVIEW.md` S1-S7, with Base funding/signer observations treated as non-blocking research.
```

### M0-9 · `[M0] [Bench] Build a measured Mantle arbitrage gas-profile dataset and generator`
**Milestone** M0 · **Priority** Urgent · **Labels** `research`, `ready-for-agent`

```markdown
## Objective
Produce a reproducible evidence dataset and versioned generated gas profiles for every
production arbitrage route class, so M0-2 can choose a safe gas limit and a realistic
gas-cost estimate with an in-memory lookup and no `eth_estimateGas` or `eth_call` in the
latency-critical gas-sizing path.

## Context
The current 300M–2.8B hop constants exceed the observed Mantle 60M block gas limit. A
2026-07-19 preliminary read-only sample found 50 gwei `baseFeePerGas` and a 60M block gas
limit in 101 consecutive recent blocks and 21 points spaced across blocks
97,158,262–98,158,262. That supports current stability but is not evidence of a permanent
protocol constant.

Average gas used times a fixed percentage is not a safe limit rule. V2 is relatively stable
by hop count, while V3 tick crossings and Moe bin crossings create long-tail and multi-modal
execution cost. The profile must also keep the execution `gas_limit` separate from
`expected_gas_used` used for profitability.

## Blocked By
- M0-8 / WHI-501 — the generator may be developed in parallel, but profile qualification and
  completion require the final replacement ABI and optimized runtime codehash.

## Blocks
- M0-2 / WHI-502 — runtime integration requires an approved generated profile and schema.

## Implementation
1. Add a shared versioned profile schema in `src/execution/gas_profile.rs`, a reproducible
   generator at `examples/generate_gas_profile.rs`, and generated artifacts under
   `config/gas_profiles/`.
2. Collect canonical successful receipts from historical executor runtime code hashes as research
   inputs, but never qualify them for the replacement. Decode
   each ordered route and record chain/block identity, ordered protocols, hop count, actual
   `gasUsed`, effective gas price, base fee, block gas limit, inclusion latency, and executor
   code hash. Keep revert observations separately; do not mix partial revert gas into
   successful-route limits.
3. Qualify every supported route class with hash-pinned fork/replay fixtures using M0-8's exact
   optimized replacement bytecode and final calldata (no `_expectedStates`, explicit `minProfit`
   and `deadline`). At minimum distinguish ordered protocol sequence and hop count;
   V3 and Moe profiles must additionally bucket tick and bin crossings when those change the
   distribution materially.
4. For each profile key publish sample count, min, p50, p95, p99, max, holdout result,
   `expected_gas_used`, and a conservative `gas_limit` policy. Derive the limit from tail/maximum
   evidence plus an explicit margin, not average times 1.2. Reject profiles whose required limit
   cannot remain below the observed minimum block gas limit.
5. Analyze base fee, block gas limit, effective priority fee, and inclusion behavior over a dated
   block range. Record stable observations without promoting 50 gwei or 60M to permanent constants.
6. Measure the replacement's callback provenance, safe-transfer, role, deadline and final-profit
   overhead, plus gas removed with `_validateStates`; do not import the Base trace's 1.24M sample
   as Mantle evidence without its complete transaction/trace artifact.
7. Make generation deterministic. The artifact includes chain id, start/end block number and hash,
   executor code hash, schema/tool version, route-key definition, sampling policy, margin policy,
   sample counts, and a content digest. Identical inputs must produce an identical artifact.

## Out of scope
- Runtime consumption and send-path migration, owned by M0-2 / WHI-502.
- Profit-aware priority bidding or private submission policy, owned by M4-9 and later tuning.
- Semantic transaction preflight, owned by M2-3.

## Acceptance criteria
- [ ] One command regenerates the checked artifact from pinned inputs and identical inputs
      produce an identical digest.
- [ ] Every active production route class has an evidence-backed profile or an explicit
      unsupported result; no unknown class silently receives a generic limit.
- [ ] Every approved profile names M0-8's final optimized runtime codehash and ABI digest;
      historical/old-executor samples are research-only and cannot qualify production.
- [ ] Each profile reports sample count, p50/p95/p99/max, holdout coverage, separate
      `expected_gas_used` and `gas_limit`, and the exact margin policy.
- [ ] Holdout plus fork/replay tests demonstrate that every supported route completes below its
      proposed limit and below the observed minimum block gas limit.
- [ ] V3 tick-crossing and Moe bin-crossing effects are either represented in the key or
      disproved with recorded distribution evidence.
- [ ] The fee analysis records dated base-fee/block-limit/priority/inclusion observations and
      explicitly states that current stability is not a permanent constant.

## Testing / Verification
Run `cargo test gas_profile`, regenerate with
`cargo run --example generate_gas_profile -- --config <pinned-config>`, compare the digest
across two runs, and execute the holdout/fork replay suite against the generated artifact.

## References
- spec §R1, §B8, §F10; `execution/gas_schedule.rs`, `execution/executor.rs`.
- Linear WHI-546 and WHI-502.
```

### M0-2 · `[M0] [execution] Use measured gas profiles and live block fee context`
**Milestone** M0 · **Priority** Urgent · **Labels** `bug`, `ready-for-agent`

```markdown
## Objective
Replace invalid gas constants with an approved, versioned measured profile and the current
block fee context. Every production gas-sizing path must select a safe `gas_limit` and realistic
`expected_gas_used` with an O(1) in-memory lookup; it must not call `eth_estimateGas` or
`eth_call` for gas sizing on the latency-critical path.

## Context
The current code hardcodes 300M–2.8B gas limits, a 0.02 gwei base fee, and a 0.5 gwei fee cap
for small profits. These values are incompatible with the observed Mantle 60M block gas limit
and 50 gwei base fee. M0-9/WHI-546 owns the reproducible analysis and generated profile artifact;
this issue consumes that evidence rather than inventing another manual hop table.

Current fee stability is not a permanent protocol guarantee. Base fee and block gas limit remain
runtime block-header inputs, but reading data already captured at block ingress adds no
per-candidate RPC round trip. Risk-tiered exact-request preflight remains a separate M2-3 policy
and is not gas sizing or a mandatory production hot-path RPC.

## Blocked By
- M0-9 / WHI-546 — supplies the approved schema, evidence dataset, generated profiles, and
  margin policy.

## Blocks
- M2-1 — the execution state machine consumes the fee plan and profile identity.
- M1-6 — candidate eligibility requires valid current fee/profile inputs after appearance
  suppression is removed.

## Implementation
1. Implement the runtime loader and lookup in `src/execution/gas_profile.rs` using the schema
   generated by M0-9. Load once at startup and expose a small interface that accepts executor
   code hash, ordered protocols, hop count, and the required V3 tick/Moe bin crossing buckets
   and returns `gas_limit`, `expected_gas_used`, and profile identity.
2. Validate chain id, executor runtime code hash, schema/tool version, artifact digest, route-key
   coverage, sample threshold, and margin policy before publishing the profile as Ready. Missing,
   unsupported, stale, or mismatched profiles fail closed; there is no generic or
   `eth_estimateGas` fallback on a production send path. Only M0-8's final optimized runtime
   codehash and ABI digest may qualify; an old executor profile is never compatible.
3. Introduce one cached `BlockFeeContext { block_number, block_hash, base_fee_per_gas,
   block_gas_limit }` at block ingress. Reuse the header already received by the block loop, or
   fetch the full header once at ingress if the provider supplied only a number. Never add a
   header RPC round trip per candidate or immediately before signing.
4. Set the transaction gas limit from the selected profile and reject it if it is zero or not
   strictly below the current block gas limit under the approved reserve policy. Compute
   profitability from the separate conservative `expected_gas_used`, not from the deliberately
   larger execution limit.
5. Compute EIP-1559 fee fields from the current cached base fee plus a conservative configurable
   priority policy. Recheck `max_fee_per_gas >= current base_fee` and that the fee context still
   matches the candidate block identity immediately before signing.
6. Remove `MANTLE_BASE_FEE_WEI`, the duplicate table in `arbitrage/gas.rs`, the invalid constants
   in `execution/gas_schedule.rs`, and hop special-casing in `compute_fee_plan`. Migrate V2, old
   V3, 1559 V3, and Moe, or explicitly remove an unpatched entry point from deployment and Cargo.
7. Keep risk-tiered semantic `eth_call` policy in M2-3. Do not call `eth_call` and
   `eth_estimateGas` serially as part of this gas module, and do not treat either as a fallback
   for an unknown profile.
8. Replace lossy `U256 → string → u128.unwrap_or(0)` fee/profit conversions with checked arithmetic
   and typed overflow errors. Emit profile key/version plus canonical receipt `gasUsed`; alert and
   fail profile qualification when observed use approaches the configured limit threshold.

## Out of scope
- Dataset collection, statistical analysis, and profile generation, owned by M0-9/WHI-546.
- Full exact-request semantic preflight and snapshot/head audit policy, owned by M2-3.
- Profit-aware priority bidding and private submission policy, owned by M4-9 and later tuning.

## Acceptance criteria
- [ ] Every active production send call site uses the same validated profile lookup and current
      `BlockFeeContext`, or the entry point is absent from deployment and Cargo targets.
- [ ] A test provider proves profile selection and fee construction make zero `eth_estimateGas`,
      zero gas-sizing `eth_call`, and zero per-candidate header RPC calls.
- [ ] Unknown route keys, executor code-hash mismatch, invalid digest/schema, insufficient
      evidence, and stale block identity all fail closed before signing.
- [ ] The loader rejects every profile whose executor codehash or ABI digest differs from
      M0-8's final undeployed build artifact.
- [ ] Submitted `gas_limit` comes from the approved profile and remains below the current block
      gas limit under the reserve policy.
- [ ] Profitability uses `expected_gas_used`, while the transaction uses the separate larger
      `gas_limit`; tests prove the two cannot be accidentally conflated.
- [ ] `max_fee_per_gas >= current base_fee` is rechecked immediately before signing and checked
      arithmetic reports overflow instead of converting it to zero.
- [ ] Canonical receipts record profile identity and actual `gasUsed`; a threshold breach raises
      an alert and invalidates production qualification until the profile is regenerated.
- [ ] A fork/testnet transaction built from the generated profile and live block fee context is
      accepted and mined.

## Testing / Verification
Run `cargo test gas_profile`, the execution fee/profile unit suite with a counting fake provider,
repository-wide production send-callsite checks, and one fork/testnet submission using the
checked M0-9 artifact. Confirm logs show the profile digest/key, block fee-context identity,
selected limit, expected gas used, and receipt gas used.

## References
- M0-9 / WHI-546.
- spec §R1, §B8, §B9; `execution/gas_schedule.rs`, `execution/executor.rs`, `arbitrage/gas.rs`.
```

### M0-3 · `[M0] [execution] Protect Moe principal on every resized execution`
**Milestone** M0 · **Priority** Urgent · **Labels** `bug`, `ready-for-agent`

```markdown
## Objective
Stop the Moe execution path from allowing the entire input to be lost: re-simulate whenever
the input is resized and pass an explicit positive `minProfit` into M0-8's protocol-independent
final balance gate.

## Context
`examples/protocols/moe/moe_monitor_executor_service.rs:1388` sets `amounts_out` all-zero,
and :1364/:1379 resize input to `executor_balance` without re-simulating. The retired ABI's
overloaded final check then degenerates to "input may be fully lost". M0-8 replaces it with
`balanceAfter >= balanceBefore + minProfit`; this issue makes Moe construct that final request
from a fresh simulation of the actual adjusted input. See spec 01-R5.

## Blocked By
- M0-8 — land against the hardened replacement executor.

## Blocks
- M2-7 — any selected Moe E2E route must inherit the same principal protection.
- M2-8 — the P2.5 gate cannot approve Moe execution before principal protection is exercised.

## Implementation
1. When shrinking input to `executor_balance`, re-run the complete path simulation with the new
   input and abort if it no longer clears current gas plus the configured minimum net profit.
2. Build M0-8's final ABI with an explicit positive `minProfit`; do not encode principal safety
   by mutating a protocol-specific `amountsOut[last]` value.
3. Keep every production sender disabled through the M2-8 human gate; M0-3 passing is necessary
   but does not independently authorize Moe sends.

## Acceptance criteria
- [ ] A test/replay whose final WMNT delta is below explicit `minProfit` causes M0-8's final
      balance gate to revert atomically, independent of hop protocol.
- [ ] Resized input is validated by re-simulation before submit; unprofitable resize aborts.

## References
- spec §R5; `moe_monitor_executor_service.rs:1360-1430`, `ArbitrageExecutor.sol:168-170`.
```

### M0-4 · `[M0] [Infra] Repair Cargo.toml example targets and restore green build/test`
**Milestone** M0 · **Priority** Urgent · **Labels** `chore`, `ready-for-agent`

```markdown
## Objective
Restore a green `cargo build --all-targets` and `cargo test --all-targets` by repointing the
9 broken `[[example]]` entries, fixing/removing every obsolete explicit or auto-discovered
target, and fixing the library test overflow.

## Context
`cargo build`/`check --lib` currently pass, but `--all-targets` and `cargo test` fail: 8
example paths moved `examples/test/ → examples/protocols/agni/` without updating
`Cargo.toml`, `fetch_failed_pools_ticks` was deleted but still declared, and
`src/amms/moe/math/uint256x256_math.rs:185` has a compile-time overflow `1_u64 << 200` in a
`#[test]`. See spec 01-A1, A3, and the round-2 overflow note.

## Blocked By
None (entry point).

## Blocks
- M0-7 — cleanup verification uses the restored all-target baseline.
- M1-1 — snapshot work builds on a green tree.

## Implementation
1. `Cargo.toml`: repoint the 8 example paths to `examples/protocols/agni/…`; delete the
   `fetch_failed_pools_ticks` declaration.
2. `uint256x256_math.rs:185`: change `1_u64 << 200` to `U256::from(1u64) << 200`.
3. Delete the compile-broken legacy examples `examples/subscribe.rs`,
   `examples/swap_calldata.rs`, and `examples/test/execute_sepolia_arbitrage.rs`; remove the
   explicit Cargo declaration where present. Root `examples/*.rs` require actual file deletion
   because Cargo auto-discovers them (spec 05-D).
4. Keep `tests/moe_swap.rs`: update obsolete Alloy/tracing/timestamp APIs so it compiles, split
   credentialed live-RPC cases behind an explicit `#[ignore]`/feature gate, and add a minimal
   offline deterministic Moe fixture covering the same core behavior. M2-4 may replace it only
   after broader fixed-state differential coverage lands; do not make CI green by deleting tests.
5. Confirm the four real services still `cargo check --example …`.

## Acceptance criteria
- [ ] `cargo build --all-targets` exits 0.
- [ ] `cargo test --all-targets` exits 0; live-RPC tests are explicitly gated/ignored with an
      offline deterministic counterpart rather than silently depending on local credentials.
- [ ] `cargo check --example v3_monitor_executor_service_1559` (and the other three) pass.

## References
- spec §A1, §A3, §05-D; `Cargo.toml`, `uint256x256_math.rs:185`.
```

### M0-5 · `[M0] [Infra] Pin the Rust/Solidity/Foundry toolchain and repair dependency initialization`
**Milestone** M0 · **Priority** High · **Labels** `chore`, `ready-for-agent`

```markdown
## Objective
Make both Rust and contract builds reproducible: commit `Cargo.lock`, initialize the
`forge-std` submodule, pin exact Rust/solc/Foundry versions, and clean up `.gitignore` rules
that leak build artifacts or confuse repository setup.

## Context
`.gitignore:2` ignores `Cargo.lock`, so `alloy="1.0.25"` currently resolves to `1.8.3` — a
production bot must pin. `build.rs` and Foundry config hardcode a host-specific solc path,
while the repository pins neither Rust nor Foundry/solc versions. `contracts/lib/forge-std`
is a registered gitlink (mode 160000) but
uninitialized; `git check-ignore` confirms the `.gitignore` rule does **not** block init, so
it just needs `submodule update --init`. See spec 01-A2, 05-C. `Cargo.lock` is NOT to be
deleted; `foundry.lock` must be kept.

## Blocked By
None.

## Blocks
- M0-7 — cleanup depends on the ignore/reproducibility boundary landing first.
- M2-7 — the live Sepolia gate must use the pinned, reproducible Rust/Foundry/Solidity toolchain.

## Implementation
1. Remove `Cargo.lock` from `.gitignore`; commit the current lockfile.
2. `git submodule update --init contracts/lib/forge-std`.
3. `.gitignore`: add explicit Git patterns `contracts/executor/out/`,
   `contracts/executor/cache/`, `contracts/executor/broadcast/`, `contracts/broadcast/`,
   `**/*.bak`, and `.DS_Store`; keep `foundry.lock` tracked; drop or narrow the
   `contracts/lib/` rule. Do not use shell brace expansion as if Git supported it.
4. Add `rust-toolchain.toml` with an exact Rust channel/version. Pin an exact solc version in
   Foundry config without a workstation-specific absolute path, and record/pin the exact
   Foundry release used by CI and developers.
5. CI prints and verifies `rustc --version`, `solc --version`, and `forge --version`; run
   locked Rust builds and fail if generated ABI/lockfile output drifts unexpectedly. Document
   that contract/ABI changes require `SKIP_FORGE=0`, and exercise that path in CI.

## Acceptance criteria
- [ ] `Cargo.lock` is tracked; `cargo build` uses the committed lock.
- [ ] `contracts/lib/forge-std` is populated; `forge build` resolves std.
- [ ] New executor build/deploy artifacts are ignored by the explicit patterns; M0-7 owns
      archiving and untracking artifacts already present in the index.
- [ ] Rust, solc, and Foundry exact versions are repository-visible and CI-verified; no
      `/opt/homebrew/bin/solc` dependency remains.

## References
- spec §A2, §05-C.
```

### M0-6 · `[M0] [Infra] Generate and validate a dedicated Moe pool list`
**Milestone** M0 · **Priority** High · **Labels** `chore`, `ready-for-agent`

```markdown
## Objective
Generate a dedicated `data/poolLists_moe.csv` from the configured canonical Moe factory and
make the Moe loader fail closed, so it never interprets an Agni list as Moe pools.

## Context
`moe_monitor_executor_service.rs:812` reads `data/poolLists_moe.csv` (missing) and warns-
falls-back to `poolLists.csv`, which holds Agni pools — wrong inputs for Moe. See spec 01-A4.

## Blocked By
None.

## Blocks
- M1-4 — Moe snapshot work needs real Moe pools.
- M3-10 — the cross-protocol manifest generator supersedes this dedicated stopgap list.

## Implementation
1. Add/reuse factory discovery to enumerate Moe LB pairs at a recorded canonical block and
   write `data/poolLists_moe.csv` with factory, pool, tokenX/tokenY, bin step, and creation block.
2. Validate every row against the configured Moe factory and token getters before loading.
3. Remove the silent Agni fallback; fail loudly if the file is absent, empty, duplicated, or
   contains a non-canonical/wrong-protocol pool. Cross-protocol schema unification is deferred
   to M3 config consolidation rather than chosen ad hoc in this issue.

## Acceptance criteria
- [ ] Moe service loads only Moe LB pairs (assert non-empty, correct protocol).
- [ ] No silent fallback to an Agni pool list.
- [ ] Regenerating from the same factory/block is deterministic and each row passes on-chain
      provenance/token-pair validation.

## References
- spec §A4; `moe_monitor_executor_service.rs:812`.
```

### M0-7 · `[M0] [Infra] Remove tracked junk and confirmed dead code (safe deletions)`
**Milestone** M0 · **Priority** Medium · **Labels** `chore`, `ready-for-agent`

```markdown
## Objective
Delete the zero-risk junk and confirmed dead code to shrink noise, without touching anything
gated on the architecture decision.

## Context
Spec 05 enumerates safe deletions. Exclusions the round-2/3 review pinned: keep `foundry.lock`,
`mock.rs`, `broadcast/` (archive first), and `pair_parameters::Parameters` (aliased as
`MoeParameters`, used at `moe/mod.rs:1492`). Big refactors (Agni merge, dead lib components)
are deferred to M3-2/M3-8.

## Blocked By
- M0-4 — establishes the target/test baseline used to prove deletions.
- M0-5 — installs ignore rules and preserves reproducibility artifacts before untracking junk.

## Blocks
None.

## Implementation
1. Delete `.DS_Store`, `*.sol_bak`, `src/amms/agni/mod.rs.bak`, the three `poolLists.csv.*bak`.
2. Untrack existing `contracts/executor/out/` and `contracts/executor/cache/` content after
   M0-5's ignore rules land; create/update `tech-docs/deployments.md` from `broadcast/`
   deployment history before untracking any broadcast content.
3. Remove clearly dead, zero-call-site helpers only where spec 05-B marks them confirmed and
   not gated on the merge (e.g. Moe `calc_*`, `price_liquidity`, `price_from_id`,
   `mul/shift_*`). Leave `NonceManager`, `StateSpaceManager` (M2/M1 reuse), and
   `MoeParameters` intact.

## Acceptance criteria
- [ ] Working tree free of `.bak`/`.DS_Store`; `git status` clean of build artifacts.
- [ ] `cargo build --all-targets` and `cargo test --all-targets` stay green after deletions;
      no deletion-only test is added merely to assert that removed symbols are absent.

## References
- spec `specs/05-cleanup.md` §A/§B.
```

### M1-1 · `[M1] [state-space] Implement the MarketSnapshot consistency and readiness protocol`
**Milestone** M1 · **Priority** Urgent · **Labels** `feature`, `ready-for-agent`

```markdown
## Objective
Introduce a `MarketSnapshot` that is a consistency protocol, not just a version tag: all
reads for one snapshot are pinned to one canonical block hash, carry full chain/header
identity, publish atomically, and expose explicit readiness so no consumer can keep quoting an
old snapshot during replacement, rollback, gap recovery, or a failed new-head sync.

## Context
README P1 and spec 02 "Sync module". A same-height replacement creates no number gap and may
retain the same parent, so it is detected by `(number, hash)` comparison; a child extending
the wrong branch is detected by `parent_hash`. Today `StateSpace.latest_block` is never
advanced (spec 01-D1), so reorg logic is dead.

## Blocked By
- M0-4 — build must be green.

## Blocks
- M1-2, M1-3, M1-4, M1-5, M1-6, M1-7, M1-8, M1-9, M2-1, M2-2, M3-4, M3-7 — all consume the snapshot contract.

## Implementation
1. Define `SnapshotId { chain_id, block_number, block_hash }` plus immutable header context
   containing `parent_hash` and `block_timestamp`; embed it in `MarketSnapshot` with pool
   state and protocol coverage metadata.
2. Pin state calls (`slot0`, tick, bin, balances) by EIP-1898 block hash with canonicality
   required where supported. Query logs with a block-hash filter, not a number-only range.
   If an RPC cannot hash-pin a required call, fix every call to the target block number, compare
   its canonical hash immediately before and after all reads, and reject if either differs.
   Never let the fallback's middle call read `latest`.
3. Accept normal progress only when `number == last.number + 1` and
   `parent_hash == last.hash`. Ignore an exact duplicate `(number, hash)` notification.
   A same-height different hash, any lower height, a forward child with the wrong parent, or
   a numeric gap stops quoting; gaps route to M1-7, branch changes route to resync/unwind.
4. Publish atomically: if any read, coverage sync, or identity validation fails, do not publish
   partial state. Never mix a header timestamp from one hash with state from another.
5. Expose `SnapshotStatus::Ready(Arc<MarketSnapshot>) | Syncing | Halted(reason)` (or an
   equivalent readiness token). On any new head, gap, fork, or read failure, leave `Ready`
   immediately. Retain the previous good snapshot only as a recovery baseline, never as an
   executable quote source while the canonical head is unresolved.

## Out of scope
- Full reorg unwinding via StateChangeCache (M3-7).
- Backfill of skipped block numbers (M1-7) — complementary, separate issue.

## Acceptance criteria
- [ ] For a replayed range, every candidate's inputs share one `block_hash` (assert in a test).
- [ ] A same-height different-hash notification and a next-height wrong-parent child each halt
      quoting and trigger resync; an exact duplicate notification is idempotent.
- [ ] Hash-pinned state/log tests prove one snapshot cannot mix two block hashes; the fallback
      path rejects a canonical-hash change during reads.
- [ ] A mid-snapshot read failure leaves the previously published snapshot intact.
- [ ] That same failure switches consumers out of `Ready`; candidate creation/sign/send stays
      disabled until a complete current canonical snapshot is atomically published.

## References
- spec §D1, README P1, 02 "Sync module".
```

### M1-2 · `[M1] [arbitrage] Quote V3 from live pool state and wire the inverted index`
**Milestone** M1 · **Priority** Urgent · **Labels** `bug`, `ready-for-agent`

```markdown
## Objective
Make V3 candidate discovery quote from the live, per-block-updated pool state instead of the
startup clone. Use the already-built inverted index so only affected paths rerun AMM math,
while every cached candidate still receives current fee/balance profitability treatment.

## Context
Both active V3 services simulate against their startup `path_cache.state_pools` (1559 :935,
old V3 :913); the 1559 inverted index `pool_to_path_indices` (:270) is built but never read.
Spec 01-R2.

## Blocked By
- M1-1 — quote from the consistent snapshot.

## Blocks
- M2-7 — the reusable live E2E must not exercise a startup-state V3 path.
- M2-8 — the shadow gate requires live V3 quotes, not startup state.

## Implementation
1. Change `find_profitable_candidates` to simulate against the live snapshot pools, not
   `path_cache.state_pools`.
2. Read `pool_to_path_indices` to select paths whose pools changed for gross AMM re-quote.
   On every block, refresh net profitability with the latest fee context even when no pool log
   arrived; if the balance bound changes the feasible input domain, re-run sizing. M4-2 may
   optimize this with a per-input quote curve, but M1 must remain correct without that cache.
3. Remove the now-unused startup clone or repurpose it as the snapshot base.
4. Apply the fix to both V3 services. If the old service is intentionally superseded, remove it
   from deployment and Cargo targets in this issue instead of leaving a stale active path.

## Acceptance criteria
- [ ] A price change on a pool in block N changes that path's quote in block N (test).
- [ ] Only paths touching changed pools rerun AMM math (assert count), but a base-fee change
      with no pool log can still make a cached opportunity ineligible in that block.
- [ ] No "field `pool_to_path_indices` is never read" warning remains.
- [ ] Repository-wide checks find no active V3 send/quote entrypoint using startup `state_pools`.

## References
- spec §R2; `v3_monitor_executor_service_1559.rs:270,323,920,935`;
  `v3_monitor_executor_service.rs:913`.
```

### M1-3 · `[M1] [amms/v3] Fix Agni tick coverage and verify/harden UniswapV3 coverage`
**Milestone** M1 · **Priority** Urgent · **Labels** `bug`, `ready-for-agent`

```markdown
## Objective
Fix Agni's confirmed tickless startup path, verify the existing UniswapV3 full-sync/mutable
behavior, and make every concentrated-liquidity simulation entry point return
`IncompleteState` rather than silently using zero liquidity outside proven coverage.

## Context
Agni `init_basic` clears `tick_bitmap`/`ticks` (`agni/mod.rs:397-398`), and its tick loop uses
`self.ticks.get(&tick_next).map_or(0, |i| i.liquidity_net)` (`agni/mod.rs:270-274`) — so an
`initialized == true` tick with a missing record crosses with `liquidity_net = 0`, a silent
wrong result. UniswapV3 is not the same confirmed startup/mutable bug: `init` calls bitmap/tick
sync (`uniswap_v3/mod.rs:582-607`) and `simulate_swap_mut` writes back sqrt price/tick/liquidity.
However, both UniV3 simulation loops also fall back to `liquidity_net=0` when bitmap says
initialized but the tick record is absent (`:363-368`, `:510-515`). Spec 01-R3 therefore
requires Agni repair plus UniV3 verification/hardening, not a rewrite of working UniV3 state flow.

## Blocked By
- M1-1.

## Blocks
- M2-4 — differential tests validate this.
- M3-4 — checkpoint restore must preserve V3 coverage metadata.

## Implementation
1. In Agni, sync the required tick data at startup (use existing `sync_tick_bitmaps`/
   `sync_tick_data`; stop publishing a tickless `init_basic` result).
2. For UniswapV3, first prove the existing full-range init and mutable state commit with
   deterministic fixtures; preserve those paths unless a fixture exposes a real defect.
3. Track synced bitmap-word coverage separately from tick records in both adapters so
   "synced and zero" is distinguishable from "not synced".
4. Apply the same invariant in Agni/UniswapV3 and in mutable/immutable swap paths: if
   `step.initialized` but the tick record is absent, or traversal leaves synced word coverage,
   return `AMMError::IncompleteState` instead of `map_or(0, …)` or a partial quote.
5. Callers treat `IncompleteState` as "skip this candidate", not a zero quote.

## Acceptance criteria
- [ ] Agni startup fixtures fail the old tickless path and quote correctly after sync.
- [ ] UniV3 fixtures prove existing full init and mutable post-swap state behavior before hardening.
- [ ] Agni and UniswapV3 mutable/immutable tests return `IncompleteState` on unsynced words.
- [ ] In all four paths, an `initialized` tick with no record never becomes a zero-liquidity cross.
- [ ] Deterministic within-coverage unit fixtures preserve exact expected amount/tick/liquidity
      transitions; M2-4 later adds independent on-chain differential evidence.

## References
- spec §R3; `agni/mod.rs:197,270-274,300,367,397-398`;
  `uniswap_v3/mod.rs:363-368,510-515,548-551,582-607`.
```

### M1-4 · `[M1] [amms/moe] Build an atomic, timestamped MoeSnapshot with explicit coverage`
**Milestone** M1 · **Priority** Urgent · **Labels** `bug`, `ready-for-agent`

```markdown
## Objective
Give Moe an atomic snapshot that binds slot0 + bins + queried coverage + block hash + header
timestamp, returns `IncompleteState` instead of a partial quote when the swap exceeds coverage,
and prices from `active_id` rather than the reserve ratio.

## Context
`sync_active_bins_batch` (`moe/mod.rs:525`) only inserts non-zero bins (never deletes),
`simulate_swap_precise` returns partial `Ok(amount_out)` at boundary/visited/`MAX_ITERATIONS`
breaks (`moe/mod.rs:1502-1569`), and `calculate_price` uses reserve ratio (`:853`) instead of
`get_price_from_id` (:340). Time-dependent math currently receives `time_of_last_update`
instead of the target header timestamp (:745-756). Spec 01-R6, B4-B6, and D5 also require
removing incomplete/invalid-state fallbacks. Batched sync must not let a later batch wipe an
earlier one — replace atomically.

## Blocked By
- M1-1; M0-6 (real Moe pools).

## Blocks
- M2-4 — differential tests validate Moe math and coverage.
- M3-4 — checkpoint restore must preserve Moe ranges and timestamp context.

## Implementation
1. Define `MoeSnapshot { slot0, bins, queried_ranges, block_hash, block_timestamp }`; use the
   timestamp of that same header for time-dependent fee/hook logic. Accumulate a full sync
   round into a temp snapshot, explicitly deleting bins that read zero; commit atomically
   only if slot0 + all bin batches succeed.
2. `simulate_swap_precise`: if the walk exits `queried_ranges` or ends with `amount_left > 0`,
   return `IncompleteState` — never a partial quote.
3. Require complete bins for precise quoting; an absent/unsynced bin set returns
   `IncompleteState` rather than the dimensionally wrong constant-product fallback. Ensure
   every published Moe snapshot runs bin sync, not only an example-specific side path.
4. `calculate_price`: use `get_price_from_id(active_id, bin_step)`. Delete every arbitrary
   "reasonable" amount/reserve threshold. Validate with checked `U256`, ABI field ranges,
   actual reserves/coverage, and protocol invariants; only real overflow, invariant violation,
   or incomplete coverage becomes a typed error.

## Acceptance criteria
- [ ] Depleted (now-zero) bins are removed on resync (test with a bin going to zero).
- [ ] A swap needing liquidity beyond `queried_ranges` returns `IncompleteState`.
- [ ] Empty/unsynced bins never select the old constant-product fee formula; invalid state is
      observable as a typed error rather than a zero quote.
- [ ] A fixed-block test proves Moe time-dependent math receives that snapshot's header
      timestamp, not wall-clock time or a timestamp from another block.
- [ ] A protocol-valid fixture with reserve >1e30 is quoted normally rather than rejected by
      a heuristic; deterministic `active_id` price fixtures pass exactly. M2-4 later supplies
      independent on-chain differential evidence.

## References
- spec §R6, §B4-B6, §D5; `moe/mod.rs:340,525,672-719,745-756,783-853,1482-1569`.
```

### M1-5 · `[M1] [arbitrage] Cap optimal-input search at executor WMNT balance`
**Milestone** M1 · **Priority** High · **Labels** `bug`, `ready-for-agent`

```markdown
## Objective
Bound the input-sizing search by the executor's actual WMNT balance so profitable
opportunities whose optimum exceeds funding are resized to the available amount instead of
being dropped at submit time.

## Context
All four active services search with a hardcoded upper bound before reading executor balance
(1559 :1128/:1034; old V3 :1136/:1070; V2 :848/:783; Moe :1524/:1349). They can discard or
mis-size a fundable opportunity. Spec 01-B1.

## Blocked By
- M1-1 — executor balance must be read from the same canonical `SnapshotId` as pool state.

## Blocks
- M2-7 — testnet E2E sizing must use the same snapshot-bound balance contract.
- M2-8 — shadow sizing must use the snapshot-bound executor balance.

## Implementation
1. Hash-pin the executor WMNT `balanceOf` read to M1-1's snapshot (or use its verified
   block-number fallback) and store it in that immutable block context.
2. Pass `max_input = min(MAX_INPUT, executor_balance)` into `best_path_simulation` before the
   search runs.
3. Re-simulate at the resized input to confirm profitability.
4. Apply the snapshot-bound balance search to V2, old V3, 1559 V3, and Moe; explicitly retire
   any entrypoint not migrated. Moe also retains M0-3's principal/min-return checks.

## Acceptance criteria
- [ ] When unconstrained optimum > balance, replay/shadow produces an eligible resized candidate
      and exact request (not a drop); M1 itself does not authorize production submission.
- [ ] Search never returns an input exceeding executor balance.
- [ ] A mixed-block balance/pool-state fixture is rejected; sizing proves both values share the
      candidate's full `SnapshotId`.

## References
- spec §B1; all four `*_monitor_executor_service` balance/search call sites cited above.
```

### M1-6 · `[M1] [arbitrage] Remove appearance-count suppression while preserving execution gates`
**Milestone** M1 · **Priority** High · **Labels** `bug`, `ready-for-agent`

```markdown
## Objective
Stop permanently blacklisting persistent opportunities: remove appearance-count suppression
from execution eligibility without bypassing the snapshot, coverage, economics, risk,
cooldown, circuit-breaker, deadline, or exact-preflight gates defined by this roadmap.

## Context
`AppearanceTracker` exists in all four active services. In 1559 it increments per distinct
block and permanently filters after `MAX_APPEARANCES=3`; `mark_as_failed` (:641-643)
permanently blacklists any balance/RPC/send error. A persistent real opportunity gets killed.

## Blocked By
- M0-2 — valid measured gas/profile and current fee inputs are P1 eligibility prerequisites.
- M1-1 — freshness means explicit `SnapshotStatus::Ready` with the current full id.

## Blocks
- M2-7 — repeatable triggers must not be permanently suppressed by prior appearances.
- M2-8 — the gate must exercise all state/economic/risk gates without permanent appearance suppression.

## Implementation
1. Remove appearance-count gating from execution eligibility.
2. Preserve every non-appearance gate available at this phase: Ready snapshot/coverage,
   current net profit, balance/position limits, cooldown, and preflight. M2 adds the full
   circuit-breaker, loss-budget, on-chain deadline, and exact-final-request lifecycle.
3. Keep any block-based counter only for tracker memory GC, not eligibility.
4. Split the failure store into permanent (path structurally infeasible) vs transient
   (balance/RPC/nonce, TTL-backed) and stop permanently blacklisting transient failures.
5. Apply this to V2, old V3, 1559 V3, and Moe; explicitly retire any service not migrated.

## Acceptance criteria
- [ ] An opportunity present across many blocks stays eligible when every non-appearance gate passes (test).
- [ ] A transient failure re-becomes eligible after its TTL; a permanent one does not.
- [ ] Repository-wide checks find no active AppearanceTracker-based eligibility gate.

## References
- spec §R7; `v3_monitor_executor_service_1559.rs:171-215,641-643`.
```

### M1-7 · `[M1] [state-space] Recover WS gaps with canonical header and log backfill`
**Milestone** M1 · **Priority** High · **Labels** `feature`, `ready-for-agent`

```markdown
## Objective
Prevent silent state drift on a WS reconnect: combine full block identity continuity with
hash-pinned log backfill before processing a newly delivered head.

## Context
`subscribe` fetches logs only for the delivered block (spec 01-D3); a reconnect that skips
blocks never backfills. Complementary to M1-1's parent-hash fork detection (that catches
same-height replacement; this catches skipped heights).

## Blocked By
- M1-1.

## Blocks
- M2-7 — the live testnet harness must fail closed across reconnect gaps.
- M2-8 — the observation window is invalid until reconnect gaps are recovered fail-closed.

## Implementation
1. Track the last processed `SnapshotId` and header, not just a number.
2. On a new block, validate number/hash/parent using M1-1. If `number > last+1`, fetch each
   missing canonical header, verify the parent chain, and fetch each block's logs by block hash
   (chunked number-range fallback only with canonical-hash verification around the read).
3. Reconnect the WS on drop and resume only after the full gap is applied atomically in order.
4. For historical discovery/range fallback, use a configurable initial window and bisect/retry
   on provider range or result-count errors; record endpoint capability instead of assuming a
   permanent chain-wide 10k limit.

## Acceptance criteria
- [ ] A simulated skipped-block reconnect backfills the gap; no pool misses its updates.
- [ ] A reorg during backfill is detected by hash/parent checks and publishes no mixed branch.
- [ ] A simulated provider range/result limit causes bounded window reduction and complete,
      duplicate-free recovery; no hardcoded “universally safe” range is claimed.

## References
- spec §D3, §D4.
```

### M1-8 · `[M1] [execution] Process the current block N (remove deliberate N-1 lag)`
**Milestone** M1 · **Priority** High · **Labels** `bug`, `ready-for-agent`

```markdown
## Objective
Remove the built-in one-block latency: process canonical block N on receipt rather than N-1.

## Context
V2 (`target_number = number - 1`), old/1559 V3, and Moe all process N-1. If the lag was a
workaround for logs-not-ready, fix the real cause rather than keeping any active entrypoint stale.

## Blocked By
- M1-1.

## Blocks
- M2-1.

## Implementation
1. Change block handling to target `number`; verify the delivered block's logs are queryable.
2. If a race exists, wait on log availability for block N rather than blindly using N-1.
3. Apply to all four services or explicitly retire an unpatched entrypoint.

## Acceptance criteria
- [ ] The bot processes block N's logs in the same iteration it receives N.
- [ ] No regression in pool-update correctness on a replay.
- [ ] Repository-wide checks find no active `number - 1`/`saturating_sub(1)` block target.

## References
- spec §R4; `v3_monitor_executor_service_1559.rs:655-664`.
```

### M1-9 · `[M1] [amms/agni] Repair mutable V3 swap state transitions`
**Milestone** M1 · **Priority** Urgent · **Labels** `bug`, `ready-for-agent`

```markdown
## Objective
Make `AgniPool::simulate_swap_mut` apply the exact post-swap sqrt price, tick, and liquidity
state instead of copying an unchanged clone, so sequential simulations consume real state.

## Context
`src/amms/agni/mod.rs:300-312` clones the pool, calls immutable `simulate_swap`, then copies
unchanged fields back. This is B7. M2 differential tests and M3 shared-core extraction must
consume an already-correct mutable implementation; they cannot own this prerequisite fix.

## Blocked By
- M1-1 — mutable state must carry the same complete snapshot/coverage identity.

## Blocks
- M2-4 — fixed-state differential tests cover the repaired mutable entry point.

## Implementation
1. Return/apply a typed post-swap state from the shared Agni simulation loop: amount out,
   sqrt price, tick, liquidity, and protocol fields affected by tick crossing.
2. Preserve M1-3 coverage failures: no partial mutation on `IncompleteState` or any error.
3. Add sequential-swap unit fixtures where the second quote differs from a fresh-pool quote.

## Acceptance criteria
- [ ] A successful mutable swap updates sqrt price/tick/liquidity to deterministic expected values.
- [ ] A second sequential swap starts from the first result; the old no-op implementation fails the test.
- [ ] Any error leaves the original pool unchanged, including uncovered/missing tick cases.

## References
- spec §B7; `src/amms/agni/mod.rs:197-312`.
```

---

## 4. M2–M4 设计摘要（M2-9/M2-10 除外，不是可直接执行或可转 ready 的 canonical Body）

> 下列条目保留 Objective / dependency / implementation / acceptance 的设计信息，但粗体 bullet **不符合** canonical issue headings。它们已作为 `needs-triage` 占位 issue 存在于 Linear（见约定 §6.5）；**转出 `needs-triage`（转 ready）或交付实现之前**，必须逐条展开为 `## Objective`、`## Blocked By`、`## Blocks`、`## Implementation`、`## Acceptance criteria`，补文件锚点/测试命令和 metadata，届时才授予相应 ready label。例外是 M2-9/M2-10：两者从创建起就是完整 canonical、`ready-for-human` 的人工 runbook。

### M2-1 · `[M2] [execution] Implement the nonce-intent execution state machine`
- **Objective**: Decouple discover/send/receipt tracking in every active service; model nonce intents with multiple attempts, canonical inclusion/finality, reorg recovery, and same-nonce replace/cancel.
- **Blocked By**: M0-2, M0-8, M1-1, M1-8. **Blocks**: M2-2, M2-6, M3-5.
- **Implementation**: (1) discover/size feeds a bounded latest-wins queue; `SnapshotStatus` must be `Ready` and the full id current immediately before signing. (2) Broadcast returns immediately; a separate receipt tracker handles every active service. (3) One nonce intent owns attempts/hashes: `reserved → submitted(attempt*) → included_unconfirmed → finalized | reverted_finalized`; replacement creates a new attempt, not a terminal state; cancel is terminal only after its tx canonical-finalizes. Track dropped/original-vs-replacement races. (4) Every replacement binds the latest `BlockFeeContext` and compatible gas-profile identity, rebuilds exact calldata/deadline, applies M2-3 preflight policy, and passes latest net-profit/loss-budget limits; it never returns to production `eth_estimateGas`. (5) Verify receipt block hash canonical and the configured confirmation/finality policy; reorg reopens/resyncs nonce, balance, and candidate. (6) Migrate V2, old V3, 1559 V3, and Moe or explicitly retire an unpatched service.
- **Acceptance**: no active hot path blocks on `watch()`; no stale/not-Ready work is signed; all hashes for one intent are tracked without nonce reuse; blind fee bump is impossible; restart/reorg tests recover an included-unconfirmed intent; only canonical-finalized receipt/cancel is terminal; expired original calldata reverts on-chain.

### M2-2 · `[M2] [arbitrage] Version the MarketSnapshot→Candidate→Preflight→Submit pipeline`
- **Objective**: Thread M1-1's full `SnapshotId { chain_id, block_number, block_hash }` plus `pool_universe_fingerprint/manifest_version` (or an equivalent monotonic readiness epoch) through candidate → final request → preflight → submit so no stage can silently operate on a different state or topology generation.
- **Blocked By**: M1-1, M2-1. **Blocks**: M2-3, M2-7, M2-8.
- **Implementation**: (1) `Candidate { snapshot_id, pool_universe_fingerprint, manifest_version_or_epoch, ordered_hops, input, expected_out, … }`. (2) Each transition requires `SnapshotStatus::Ready` with the identical full state and topology identity; mismatch/Syncing/Halted means purge/drop and recompute, never a stale-block allowance. (3) Balance resizing/re-simulation and final calldata construction happen before preflight. (4) Give one pipeline component ownership of transitions and migrate all active entrypoints.
- **Acceptance**: a candidate is rejected before signing on chain/number/hash/topology-generation difference or non-Ready status; tests cover same-height replacement, height advance, a same-block manifest promotion, and a failed new-head sync that retains an old recovery snapshot but revokes execution.

### M2-3 · `[M2] [execution] Apply risk-tiered exact-request semantic preflight`
- **Objective**: Apply `eth_call` to the exact final transaction request only where the approved risk policy requires it. It is mandatory in E2E/shadow, for a new executor codehash/profile, unsupported or incompletely modeled venues/hooks, and production canary; stable low-latency production paths may disable or sample it only after recorded evidence and human approval. Semantic checking remains separate from M0-9/M0-2 measured-profile gas sizing.
- **Blocked By**: M2-2. **Blocks**: M2-7, M2-8.
- **Implementation**: (1) after balance resizing/re-simulation, build the final request with the authorized hot executor `from`, replacement `to`, exact `data/value`, gas profile and all transaction fields. (2) The policy key includes executor codehash, route/venue support, environment stage and qualification state. (3) When required, prefer reliably supported `pending`, otherwise explicitly use `latest`, and run exactly one semantic `eth_call`; never call `eth_estimateGas`. (4) Re-read the in-memory complete state+topology+gas-profile `Ready` identity after the response; mismatch purges/recomputes. (5) Measure RPC latency, revert reasons and false-negative/late-inclusion outcomes; an approval record is required before disabling or sampling preflight for a production profile.
- **Acceptance**: E2E/shadow/canary and unqualified profiles cannot bypass required preflight; an approved stable profile can use zero semantic-preflight RPCs in the hot path; no path calls `eth_estimateGas`; wrong `from`, changed calldata/head, or topology change cannot reuse a pass; results and latency are recorded. A pass proves only non-reversion in the selected RPC state, never inclusion or realized profit.

### M2-4 · `[M2] [Bench] Add exact fixed-state AMM differential tests`
- **Objective**: Pin off-chain AMM math to on-chain behavior with a differential suite covering V2 rounding, Agni repaired mutable/immutable cross-tick paths, verify-only UniswapV3 mutable/immutable baselines plus coverage hardening, and Moe cross-bin behavior at fixed block hashes/timestamps.
- **Blocked By**: M1-3, M1-4, M1-9. **Blocks**: M2-7, M2-8, M3-2, M4-3, M4-4.
- **Implementation**: (1) fixture set of canonical pools + `SnapshotId`/timestamp. (2) Hash-pin an on-chain quoter/call and require exact integer amount-out equality for V2, Agni V3 and UniswapV3 immutable/mutable state transitions, and canonical Moe pairs without unmodeled hooks. Unsupported hook behavior returns typed `Unsupported/IncompleteState` rather than using a loose tolerance. Any future nonzero tolerance requires a reviewed ADR with a fixed absolute bound and protocol proof. (3) Cover V2 one-wei rounding, Agni repaired startup/mutable paths, UniV3 existing full-sync/mutable baselines plus initialized-tick-missing fail-closed behavior, Moe active-id/fee/time/cross-bin behavior, and coverage errors. (4) Mutation tests deliberately break rounding/tick/bin logic and must fail.
- **Acceptance**: `cargo test --test differential` passes exact comparisons for every supported entry point; unsupported cases fail closed; each mutation is caught. Results prove only fixed-state equivalence, not future inclusion.

### M2-6 · `[M2] [execution] Add authenticated circuit breakers and pending cancellation`
- **Objective**: Add consecutive-revert halt, max-loss-per-window, max-position cap, and an authenticated operational kill switch.
- **Blocked By**: M2-1. **Blocks**: M2-7, M2-8.
- **Implementation**: (1) counters in the execution state machine; window loss includes actual receipt gas from every canonical-included success, revert, or cancel, not only trade PnL. Non-included losing attempts in a same-nonce race are recorded but have no receipt/gas and are not charged. (2) halt + alert on N reverts or total window loss > cap; do not import Base's observed revert rate as a Mantle threshold. (3) hard per-tx/total deployed-inventory caps. (4) authenticated/audited local pause is checked before reserve/sign/broadcast and fail-closed on untrusted restart state; when M0-8's pause-only guardian is configured, the breaker may request an on-chain pause but only the cold admin can unpause. (5) entering pause triggers best-effort cancel for every replaceable pending nonce, with M0-8's on-chain deadline as hard fallback.
- **Acceptance**: fixtures prove canonical replacement/cancel receipt gas cannot bypass or double-count the loss window, while non-included attempts add zero realized gas; evidence-based revert/loss/position/inventory caps pause and alert; hot executors cannot withdraw/unpause/change trust; pause survives restart, creates no new attempts, requests cancel for every pending intent, and a cancel failure can only remain executable until its finite deadline.

### M2-7 · `[M2] [Infra] Build a reusable Mantle Sepolia arbitrage E2E gate`
- **Objective**: Provision a reusable Mantle Sepolia environment, trigger a controlled price imbalance with one command, and prove the repaired real bot completes and reconciles one arbitrage through canonical finality.
- **Blocked By**: M0-3, M0-5, M1-2, M1-5, M1-6, M1-7, M2-2, M2-3, M2-4, M2-6. **Blocks**: M2-8.
- **Implementation**: (1) Use a dedicated `MANTLE_SEPOLIA_E2E_*` credential namespace and test-only signer. Query the network id, verify the then-current official Mantle Sepolia metadata (the repository currently assumes chain id 5003), hard-reject Mantle mainnet 5000, mainnet RPC/executor addresses, and any production key. (2) Inventory current canonical Sepolia factory/router/pool deployments and verify factory provenance/runtime code. If public deployments are incomplete or uncontrollable, deploy repo-owned factories/pools that are ABI- and pricing-behavior-compatible with at least two supported venues; label them fixtures rather than canonical DEX deployments. Create a WMNT settlement cycle with seeded depth. (3) Build an idempotent bootstrap for test tokens, venues/pools, liquidity, **M0-8's exact replacement artifact**, and test inventory. This is the first network deployment of the replacement. Persist schema/config/ABI/profile fingerprints plus every address, deployment tx, runtime codehash, cold admin/hot executor/guardian and seed-liquidity receipt; reuse only an exact match. (4) Provide a separate parameterized trigger command that swaps one controlled pool, records before/after reserves and tx/block identity, and can be rerun without token/pool redeployment. (5) Orchestrate the repaired production pipeline through `Ready snapshot → candidate → measured gas-profile lookup + current fee context → required exact-request preflight → broadcast → canonical-finalized receipt`; assert ordered path/provenance, trade-local deltas, explicit minProfit/deadline, SafeTransfer behavior, role separation, sender receipt gas, nonce intent/attempt and realized-PnL reconciliation. Timeout/failure emits a bounded diagnostic bundle and returns nonzero. (6) Keep the live credentialed gate manual/scheduled and excluded from ordinary CI; add an offline deterministic counterpart to normal CI. Replace, do not revive, the obsolete `execute_sepolia_arbitrage` target deleted by M0-4.
- **Acceptance**: bootstrap reruns without duplicate deployment and rejects any manifest/codehash/owner/network mismatch; the trigger can create a second opportunity after a completed run; one command produces trigger and arbitrage tx hashes, a canonical-finalized receipt, positive reconciled settlement delta after configured costs, and a reproducible evidence bundle; no production secret/address/network is accepted; public-vs-fixture provenance is explicit; ordinary CI remains credential-free. The evidence names the exact selected protocol adapters/topology and claims E2E coverage only for them; M2-4 remains the all-supported-protocol math gate and M2-8 remains the full-pipeline observation gate.

### M2-8 · `[M2] [execution] Run the P2.5 signerless shadow validation gate`
- **Objective**: Run the complete canonical-snapshot → size → exact-request preflight pipeline in a runtime without signer/broadcast capability, then require a human go/no-go before any existing entrypoint may regain production send.
- **Blocked By**: M0-3, M1-2, M1-5, M1-6, M1-7, M2-2, M2-3, M2-4, M2-6, M2-7. **Blocks**: M2-9, M3-1, M3-2, M3-4, M3-5, M3-7, M3-10.
- **Implementation**: (1) enforce a hard dry-run boundary after final-request preflight and before signing; the shadow runtime receives no signer or broadcast capability. (2) Ledger rows include the full state+topology readiness identity, ordered path, input, final-request digest, expected gross/net profit, fee/header context, coverage status, final preflight state/result, and opportunity lifetime. (3) Before the run, define minimum canonical blocks/runtime, candidate and preflight samples, protocol/topology coverage, and continuity/coverage error budgets; duration is an input to evidence, not an automatic 1–2 week pass. (4) Publish the decision criteria and summary; do not call simulated profit realized PnL.
- **Acceptance**: no transaction is signed/sent; every ledger candidate has complete coverage/current state+topology/profile identity; all predeclared evidence thresholds and error budgets are evaluated; statistics are reproducible; a `ready-for-human` decision explicitly records approve/reject criteria. Approval does not itself deploy, fund or enable production; it only unblocks M2-9's unfunded deployment/verification for the tested existing entrypoints and is not inherited by M3's merged binary. Reject leaves M2-9/M2-10 and downstream work blocked. M4-6 (reversible dev config) and M4-9 (evidence-only research) are explicit non-sending exceptions.

### M2-9 · `[M2] [Contracts] Deploy and verify the unfunded production executor`
**Milestone** M2 · **Priority** Urgent · **Labels** `chore`, `ready-for-human`

```markdown
## Objective
After an explicit M2-8 approve decision, deploy the exact qualified replacement executor once to
Mantle mainnet, verify its runtime and authority configuration, and stop with the contract paused,
unfunded, and unable to execute. Record enough evidence for a separate human funding decision.

## Context
M0-8 deliberately ends with an undeployed artifact; M2-7 is its first network deployment and is
testnet-only. Shadow approval must not silently deploy or fund anything. Mainnet deployment and role
configuration change production state but need not expose inventory. Funding and canary execution
have a higher economic risk and therefore belong to M2-10 behind a second explicit human go/no-go.

## Blocked By
- M2-8 — only an explicit approve outcome for the exact artifact/profile/config may unblock this runbook.

## Blocks
- M2-10 — funding and canary may be considered only from a verified paused/unfunded deployment.

## Implementation
1. Re-read the signed/recorded M2-8 decision and pin the approved git commit, M0-8 optimized runtime
   codehash + ABI digest, M0-9 gas-profile digest, chain id `5000`, settlement asset, supported venue
   factories/init-code hashes or allowlist, prospective inventory/canary caps, and breaker settings. Any drift
   returns to the owning implementation/qualification issue; do not patch source during deployment.
2. Using the pinned Foundry/compiler/toolchain and a cold-admin deployment account, deploy exactly
   that bytecode once. Immediately verify chain id, deployed runtime codehash, immutable WMNT, ABI
   getters and constructor/config values before granting authority or moving inventory.
3. Configure the approved revocable hot executor(s) and optional pause-only guardian while the
   contract remains paused. Verify that hot executors cannot withdraw, change trust, or unpause;
   do not give any production runtime permission to send through the address in this issue.
4. Record the deployment transaction, address, block/hash, runtime codehash, ABI/profile/config
   digests, cold admin, hot executors, guardian and trust configuration in
   `tech-docs/deployments.md` (or its canonical successor). Update secret-free runtime configuration;
   never store private keys or secret values in the repository.
5. Query WMNT, every known path token, and native MNT balances. They must all be zero. Publish a
   signed/recorded deploy-and-verify result whose explicit terminal state is `paused + unfunded` and
   whose approve/reject recommendation is an input to M2-10, not an authorization to move funds.

## Out of scope
- Contract, ABI, Rust-caller, gas-profile, or security-policy changes.
- A second deployment to work around a verification mismatch.
- Any token/native funding, unpause, canary transaction, or production signer/broadcast enablement.

## Acceptance criteria
- [ ] The only new production address has the exact M0-8 runtime codehash/ABI and M0-9 profile digest
      approved by M2-8, on chain id 5000; any mismatch aborts the runbook.
- [ ] Cold admin, revocable hot executor(s), optional pause-only guardian, trust configuration,
      settlement asset and venue provenance all match verified on-chain reads.
- [ ] The executor remains paused, every token/native balance is zero, no transaction has executed
      through it, and no deprecated executor address receives funds or traffic.
- [ ] Deployment/role/config evidence is complete and secret-free; it records a distinct second
      human go/no-go as mandatory before M2-10 may fund or canary the address.

## Testing and verification
- Run the pinned deployment command recorded by the issue; capture its receipt and deployed address.
- Use `cast chain-id`, `cast code`, and contract getter/role calls against Mantle mainnet; hash the
  returned runtime and compare it byte-for-byte with the M0-8 artifact.
- Query WMNT, all known path-token, and native MNT balances and prove they remain zero.
- Verify paused/role state from an independent read-only account and attach the complete evidence.

## References
- M0-8 / WHI-501; M0-9 / WHI-546; M2-3 / WHI-521; M2-6 / WHI-524;
  M2-7 / WHI-525; M2-8 / WHI-526.
- `contracts/executor/ArbitrageExecutor.sol`, `tech-docs/deployments.md`.
```

### M2-10 · `[M2] [Contracts] Fund and canary the verified production executor`
**Milestone** M2 · **Priority** Urgent · **Labels** `chore`, `ready-for-human`

```markdown
## Objective
After a second explicit human go/no-go, fund the verified production executor with only the approved
capped WMNT inventory and execute one controlled canary through canonical finality. Stop or proceed
using recorded breaker, preflight, receipt, gas, and realized-settlement evidence.

## Context
M2-9 ends with a mainnet address that is codehash/role/config verified but paused and unfunded. This
separates low-economic-risk deployment verification from the first operation that exposes principal.
The production latency model intentionally uses bounded resident WMNT inventory; it does not add a
per-transaction native funding round trip.

## Blocked By
- M2-9 — requires its exact address and complete `paused + unfunded` verification artifact, followed
  by a separately recorded human approve decision for the proposed cap and canary parameters.

## Blocks
- Broader production signer/broadcast enablement for the approved existing entrypoints.

## Implementation
1. Re-read M2-8 and M2-9 evidence immediately before funding. Verify chain id `5000`, current runtime
   codehash/ABI/profile/config digests, paused state, roles, venue trust, zero balances, current gas
   profile qualification, and that no intervening change invalidated either decision.
2. Record the second human go/no-go with the exact WMNT inventory cap, native MNT operational cap,
   canary input/notional, explicit `minProfit`, finite `deadline`, M2-3 mandatory canary preflight,
   M2-6 breaker/loss limits, and abort/withdraw criteria. Reject means no transfer and no unpause.
3. Transfer only the approved WMNT amount and verify it on-chain. Account for native MNT separately
   and keep it at or below its explicit operational cap; native MNT is not trade inventory and
   per-transaction `msg.value` funding remains disabled.
4. Enable only the approved hot executor/canary path, run required exact-request preflight, and submit
   the single bounded canary. Track nonce intent/attempt through canonical finality and reconcile the
   executor settlement delta plus sender receipt gas against the approved limits.
5. On any role/code/config/profile/preflight/receipt/PnL mismatch, pause immediately, disable sending,
   execute the documented inventory recovery path, and record the failure. A successful canary may
   authorize only the tested existing entrypoints within approved caps; it does not give the M3
   merged binary a signer or bypass M3-9.

## Out of scope
- Contract, ABI, caller, gas-profile, or trust-policy changes.
- Uncapped inventory, multiple canaries, broad inventory scaling, flash loans, packed calldata,
  disabling canary preflight, or merged-binary production enablement.

## Acceptance criteria
- [ ] A second human approval names the exact verified address/codehash and all funding/canary caps;
      without it, balances remain zero and the contract remains paused.
- [ ] WMNT/native funding never exceeds its separate approved cap, no deprecated executor is funded,
      and the resident-inventory model adds no per-transaction funding transaction.
- [ ] Exactly one bounded canary uses required preflight and reaches a canonical-finalized receipt;
      profile/gas identity and realized settlement PnL reconcile within the approved limits.
- [ ] Any failure leaves new sending disabled and triggers pause/recovery evidence; success authorizes
      only the tested existing entrypoints and does not grant signer access to the M3 binary.

## Testing and verification
- Independently re-run the M2-9 `cast` code/role/config/balance checks before approving funding.
- Record WMNT/native balances before and after the capped transfer.
- Attach the canary intent, preflight result, transaction/receipt/block identity, profile/gas fields,
  settlement delta, sender gas reconciliation, breaker state, and final human proceed/stop decision.

## References
- M2-3 / WHI-521; M2-6 / WHI-524; M2-8 / WHI-526; M2-9 / WHI-547.
- `contracts/executor/ArbitrageExecutor.sol`, `tech-docs/deployments.md`.
```

### M3-1 · `[M3] [Infra] Merge four services into one multi-protocol binary (concurrent V2/V3/Moe)`
- **Objective**: Collapse the four `*_monitor_executor_service` files (~5.6k lines) into one `src/bin/bot.rs` that can enable V2/V3/Moe **concurrently** (required for cross-DEX arbitrage), via a `Protocol` trait + shared block loop / config / execution worker.
- **Blocked By**: an approved M2-8 outcome. **Blocks**: M3-3, M3-6, M3-8, M3-9, M3-10, M4-1.
- **Implementation**: (1) extract shared scaffolding (block loop, `ServiceConfig::from_env`, execution state machine) into library modules per spec 02. (2) `Protocol` trait for Agni-V2/Agni-V3/Moe. (3) `--protocols agni-v3,agni-v2,moe` multi-select. (4) Introduce `PoolUniverseSource`; the initial legacy CSV adapter validates every row's protocol/token/provenance, freezes after startup, and publishes a stable universe fingerprint. It has no reload/promotion path. (5) unify env var names with documented aliases. (6) Keep the four old services production-disabled as replay references until M3-9; do not offer thin-shim alternate production entrypoints.
- **Acceptance**: one signerless binary runs all three protocols and discovers cross-DEX paths using the frozen validated adapter; replay fixtures compare old/new behavior; no topology hot reload exists before M3-10; deployment config cannot enable the old services or give the merged binary a production signer before M3-9.

### M3-2 · `[M3] [amms/agni] Share concentrated-liquidity core with UniV3, keep Agni adapters`
- **Objective**: Remove the ~900-line verbatim copy by sharing the tick-crossing/liquidity math core with `uniswap_v3`, while keeping Agni's own event decoding + ABI/factory/init-code adapters.
- **Blocked By**: an approved M2-8 outcome and M2-4 (the gate authorizes P3 work; diff tests guard the refactor). **Blocks**: M3-8.
- **Implementation**: (1) extract a shared `TickPool` core. (2) Agni provides params + adapters, not a copy of the math. (3) preserve M1-9's already-correct mutable behavior and replace saturating liquidity arithmetic with checked errors.
- **Acceptance**: Agni math is a thin parameterization; diff tests (M2-4) stay green; no `saturating` on liquidity_net remains.

### M3-3 · `[M3] [arbitrage] Configure settlement semantics and remove invalid misprice paths`
- **Objective**: Put settlement-cycle semantics in the strategy layer, remove/fix the unit-invalid open `find_two_pool_misprices` path, and dedup only economically identical ordered cycles.
- **Blocked By**: M3-1. **Blocks**: M3-9.
- **Implementation**: (1) put `settlement_asset` in strategy/engine config; enforce start=end there and in builder/contract. Require `settlement_asset == executor.WMNT == wrapped native gas asset` for this deployment; other assets require a generalized contract and native-gas conversion. (2) Delete `find_two_pool_misprices` and its monitor path because it emits open A→B quantities with invalid profit units; the engine's supported opportunity primitive is a closed settlement cycle. (3) dedup key is the complete ordered `(pool, token_in, token_out)` sequence, canonicalized by rotation only. Instrument the actual duplicate source before suppression. (4) make `max_hops` a strategy config **defaulting to 3** (per ARB_PATHS_MANTLE.md; 2–3 hops = 93.5%) and converge the library `pathfinder` default + every service `MAX_HOPS` on it (currently V2/Moe=4, pathfinder default=4).
- **Acceptance**: no open A→B path reaches `simulate_path` profit arithmetic; incompatible settlement config fails startup; opposite-direction/order paths remain distinct; `max_hops` defaults to 3 with no residual 4-hop constant on any active path; unit tests cover token-decimal mismatch and ordered dedup without losing paths.

### M3-4 · `[M3] [state-space] Persist and verify integrity-checked checkpoints`
- **Objective**: Restore quickly without ever publishing corrupt, stale, cross-chain, or insufficiently verified pool state as a quotable `MarketSnapshot`.
- **Blocked By**: an approved M2-8 outcome plus M1-1, M1-3, M1-4. **Blocks**: none.
- **Implementation**: (1) persist schema version, `SnapshotId` + parent/timestamp, behavior-affecting config fingerprint, pool-universe fingerprint, V3 word/tick and Moe queried-range coverage, and the complete pools/ticks/bins payload. Cover metadata and payload with a content digest. (2) Write a same-directory temp file, flush/fsync it, atomically rename it, and fsync the directory. (3) Deserialize with strict file-size, collection-count, nesting/depth, and allocation bounds. (4) State the trust model: a digest detects accidental corruption; if malicious local modification is in scope, require a MAC/signature whose secret/key is stored independently of the checkpoint. (5) Before publication, verify the saved block hash is still canonical and validate pricing-critical slot0 plus coverage boundaries against hash-pinned chain reads. On any uncertainty, discard and fully sync.
- **Acceptance**: truncated/partially written/digest-mismatched/oversized checkpoints are rejected without panic; a same-height replaced block and a config/pool-universe mismatch trigger full sync; no restored state is published before canonicality and chain-backed validation pass; tamper resistance is not claimed unless the independent-key MAC/signature test passes.

### M3-5 · `[M3] [Infra] Reconcile canonical-finalized PnL with chain balances`
- **Objective**: Persist every nonce intent/attempt and recognize PnL only from canonical-finalized receipts, reconciling executor inventory with sender native-gas spend.
- **Blocked By**: an approved M2-8 outcome plus M2-1. **Blocks**: M3-9.
- **Implementation**: (1) SQLite durable store keyed by nonce intent and tx attempt/hash. (2) Record included-unconfirmed separately and recognize realized results only after M2-1 canonical-finality. (3) Reconcile settlement token movements and sender native gas; convert only under M3-3. (4) expose gross/net realized PnL; keep shadow expected profit separate.
- **Acceptance**: realized PnL uses only canonical-finalized receipts; reorg removes unfinalized attribution; token deltas minus receipt gas reconcile; replacement races are counted once at intent level and shadow rows never enter realized PnL.

### M3-6 · `[M3] [Infra] Expose Prometheus pipeline and execution metrics`
- **Objective**: Expose per-block processing latency, opportunities found, submit count, success rate, current balance, cumulative gas.
- **Blocked By**: M3-1. **Blocks**: M3-9, M4-1.
- **Implementation**: expose a Prometheus HTTP endpoint (bind address configurable, loopback by default) from the merged binary. Instrument block→snapshot→candidate→preflight→broadcast stages, snapshot readiness/fork/gap counts, nonce intents/attempts/finality, balances, gas, and circuit-breaker state; keep structured logs for exemplars only.
- **Acceptance**: Prometheus exposes the named counters/gauges and p50/p95/p99-capable histograms; endpoint auth/network exposure is documented and defaults to local-only.

### M3-7 · `[M3] [state-space] Implement full reorg unwinding without deep-reorg panic`
- **Objective**: Implement `StateChangeCache`-based unwinding so a reorg rolls back to a consistent pre-fork state; deep reorgs return an error and trigger resync instead of panicking.
- **Blocked By**: an approved M2-8 outcome plus M1-1. **Blocks**: M4-8.
- **Implementation**: (1) advance/link `latest_block` so unwinding fires (spec D1). (2) `cache.rs:47` return `Err` on deep reorg; caller full-resyncs. (3) raise `CACHE_SIZE` (30→≥128).
- **Acceptance**: a simulated N-block reorg (N<cap) restores pre-fork state; a >cap reorg errors + resyncs, no panic.

### M3-8 · `[M3] [Infra] Remove dead lib components superseded by the merge`
- **Objective**: After M3-1, delete only components proven superseded and zero-call-site (the old `Executor` struct/placeholders, obsolete gas implementation, unused filters/discovery pieces, and confirmed dead Moe helpers), while preserving still-used test and migration dependencies.
- **Blocked By**: an approved M3-9 outcome plus M3-1 and M3-2. **Blocks**: none.
- **Implementation**: delete the four production-disabled legacy services after M3-9, then per spec 05-B1..B5 remove only proven zero-call-site components. Keep active dynamic fee code; reuse/replace `NonceManager` only after migration; keep `MoeParameters`/`mock.rs`. Do not delete `PathOptimizer::optimize` until M4-4 migrates `mock.rs:260`.
- **Acceptance**: exactly one production binary remains; each deleted symbol/file has no target call site; `cargo test --all-targets` and offline mock are green; no compatibility shim is mislabeled dead.

### M3-9 · `[M3] [execution] Requalify the merged binary in signerless shadow mode`
- **Objective**: Prove the merged multi-protocol pipeline is replay-equivalent where behavior should match and trustworthy for new cross-protocol paths before it receives a production signer.
- **Blocked By**: M3-1, M3-3, M3-5, M3-6, M3-10. **Blocks**: M3-8 and production enablement of the merged binary, but only on an approve outcome; reject keeps both blocked.
- **Implementation**: make deterministic old-vs-new replay equivalence the primary gate for unchanged same-protocol behavior. Then run the merged binary without signer/broadcast capability for a risk-sized shadow sample focused on changed behavior: cross-protocol paths, concurrent scheduling, manifest integration and any intentional replay delta. Reuse M2-7/M2-8 fixtures, thresholds and evidence where inputs/behavior are unchanged; do not default to another full 1–2 week window. Define human approve/reject thresholds before the run.
- **Acceptance**: expected replay equivalence passes; intentional architecture differences are reviewed and receive targeted shadow evidence proportional to risk; no send capability exists during the run; a `ready-for-human` decision records evidence and explicitly grants or denies signer access. M2-8 approval alone cannot satisfy this gate.

### M3-10 · `[M3] [Infra] Generate a versioned static pool and path manifest`
- **Objective**: Move factory/topology discovery out of the hot path by generating a deterministic, reviewable manifest of canonical pools and ordered settlement cycles **up to 3 hops** (per ARB_PATHS_MANTLE.md §4: 2–3 hops = 93.5% of arbs; 4+ negligible), while keeping all price/liquidity/gas state dynamic.
- **Blocked By**: M0-6, an approved M2-8 outcome, and M3-1. **Blocks**: M3-9, M4-7.
- **Implementation**: (1) enumerate configured canonical factories at one recorded `SnapshotId`; verify protocol, token pair/direction and factory provenance. (2) retain pools participating in at least one ordered `settlement_asset` cycle with **≤3 hops** (`max_hops=3`). (3) apply versioned TVL plus standardized executable-depth/price-impact policy; record decimals, valuation source/time/version and send unknowns to human review. (4) emit schema/chain/header/factory/config/policy/tool fingerprints, deterministic pool/path payload, and content digest. (5) replace M3-1's frozen legacy adapter through the shared `PoolUniverseSource`. Periodic jobs generate candidate diffs only; the future `ready-for-human` promotion approves changes. Promotion stops new intents, lets pending intents canonical-finalize or best-effort cancels them with deadline fallback, verifies the generation block remains canonical, leaves `Ready`, purges old candidates/queues, fully syncs the new universe, atomically rebuilds snapshot/path index at a block boundary, and only then returns `Ready` with a new `pool_universe_fingerprint/manifest_version` (or monotonic readiness epoch). Candidate/send identity must match it. (6) Keep candidate manifest separate from owner-controlled executor allowlist; never auto-promote discovery into chain trust. Where an allowlist is enabled, the runtime executable subset is the intersection of the approved manifest and current owner-controlled allowlist.
- **Acceptance**: identical inputs yield byte-stable manifests/diffs; a graph oracle proves completeness only relative to the recorded factory/block/settlement/max-hop/filter inputs (not all future opportunities); non-canonical pools, open/>3-hop paths, unverifiable valuation, and corrupt fingerprints fail closed or require review; a same-block promotion invalidates every old candidate and runtime never mixes manifest versions; pool state/base fee remain per-block; promotion cannot mutate the executor allowlist automatically or execute a pool outside the allowlisted intersection.

### M4-1 · `[M4] [Bench] Measure p50/p95/p99 block-to-submit replay latency`
- **Objective**: A replay harness that feeds recorded blocks through the merged pipeline and reports the block→submit latency distribution.
- **Blocked By**: M3-1, M3-6. **Blocks**: M4-2, M4-3, M4-4, M4-5, M4-7, M4-8.
- **Implementation**: record a block/log fixture; replay through discover→size→preflight; measure wall-clock per stage; report p50/p95/p99.
- **Acceptance**: the harness prints p50/p95/p99; results are reproducible on the fixture.

### M4-2 · `[M4] [arbitrage] Cache per-input quotes and re-optimize changing bounds`
- **Objective**: Two-layer incremental evaluation: recompute AMM quote curves only for paths whose pool versions changed; every block apply current fee policy, while treating a changed balance bound as a possible re-optimization event rather than merely clipping one old optimum.
- **Blocked By**: M4-1. **Blocks**: none.
- **Implementation**: (1) cache per-input gross-quote samples/peak intervals by a path `QuoteDependencyFingerprint` containing every dependency pool's state+coverage version and quote-relevant header inputs (for example Moe timestamp), not by the globally changing `SnapshotId` and not as one best point. (2) Fee/priority changes re-score input-dependent gas without rerunning AMM math. (3) Balance changes alter the feasible domain: reselect/local-optimize from covered samples or rerun sizing. (4) Reuse across block ids only when the fingerprint is identical; rebind any candidate to the current complete state+topology `Ready` identity and rerun freshness/net/preflight gates.
- **Acceptance**: unchanged V2/V3 pool dependencies reuse gross samples across a new block while a base-fee jump flips net profitability; a Moe timestamp dependency or any state/coverage change invalidates only affected paths; a changed balance fixture finds the best feasible input where clipping is wrong; no candidate bypasses current state+topology readiness.

### M4-3 · `[M4] [amms/moe] Wire tree_math for bin traversal (replace ±1 linear scan)`
- **Objective**: Replace the ±1 linear bin scan (≤512 iters) with the ported `tree_math` bitmap-tree next-non-empty-bin lookup.
- **Blocked By**: M2-4, M4-1 (diff tests guard; benchmark justifies). **Blocks**: none.
- **Implementation**: wire `src/amms/moe/math/tree_math.rs` into `simulate_swap_precise` bin traversal (`moe/mod.rs:1502,1572-1591`).
- **Acceptance**: bin traversal uses tree_math; diff tests stay green; non-empty liquidity
  beyond 512 bins is reachable when it lies inside the snapshot's explicit queried coverage.

### M4-4 · `[M4] [arbitrage] Implement multi-peak optimal-input search`
- **Objective**: Replace ad-hoc hill-climb with a search that does not assume concavity: log-scale coarse sampling to locate candidate peak intervals, local optimize each, check endpoints.
- **Blocked By**: M2-4, M4-1. **Blocks**: none.
- **Implementation**: implement in the shared engine optimizer; validate against replayed piecewise/multi-peak paths (V3 tick, LB bin, integer rounding, input-dependent gas). Migrate production services and `MockArbitrageContext` (`mock.rs:260`) to it, prove replay/differential parity, then delete `PathOptimizer::optimize`.
- **Acceptance**: on a known multi-peak fixture, the search finds the best sampled/refined optimum where the old hill-climb does not; production and offline mock use the same implementation; the old optimizer has zero call sites before deletion; measured latency remains within budget.

### M4-5 · `[M4] [execution] Evaluate revm CacheDB for final-request simulation`
- **Objective**: Evaluate/adopt a revm CacheDB local EVM simulation for the final candidate, only if M4-1 latency data shows `eth_call` preflight is the bottleneck.
- **Blocked By**: M4-1. **Blocks**: none. **Labels**: `research`.
- **Implementation**: spike revm against a fixed block; compare fidelity + latency vs `eth_call`. Note it is higher-fidelity, not identical (state/block-env completeness caveat).
- **Acceptance**: a written decision (adopt/defer) backed by measured latency + fidelity numbers.

### M4-6 · `[M4] [Infra] Lower dev profile opt-level for faster iteration`
- **Objective**: Speed up dev builds by lowering `[profile.dev]` from `opt-level=3`+lto to `opt-level=1`, no lto; keep release fully optimized.
- **Blocked By**: none. **Blocks**: none. **Labels**: `chore`.
- **Implementation**: edit `Cargo.toml` `[profile.dev]`.
- **Acceptance**: dev incremental build time drops measurably; release profile unchanged.

### M4-7 · `[M4] [arbitrage] Cache path pool-index mappings after graph construction`
- **Objective**: Remove O(hops × pools) address scans from the hot quote path by resolving each ordered hop to stable pool indices when a path cache is built.
- **Blocked By**: M3-10, M4-1. **Blocks**: none.
- **Implementation**: store `Vec<pool_index>` (plus the expected ordered token directions) in each path-cache entry; rebuild/validate indices whenever the pool universe fingerprint changes.
- **Acceptance**: hot-path replay performs no full-pool linear lookup per hop; stale indices fail validation after a pool-universe change; benchmark data shows the measured effect.

### M4-8 · `[M4] [state-space] Evaluate inverse deltas for the reorg buffer`
- **Objective**: Evaluate inverse state deltas for the reorg buffer only if profiling shows cloning mutated V3/Moe state is material; preserve M1 atomic snapshot semantics.
- **Blocked By**: M3-7, M4-1. **Blocks**: none. **Labels**: `research`.
- **Implementation**: profile clone cost/size, prototype per-block inverse deltas for only mutated pools, and test multi-block unwind plus deep-reorg full-resync behavior. This does not weaken canonical snapshot publication or checkpoint validation.
- **Acceptance**: an adopt/defer decision includes memory/latency data; if adopted, replayed unwind is state-equivalent to the full-snapshot baseline and deep reorg remains fail-closed.

### M4-9 · `[M4] [Docs] Verify Mantle sequencing, priority-fee, and private-submission behavior`
- **Objective**: Replace assumptions about production Mantle ordering with current authoritative evidence before tuning priority fees or choosing a private submission channel.
- **Blocked By**: none. **Blocks**: future priority-fee/private-routing tuning. **Labels**: current `needs-triage`; target after expansion: `research`, `ready-for-human`.
- **Implementation**: check current official operator/protocol documentation and provider capabilities; where documentation is incomplete, run controlled non-value transaction experiments and record date/network/block range. Distinguish a research proposal from deployed behavior, and public provider APIs from sequencer guarantees.
- **Acceptance**: a dated decision record states what is verified, unknown, and inferred about FCFS/fee ordering/private submission; M0-2 defaults remain conservative until evidence supports a change.

---

## 5. 依赖图（关键路径）

```
M0-1(drain/retire) ─▶ M0-8(final undeployed replacement) ─▶ M0-3(moe protect), M2-1(exec SM)
M0-8 ─▶ M0-9(measure/generate final-codehash gas profiles) ─▶ M0-2(profile runtime/current fee context)
M0-2 ─▶ M1-6(eligibility), M2-1(exec SM)
M0-4(build) ─▶ M0-7(cleanup), M1-1(snapshot/readiness)
M0-5(toolchain) ─▶ M0-7, M2-7(Sepolia E2E)
M0-6(moe pools) ─▶ M1-4(moe snapshot), M3-10(static manifest)

M1-1 ─┬─▶ M1-2 V3 live, M1-5 balance, M1-6, M1-7 backfill, M1-8 block N
       ├─▶ M1-3 V3 coverage ─┐
       ├─▶ M1-4 Moe coverage ├─▶ M2-4 exact diff ─▶ M3-2 shared core, M4-3, M4-4
       └─▶ M1-9 Agni mut ────┘
M0-2 + M0-8 + M1-1 + M1-8 ─▶ M2-1 ─▶ M2-2 pipeline, M2-6 breakers
M2-2 ─▶ M2-3 exact preflight
M0-3 + M0-5 + M1-2 + M1-5 + M1-6 + M1-7 + M2-2 + M2-3 + M2-4 + M2-6 ─▶ M2-7 Sepolia E2E
M0-3 + M1-2 + M1-5 + M1-6 + M1-7 + M2-2 + M2-3 + M2-4 + M2-6 + M2-7 ─▶ M2-8 P2.5 human gate
approved M2-8 ─▶ M2-9 mainnet deploy/verify(paused+unfunded) ─▶ second go/no-go + M2-10 capped fund/canary ─▶ existing-entrypoint production send

approved M2-8 ─┬─▶ M3-2 shared core (also M2-4)
               ├─▶ M3-4 checkpoint (also M1-1/M1-3/M1-4)
               ├─▶ M3-5 PnL (also M2-1)
               ├─▶ M3-7 unwind (also M1-1)
               └─▶ M3-1 merge
M3-1 ─▶ M3-3 path semantics, M3-6 metrics, M3-10 manifest (also M0-6), M4-1 benchmark
M3-1 + M3-3 + M3-5 + M3-6 + M3-10 ─▶ M3-9 post-merge human gate ─▶ M3-8 cleanup/production enable
M3-2 ─▶ M3-8
M3-7 ─▶ M4-8
M4-1 ─▶ M4-2, M4-3, M4-4, M4-5, M4-7, M4-8
```

关键路径：**M0-1 → M0-8 → M0-9 → M0-2 → M1-6/M2-1**（最终未部署 executor、final-codehash measured gas profile、零 estimate gas-sizing 热路径与当前 fee context），**M0-4 → M1-1 → (M1-2/M1-5/M1-6/M1-7) → M2-8**（实时状态、资金边界、资格与缺口回补），**M0-4 → M1-1 → (M1-3/M1-4/M1-9) → M2-4 → M2-8**（状态覆盖与数学正确性），以及 **M0-8 → M0-3/M2-1 → M2-2/M2-3/M2-6 → M2-7 → M2-8**（资金保护、执行状态机和首次测试网部署）。M0-5 还独立阻塞 M2-7。M2-8 的 **approve** 只允许 M2-9 在主网部署并核验，终态必须 paused/unfunded；M2-9 证据再经过第二次人工 go/no-go 才允许 M2-10 限额注资和单笔 canary。M2-8 approve 同时允许开始 M3，但不把既有入口的 signer 资格传给 merged binary。随后可先走 **M3-1 合并**，再由 **M0-6 + M3-1 → M3-10** 替换 legacy universe adapter，最终与 M3-3/M3-5/M3-6 一起进入证据驱动的 M3-9。M3-9 之前 merged binary 不得获得 production signer。

---

## 6. Linear 创建约束（已决）

1. **C1 拆成实现、部署核验、资金暴露三个阶段**：M0-1 只做人工撤资/native 盘点/旧地址永久退役；M0-8 在其后交付 final replacement source/ABI/Rust callers/security tests/reproducible bytecode，但不部署、不注资，因而是 `ready-for-agent`。M2-7 才首次部署到测试网；M2-8 approve 后由 M2-9 这个 `ready-for-human` runbook 在主网部署并核验，但必须停在 paused/unfunded；第二次人工 go/no-go 后，M2-10 才能限额注资并执行单笔 canary。任何 gate 的“完成”本身都不隐式改变链上状态或移动资金。
2. **Milestone 保持 M0–M4**：M1 的 snapshot 基建和协议接入使用 issue 依赖并行，不再增加 milestone 层级。
3. **Labels 只使用现有 canonical 集合**：安全项使用 `bug` + `ready-for-human`，不发明未定义的 `security` label。**Linear 实际标签映射**：`bug`→`Bug`、`feature`→`Feature`（workspace-global 既有大写标签，已复用，不重命名以免影响其他 project）；`research` / `chore` / 全部 triage label 为小写。
4. **M2–M4 默认以 `needs-triage` 占位 issue 形式存在于 Linear**（经用户指示创建，承载依赖图/路线图）：正文是设计摘要而非 canonical 正文。它们必须先展开为 canonical headings、补测试命令/metadata 并复核，才能从 `needs-triage` 转为相应 ready label 并交付实现；在此之前不得被 agent 执行。**不得**在未展开的情况下把某个 M2–M4 issue 标为 ready。例外是只执行批准后链上操作的 M2-9/M2-10，它们从创建起就是完整 canonical 且 `ready-for-human`。
5. **已写入 Linear**（project `Mantle Arbitrage bots v2`，team `WHI`）：5 个 milestone；2026-07-19 最后一次全量回读为 **49 条 issue 记录 = 46 条非重复（45 open/backlog + WHI-500 Done）+ 3 条 Duplicate，86 条唯一原生 `blocks` 边**。所有非重复 issue 都属于一个 milestone，所有依赖 endpoint 都在本项目。非重复 issue 按 milestone 为 M0:9 / M1:9 / M2:9 / M3:10 / M4:9。占位 id → 真实 id 映射（**M0 为非顺序创建，逐条列出**；M1–M4 顺序连续，已合并条目单列）：
   - `M0-1→WHI-500, M0-8→WHI-501, M0-2→WHI-502, M0-3→WHI-503, M0-4→WHI-505, M0-5→WHI-506, M0-6→WHI-507, M0-7→WHI-508, M0-9→WHI-546`
   - `M1-1..M1-9 = WHI-510..518`；`M2-1..M2-4 = WHI-519..522`；`M2-5/WHI-523` 已合并进 `M0-8/WHI-501` 并标 Duplicate；`M2-6..M2-8 = WHI-524..526`；`M2-9→WHI-547, M2-10→WHI-548`；`M3-1..M3-10 = WHI-527..536`；`M4-1..M4-9 = WHI-537..545`
   - Duplicate 共三条：创建期两笔超时重发 `WHI-504`、`WHI-509` 分别指向 `WHI-503`、`WHI-508`；被 final executor ABI 吸收的 `WHI-523` 指向 `WHI-501`，且不再有任何 `blocks/blocked-by` 边。
6. **暂不估点**：模板未强制 estimate；只有团队要求时再按统一 Fibonacci 规则补充。
7. **P2.5 gate 例外只有两项**：M4-6 是不影响 runtime 的可逆 dev-profile 修改，M4-9 是只收集证据的 research；它们可以提前进行，但不得获得 signer、改变生产优先费或被当作 M3/M4 工程开工。
8. **active-entrypoint 规则优先**：P3 合并前，四个 service 的 cross-cutting 修复必须全部接线；未迁移入口只能通过显式 deployment + Cargo retirement 排除，不能靠“默认不用”通过验收。
