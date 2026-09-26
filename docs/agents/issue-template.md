# Issue Template

The canonical shape for every Linear issue in this workspace. Copy the skeleton in
[§ Copy-paste skeleton](#copy-paste-skeleton), fill each section, and delete the inline
guidance (the `> _italic_` hints). **All issue content is written in English.**

It pairs with:

- `docs/agents/issue-tracker.md` — how to read/write Linear via the MCP tools.
- `docs/agents/triage-labels.md` — the five canonical triage labels.
- `docs/DESIGN.md` — the spec index these issues implement (it points into `specs/` and
  `tech-docs/`); every issue should trace back to a section there.

---

## Principles

A good issue is a **self-contained unit of work**: a competent engineer (or an AFK agent)
can open it cold and know *what* to build, *why*, *where* in the codebase, what it
*depends on*, and *how to know it's done* — without asking a follow-up question.

1. **One issue = one deliverable.** If it needs "and" in the objective, split it.
2. **Concrete over abstract.** Name the files, functions, contracts, env vars, config
   keys, and the spec section. Reference `path/to/file.rs:42` or
   `specs/02-architecture-refactor.md`, not "the executor module".
3. **Verifiable.** Every issue ends in acceptance criteria that are objectively
   checkable (a test passes, a `cargo` command exits clean, a ledger row or on-chain
   effect appears).
4. **Dependencies explicit.** State what blocks this and what this unblocks, so the
   work can be ordered and parallelized. Also state the files, modules and shared
   contracts it expects to touch (`## Execution`) — two issues with no `blocks` edge can
   still collide there.
5. **Scoped.** Say what is *out* of scope as clearly as what is in.
6. **No new parameters without a source.** Anything that reads as a tunable parameter
   must cite where it comes from (a spec section, a measured artifact under `evidence/`,
   `docs/references/`), or be explicitly flagged as a new, unvalidated parameter.

---

## Title convention

```
[X.Y.Z] [Component] <imperative, specific description>
```

- **`[X.Y.Z]`** — the **version / Release** this issue ships in (e.g. `[0.2.0]`).
  This is the **primary git-routing signal** (`docs/GIT_WORKFLOW.md` § Resolving
  the base branch). An implementing agent that cannot read a version prefix, or
  whose prefix disagrees with the tracker's release field, **refuses to start**.
  Omit the prefix only for repo-wide governance (the carve-out file list in
  that section) — those issues target `dev` and have no version. A `hotfix`-labelled
  issue keeps a prefix too, using the four-segment hotfix version (`[0.1.5.1]`, per
  `docs/GIT_WORKFLOW.md` § Version axis), but it routes off the **label**, not the
  prefix.
- **`[Component]`** — the subsystem or pipeline stage, using the module names in
  `AGENTS.md` § Architecture: `[amms]`, `[state-space]`, `[arbitrage]`, `[execution]`,
  `[service]`, `[signing]`, `[notify]`, `[bin]`, plus cross-cutting tags `[Config]`,
  `[Infra]`, `[Contracts]`, `[Bench]`, `[Docs]`, `[CI]`. Add a protocol qualifier when
  useful: `[amms/moe]`, `[amms/agni]`.
- **Description** — short, specific, and imperative. Prefer
  `Add MoE Liquidity Book tick math` over `MoE stuff`.

**Legacy titles.** Issues created before WHI-1497 carry a milestone-style tag
(`[M2]`, `[Go-Live]`) instead of a version. Before one is started, the owner or the
orchestrator replaces that tag with the `[X.Y.Z]` prefix matching its Release (or
confirms it is governance). An implementer does not infer the version from the old tag.

**Examples**

```
[0.2.2] [service] Reconcile the optimize/materialize route-key contract
[0.2.2] [amms/uniswap_v3] Implement batch-request pool state reader
[0.2.2] [Contracts] Add reentrancy guard to ArbitrageExecutor.executeArb
[Infra] Sync the agent process layer from code-template v0.2.0   <- governance, no version
```

Do **not** put the milestone in the title. Milestone and Release can both render
as `0.2.0` and have already disagreed in practice (title `[0.2.0]`, milestone
`0.3.0`). Milestone is a tracker field; the title prefix is the version.

---

## Metadata (set on the Linear issue, not in the body)

| Field         | How to set it                                                            |
| ------------- | ------------------------------------------------------------------------ |
| **Project**   | `Mantle Arbitrage bots v2` (always — the only project in scope for this repo). |
| **Release**   | Required-by-convention for every version-scoped issue. Must match the `[X.Y.Z]` title prefix. **A missing Release blocks implementation** — the agent refuses rather than guessing `dev`. This table is a prompt, not a gate: trackers generally do not enforce non-empty fields; enforcement is the refusal in `docs/GIT_WORKFLOW.md`. Omit only for repo-wide governance (no version prefix). |
| **Milestone** | Capability stage (`specs/06-milestones-and-issues.md` and the Linear project's milestones). Orthogonal to Release. Do not use it to express the version or to route git. |
| **Priority**  | `Urgent` / `High` / `Medium` / `Low` — see the table below.             |
| **Labels**    | Triage role from `triage-labels.md` + any type label (`bug`, `feature`, `research`, `chore`, `hotfix`). `hotfix` changes the git base branch — see `triage-labels.md`. |
| **Assignee**  | Set when claimed; leave empty in the backlog.                           |
| **Relations** | Use Linear's native `blocks` / `blocked-by` relations.                  |

### Priority guide

| Priority   | Use when…                                                                      |
| ---------- | ------------------------------------------------------------------------------ |
| **Urgent** | Blocks a milestone, or is a safety item on the funds path (`AGENTS.md` § High-risk paths). Do first. |
| **High**   | Core deliverable of the milestone; needed for it to be "done."                 |
| **Medium** | Valuable but not blocking; can slip a milestone without derailing it.          |
| **Low**    | Nice-to-have, polish, or opportunistic cleanup.                                |

---

## Body sections

Fill the sections below in order. Sections marked _(optional)_ may be dropped when they
add nothing; keep the rest even if brief.

### `## Objective`
One or two sentences: what this issue delivers and why it matters.

### `## Context` _(optional)_
Background a newcomer needs: the relevant spec section, prior research/ADR links, the
relevant on-chain reality (e.g. "WMNT is the gas token on Mantle, not WETH"). Skip if
the Objective is fully self-explanatory.

### `## Blocked By` / `## Blocks`
Dependency graph. List issue identifiers (e.g. `WHI-42`) and a short
reason. Prefer to *also* wire these as Linear `blocked-by` / `blocks` relations; the
body lines are the human-readable mirror. Use `None (entry point)` when there are no
blockers.

### `## Execution`
Required. Drives scheduling and the implementer's effort (`docs/agents/runtime.md`).

- **Complexity:** exactly one of `medium` | `high`.
  - `medium` — local, the pattern is clear, acceptance is direct, no real cross-module
    reasoning.
  - `high` — cross-module behaviour, concurrency or recovery logic, data consistency, a
    security boundary, a complex migration, or a root cause that is hard to locate.
  Complexity is separate from priority, size and human-review risk: a tiny change can be
  `high`, a large mechanical one need not be.
- **Reason:** one sentence on what makes it hard (or not).
- **Expected scope:** the files, modules and shared contracts (schema, public interface,
  generated file) it expects to change.

The designer sets it in `/to-tickets`. The orchestrator may raise it to `high` on
evidence and records why on the issue; it is never silently lowered. An issue missing this
section is completed before it is scheduled — no guessing.

### `## Implementation`
The plan of record. Numbered steps, each anchored to a concrete file/module. Include:
- Exact file paths to create or modify.
- Function/type signatures, ABI fragments, config keys or schema where they pin the
  design.
- Code blocks for anything the implementer must follow verbatim.
This is the section that makes an issue AFK-ready — be generous with specifics.

### `## Out of scope` _(optional)_
Explicitly list what this issue does **not** cover, to prevent scope creep and to signal
where follow-up issues pick up.

### `## Acceptance criteria`
A checklist of objectively verifiable conditions. Each item is a fact someone can
confirm: a passing test, a log line, a stored record, a delivered notification. If you
can't check it, it's not a criterion — rewrite it.

### `## Testing / Verification` _(optional)_
How to prove the criteria hold: the exact commands (`cargo test --locked <name>`,
`cargo run --bin bot -- --offline`), fixtures/logs to replay, or the manual steps and
expected output. Merge into Acceptance criteria for small issues.

### `## References` _(optional)_
Links to `docs/DESIGN.md` sections, `docs/references/`, prior issues, or external docs.
Bare URLs are fine.

---

## Copy-paste skeleton

```markdown
## Objective
> _One or two sentences: what this delivers and why. State the success outcome._

## Context
> _(optional) Background, docs/DESIGN.md section, platform facts._

## Blocked By
> _Issue ids + one-line reason, or `None (entry point)`._

## Blocks
> _Issue ids this unblocks, or `None`._

## Execution
> _Keep exactly one Complexity value. Reason: one sentence on the hard part. Expected
> scope: files, modules and shared contracts this changes._
- Complexity: medium | high
- Reason:
- Expected scope:

## Implementation
> _Numbered, file-anchored plan. Signatures, config keys, schema, code blocks._
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
> _(optional) Links to docs/DESIGN.md sections, references/, prior issues._
```

---

## Worked example

Title: `[0.2.2] [execution] Nonce manager with per-account cache and gap recovery`
Release: `0.2.2` · Milestone: `M1: Executable Single-Path Arbitrage` · Priority: `High` ·
Labels: `feature`, `ready-for-human` (it touches `src/execution/`, the funds path)

```markdown
## Objective
Provide a `NonceManager` that hands out monotonically increasing nonces for the executor
wallet under concurrent submissions, and recovers cleanly after a dropped/replaced tx, so
`Executor` never stalls on a `nonce too low` / `nonce gap` error.

## Context
`src/execution/nonce.rs` currently reads the on-chain nonce per transaction, which races
under back-to-back submissions and re-fetches on every send.

## Blocked By
- WHI-18 — `Executor` transaction builder must exist first.

## Blocks
- WHI-25 — parallel multi-path submission depends on a concurrency-safe nonce source.

## Execution
- Complexity: high
- Reason: concurrency plus recovery logic on the send path.
- Expected scope: `src/execution/nonce.rs`, `src/execution/executor.rs`
  (`Executor::new` signature), `src/execution/error.rs`.

## Implementation
1. `src/execution/nonce.rs`: add `NonceManager { inner: Mutex<NonceState> }` with
   `async fn next(&self) -> u64` and `async fn resync(&self) -> Result<(), NonceError>`.
2. On first use, seed from `provider.get_transaction_count(addr, Pending)`.
3. `next()` returns the cached value and increments; never awaits RPC on the hot path.
4. On a send error classified as `NonceTooLow`/`NonceGap`, call `resync()` and retry once.
5. Thread the manager through `Executor::new` instead of the per-tx read at the call site.

## Out of scope
- Cross-process nonce coordination (single-process assumption holds for now).
- Replacement-by-fee bumping — tracked separately in WHI-26.

## Acceptance criteria
- [ ] `cargo test --locked nonce` passes, including a concurrent-`next()` test asserting
      no duplicates.
- [ ] A simulated `NonceGap` error triggers exactly one `resync()` + retry, then succeeds.

## Testing / Verification
`cargo test --locked --lib execution::nonce`.
```

---

## Appendix: Milestones

Issues carry an `[X.Y.Z]` title prefix because they belong to a **Release**
(the git-routing signal). They may also sit under a **milestone** (capability
stage) — that is a tracker field, not the title tag. Milestones are named
`Mn: <Theme>` (or a named stage such as `Go-Live: first funded canary`) with a
`Success = …` line in their description; the roadmap lives in `specs/06-milestones-and-issues.md` and Linear.
If Releases change, that is the tracker Release entity, not this file.
