# Issue Template

The canonical shape for every Linear issue in this workspace. Copy the skeleton in
[§ Copy-paste skeleton](#copy-paste-skeleton), fill each section, and delete the inline
guidance (the `> _italic_` hints). **All issue content is written in English.**

This template encodes the conventions already in use across the workspace's
best-structured projects. It pairs with:

- `docs/agents/issue-tracker.md` — how to read/write Linear via the MCP tools.
- `docs/agents/triage-labels.md` — the five canonical triage labels.

---

## Principles

A good issue is a **self-contained unit of work**: a competent engineer (or an AFK agent)
can open it cold and know *what* to build, *why*, *where* in the codebase, what it
*depends on*, and *how to know it's done* — without asking a follow-up question.

1. **One issue = one deliverable.** If it needs "and" in the objective, split it.
2. **Concrete over abstract.** Name the files, functions, contracts, and env vars.
   Reference `path/to/file.rs:42`, not "the executor module."
3. **Verifiable.** Every issue ends in acceptance criteria that are objectively
   checkable (a test passes, a command exits 0, a value appears on-chain).
4. **Dependencies explicit.** State what blocks this and what this unblocks, so the
   work can be ordered and parallelized.
5. **Scoped.** Say what is *out* of scope as clearly as what is in.

---

## Title convention

```
[Mn] [Component] <imperative, specific description>
```

- **`[Mn]`** — the milestone this issue belongs to (`[M0]`, `[M1]`, …). Omit only for
  standalone issues with no milestone.
- **`[Component]`** — the subsystem or pipeline stage. For this repo, prefer the module
  names: `[amms]`, `[state-space]`, `[arbitrage]`, `[execution]`, plus cross-cutting tags
  `[Infra]`, `[Contracts]`, `[Bench]`, `[Docs]`, `[CI]`. Add a protocol qualifier when
  useful: `[amms/moe]`, `[amms/agni]`.
- **Description** — short, specific, and imperative. Prefer
  `Add MoE Liquidity Book tick math` over `MoE stuff`.

Optionally append the key techniques/scope in parentheses to disambiguate at a glance:
`[M1] [execution] Nonce manager (per-account cache + gap recovery + resync)`.

**Examples**

```
[M0] [Infra] Set up mock arbitrage harness replaying logs/pool_updates.csv
[M1] [amms/uniswap_v3] Implement batch-request pool state reader
[M1] [arbitrage] Binary-search optimal input size in PathOptimizer
[M2] [execution] Enforce non-negative-profit pre-flight check
[M2] [Contracts] Add reentrancy guard to ArbitrageExecutor.executeArb
```

---

## Metadata (set on the Linear issue, not in the body)

| Field        | How to set it                                                            |
| ------------ | ------------------------------------------------------------------------ |
| **Milestone** | Attach to the project milestone matching the `[Mn]` title tag.          |
| **Priority**  | `Urgent` / `High` / `Medium` / `Low` — see the table below.             |
| **Labels**    | Triage role from `triage-labels.md` + any type label (`bug`, `feature`, `research`, `chore`). |
| **Assignee**  | Set when claimed; leave empty in the backlog.                           |
| **Relations** | Use Linear's native `blocks` / `blocked-by` relations (see below).      |

### Priority guide

| Priority   | Use when…                                                                       |
| ---------- | ------------------------------------------------------------------------------- |
| **Urgent** | Blocks a milestone, or a critical path item others depend on. Do first.         |
| **High**   | Core deliverable of the milestone; needed for it to be "done."                  |
| **Medium** | Valuable but not blocking; can slip a milestone without derailing it.           |
| **Low**    | Nice-to-have, polish, or opportunistic cleanup.                                 |

---

## Body sections

Fill the sections below in order. Sections marked _(optional)_ may be dropped when they
add nothing; keep the rest even if brief.

### `## Objective`
One or two sentences: what this issue delivers and why it matters. Written so the reader
understands the outcome before any implementation detail. This mirrors the "Success = …"
framing used in milestone descriptions.

### `## Context` _(optional)_
Background a newcomer needs: the problem this solves, links to prior research/ADRs, the
relevant on-chain reality (e.g. "WMNT is the gas token on Mantle, not WETH"). Skip if the
Objective is fully self-explanatory.

### `## Blocked By` / `## Blocks`
Dependency graph. List issue identifiers (e.g. `WHI-42`) and a short reason. Prefer to
*also* wire these as Linear `blocked-by` / `blocks` relations; the body lines are the
human-readable mirror. Use `None (entry point)` when there are no blockers.

### `## Implementation`
The plan of record. Numbered steps, each anchored to a concrete file/module. Include:
- Exact file paths to create or modify.
- Function/type signatures, DDL, ABI fragments, or config keys where they pin the design.
- Code blocks for anything the implementer must follow verbatim.
This is the section that makes an issue AFK-ready — be generous with specifics.

### `## Out of scope` _(optional)_
Explicitly list what this issue does **not** cover, to prevent scope creep and to signal
where follow-up issues pick up.

### `## Acceptance criteria`
A checklist of objectively verifiable conditions. Each item is a fact someone can confirm:
a passing test, a `cargo` command that exits clean, an on-chain effect, a benchmark
threshold. If you can't check it, it's not a criterion — rewrite it.

### `## Testing / Verification` _(optional)_
How to prove the criteria hold: the exact commands (`cargo test <name>`,
`cargo run --example mock_arbitrage`), fixtures/logs to replay, or the manual steps and
expected output. Merge into Acceptance criteria for small issues.

### `## References` _(optional)_
Links to designs, ADRs, upstream `amms-rs` code, protocol docs, prior issues, or Dune
queries. Bare URLs are fine.

---

## Copy-paste skeleton

```markdown
## Objective
> _One or two sentences: what this delivers and why. State the success outcome._

## Context
> _(optional) Background, prior research/ADR links, relevant on-chain facts._

## Blocked By
> _Issue ids + one-line reason, or `None (entry point)`._

## Blocks
> _Issue ids this unblocks, or `None`._

## Implementation
> _Numbered, file-anchored plan. Signatures, DDL, ABI, config keys, code blocks._
1.
2.
3.

## Out of scope
> _(optional) What this issue deliberately does not cover._

## Acceptance criteria
- [ ]
- [ ]
- [ ]

## Testing / Verification
> _(optional) Exact commands / fixtures / manual steps + expected output._

## References
> _(optional) Links to designs, ADRs, docs, prior issues._
```

---

## Worked example

Title: `[M1] [execution] Nonce manager with per-account cache and gap recovery`
Milestone: `M1: Executable Single-Path Arbitrage` · Priority: `High` ·
Labels: `feature`, `ready-for-agent`

```markdown
## Objective
Provide a `NonceManager` that hands out monotonically increasing nonces for the executor
wallet under concurrent submissions, and recovers cleanly after a dropped/replaced tx, so
`Executor` never stalls on a `nonce too low` / `nonce gap` error.

## Context
`src/execution/nonce.rs` currently reads the on-chain nonce per transaction, which races
under back-to-back submissions and re-fetches on every send. Mantle's WMNT-denominated gas
path and our aggressive resubmission make a local, self-healing nonce cache necessary.

## Blocked By
- WHI-18 — `Executor` transaction builder must exist first.

## Blocks
- WHI-25 — parallel multi-path submission depends on a concurrency-safe nonce source.

## Implementation
1. `src/execution/nonce.rs`: add `NonceManager { inner: Mutex<NonceState> }` with
   `async fn next(&self) -> u64` and `async fn resync(&self) -> Result<(), NonceError>`.
2. On first use, seed from `provider.get_transaction_count(addr, Pending)`.
3. `next()` returns the cached value and increments; never awaits RPC on the hot path.
4. On a send error classified as `NonceTooLow`/`NonceGap` (see `execution/error.rs`),
   call `resync()` to re-seed from the chain and retry once.
5. Thread the manager through `Executor::new` instead of the per-tx read at the call site.

## Out of scope
- Cross-process nonce coordination (single-process assumption holds for now).
- Replacement-by-fee bumping — tracked separately in WHI-26.

## Acceptance criteria
- [ ] `cargo test nonce` passes, including a concurrent-`next()` test asserting no dupes.
- [ ] A simulated `NonceGap` error triggers exactly one `resync()` + retry, then succeeds.
- [ ] `cargo run --example mock_arbitrage` submits ≥2 back-to-back txs with no nonce error.

## Testing / Verification
`cargo test --lib execution::nonce` for unit coverage; replay with
`cargo run --example mock_arbitrage` and confirm the logs show sequential nonces.

## References
- Upstream reference: amms-rs executor nonce handling.
- `src/execution/error.rs` for the error taxonomy.
```

---

## Appendix: Milestone naming

Issues carry an `[Mn]` tag because they live under a **milestone**. Keep milestones
consistent so the tags stay meaningful:

- **Name**: `Mn: <Theme>` — e.g. `M0: Scaffold + Mock Harness`,
  `M1: Executable Single-Path Arbitrage`, `M2: Multi-Path + Hardening`.
- **Description**: one paragraph stating the scope, the rationale, and an explicit
  success line: `Success = <objectively checkable outcome>`.
- **Ordering**: `M0` is scaffolding/setup; each subsequent milestone should be
  independently demoable and build on the last.
