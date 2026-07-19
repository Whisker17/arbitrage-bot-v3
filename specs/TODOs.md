# TODO

1. 我希望整个 arbitrage bots 的性能尽可能高，所以我希望链上的路径是静态的，而不是每一次都要去动态地查询。因此，我认为我们需要提供一个 poolList 的名单，里面包含了所有可能出现套利机会的池子。因为我们知道目前 Mantle 链上的池子数量是有限的，且并不是很多。所以我认为可以写一个脚本，去获取每一个 DEX 当中所有的池子，然后我们再根据 atomic arbitrage 的逻辑，最多可以选到 4 hops 的套利路径，我们从所有的池子中筛选出所有可以组成套利路径的池子。另外，我认为还要对池子的 TVL 有一定的要求，如果太低的话，滑点可能太大。我认为这个 poolList 的发现可以做成一个脚本，定期运行一下，更新我们静态维护的 poolList 列表。

## Review 后的落地结论

这个方向适合当前 bot：**把拓扑发现移出热路径**，运行时加载静态、版本化的 pool/path manifest；但“静态”只限于 factory/pool/token/path 关系。pool reserves、V3 ticks、Moe bins、余额、base fee 和 preflight 状态仍必须按每个 canonical `MarketSnapshot` 动态更新，不能把静态 poolList 扩大解释成静态报价。

“所有可能出现套利机会”只能定义为：在某个 block、factory set、settlement asset、`max_hops=3`（**已从 4 收敛到 3**,依据 [`ARB_PATHS_MANTLE.md`](/Users/whisker/Work/src/personal/mantle-arbitrage-system/arbitrage-analysis/ARB_PATHS_MANTLE.md) §4:2–3 跳=93.5%,4+ 仅 6.5% 且利润可忽略）和筛选策略下图论上可组成的闭环集合。TVL/depth threshold 会主动牺牲召回率，未来新 pool/流动性迁移也会让旧 manifest 过时；文档和指标不能把这个配置化 candidate universe 宣称为数学上完整的未来机会全集。

### Manifest 生成契约

1. 离线脚本从配置的 canonical factory 集合枚举 pools，逐个验证 factory provenance、协议类型、token pair/direction 和协议参数。
2. 构建 token/pool 图，只保留至少参与一个 `settlement_asset` 闭环、且 hop 数 `<= 3` 的 pool（`max_hops=3`,见上,原请求的 4 已收敛)；同时输出完整有序 `(pool, token_in, token_out)` path 候选，不能按无序 pool 集合去重。
3. TVL 不能只靠未注明来源的美元数字。manifest 必须记录 token decimals、估值源/版本/时间、`min_tvl` 策略；更推荐同时记录标准 probe size 下的可执行深度/price impact。缺可靠估值源时 fail closed 或进入人工 review，不能把未知值当 0/无限大。
4. 输出包含 `schema_version`、`chain_id`、生成所用 `block_number + block_hash + timestamp`、factory-set fingerprint、strategy/config fingerprint、TVL/depth policy、tool version、pool/path payload 和完整内容 digest。相同输入必须产生确定性排序和稳定 diff。
5. 周期任务只生成 candidate manifest 和 diff，不直接改变 production。由人工审核新增/删除 pool、估值异常和路径变化后 promotion；保留上一版本和审计记录。

### 运行时契约

- 启动时校验 digest、chain/factory/config fingerprint、每个 pool provenance 和 token pair，再从 manifest 构建静态 path cache / pool-to-path index。
- promotion/reload 先停止新 nonce intent；等待在途 intent canonical-finalize，或 best-effort cancel 并由旧 calldata deadline 兜底。随后在 block boundary 使 `SnapshotStatus` 离开 `Ready`，清空所有旧 candidate/queue；重新确认 manifest 生成块仍 canonical，对新 pool universe 完成全量 state/coverage sync 和 path-cache 原子重建后，才以包含 `pool_universe_fingerprint/manifest_version`（或等价单调 readiness epoch）的新执行身份重新 `Ready`。candidate/send 必须携带并核对该身份，不能把新旧 manifest/池状态混在一个 candidate 中。
- manifest 过期、损坏、链不匹配或关键 pool 验证失败时 fail closed 并告警；不得悄悄退回另一个协议的 CSV。
- discovery manifest 是**交易候选 universe**，不是 executor 的安全授权。若 executor 使用 canonical allowlist，promotion 必须走独立、owner-controlled、可审计的变更；定时脚本绝不能自动授予链上提款/callback 信任，runtime 可执行集合只能是已审核 manifest 与当前 owner-controlled allowlist 的交集。

对应实施 issue 见 `06-milestones-and-issues.md` 的 M3-10；M0-6 只先修复当前 Moe CSV 缺失和错误 fallback。

---

2. 需要在 Mantle Sepolia 上实现测试，所以可以的话请你配置好所有的环境（发币，几个 DEX 池子，组好池子，然后触发 swap，完成套利），实现整个 e2e 的流程，然后整个流程是可复用的（当然，发币和组池子这些并不需要每次都要去做。你可以通过一个脚本去触发一次 swap，之后你的整个 arbitrage BOT 就会在 Mantle Sepolia 上完成套利。）

## TODO 2 Review 后的落地结论

这个需求应作为 P2 的**独立、credentialed Mantle Sepolia E2E gate**，并在 P2.5
signerless shadow gate 之前完成。现有 `execute_sepolia_arbitrage` target 已被 A3 证实使用
过时 API、当前不能编译；旧 QUICK_START/DEPLOYMENT_GUIDE 和历史硬编码地址只能作为
迁移线索，不能当作现成可复用环境或当前 canonical deployment 证据。

### 环境与安全边界

- E2E 只接受专用测试网 key，启动时查询并校验预期 Mantle Sepolia chain id（当前仓库
  假设为 `5003`，实施时须以当时官方网络资料/RPC 复核），并显式拒绝 Mantle mainnet
  `5000`。production signer、production executor 地址和 mainnet RPC 不得注入该流程。
- 先盘点当时 Mantle Sepolia 上可验证的 canonical factory/router/pool 部署。若公开部署
  不完整、不可控或 ABI/provenance 无法证明，就部署 repo-owned test factories/pools；
  fixture 必须与 bot 支持的真实协议 ABI/关键字节码行为等价，并在报告中明确它验证的是
  pipeline/adapter，而不是冒充 production DEX 部署。
- 至少形成两个 venue/pool 的 WMNT 闭环和一个可重复制造的套利机会；每个 pool 的
  factory provenance、token pair、初始流动性与协议参数都进入部署 manifest。

### 一次性 bootstrap 与重复运行

- `bootstrap` 幂等完成测试 token、factory/pool、初始流动性、M0-8 exact hardened executor 部署与
  测试库存注资；已有环境必须先核对 chain id、address、deployment tx、runtime codehash、
  owner、schema/config fingerprint，匹配才复用，不匹配 fail closed，不能静默覆盖。
- 独立 `trigger` 脚本只对一个受控 pool 发起可参数化 swap 制造价差，不重复发币/建池。
  它记录 before/after reserve、tx hash 和触发区块，且可在 bot 完成套利后再次运行。
- E2E orchestrator 启动已修复的真实 bot pipeline，执行
  `Ready snapshot → candidate → measured gas-profile lookup + current fee context → exact-request preflight → broadcast → canonical-finalized receipt`，
  并断言 executor settlement-token delta、sender receipt gas、nonce intent/attempt、deadline 和
  realized PnL 对账一致。超时/失败必须输出可诊断证据并停止，不以“发出交易”冒充成功。
- live testnet 流程不进入无凭证的普通 CI；提供显式命令，并作为手动或 scheduled
  credentialed gate 运行。普通 CI 保留离线/本地 deterministic counterpart。

对应实施 issue 见 `06-milestones-and-issues.md` 的 M2-7；这是 replacement 的首次网络部署，
它通过后才允许进入 M2-8。M2-8 approve 只解锁 M2-9 人工主网部署/核验 runbook，
其终态必须是 paused + unfunded；M2-9 证据经第二次人工 go/no-go 后，M2-10 才能
限额注资和执行 canary。任何 gate 本身都不自行部署或移动 production funds。
