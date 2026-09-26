# arbitrage-bot-v3 — Design Document / PRD (index)

> **Status: index, not yet a consolidated PRD.** This repo predates the template's
> single-file spec of record. The design lives in `specs/` (Chinese, audited and
> reviewed in several rounds), `tech-docs/` and the Linear project. This file keeps the
> template's **stable section numbers** so skills and issues can cite `docs/DESIGN.md §N`,
> and each section points at where that content lives today.
>
> Consolidating it into a real PRD is a separate `/to-spec` pass: fill the sections in
> place, keep the numbers, and delete the pointers as content replaces them. Until then,
> treat the linked documents as the spec of record and do not re-derive what they already
> decided.

## 1. Background & Goals

- Vision, current state and the recovery roadmap: `specs/README.md`.
- Go-live scope and gates: `specs/07-go-live-hardening.md`.
- Project facts for agents: `AGENTS.md` § What this is, § Status.

## 2. Requirements / Specification

- Diagnosed defects and the contracts that close them: `specs/01-current-issues.md`.
- Missing production features: `specs/04-missing-features.md`.
- Go-live hardening requirements: `specs/07-go-live-hardening.md`.
- Added directions (static pool/path manifest, Sepolia E2E): `specs/TODOs.md`.
- Per-issue requirements: the Linear issue (`docs/agents/issue-template.md`).

## 3. Cross-cutting Policies

- Funds safety and the human-gated funds path: `AGENTS.md` § High-risk paths.
- Fail-closed runtime behaviour (chain id, frozen pool universe, gas profiles, send
  gate): `AGENTS.md` § Runtime configuration, § Frozen pool universe.
- Performance budgets and hot-path cost: `specs/03-performance.md`.

## 4. System Architecture

### 4.1 Tech stack

Rust (pinned in `toolchain.toml` / `rust-toolchain.toml`), Foundry + solc for the
Solidity side (`contracts/`). See `AGENTS.md` § Toolchain pins.

### 4.2 Module layout

`AGENTS.md` § Architecture is the current mirror. Target architecture and refactor plan:
`specs/02-architecture-refactor.md`. Module deep dives: `tech-docs/General/`.

### 4.3 Key interfaces

`AutomatedMarketMaker` / `AMM`, `AutomatedMarketMakerFactory` / `Factory`, `AMMFilter`,
the service `Protocol` trait: `AGENTS.md` § Architecture, `tech-docs/General/`.

### 4.4 Core flows

Events → state sync → path finding → optimization → execution:
`tech-docs/General/Architecture-Overview.md`, `tech-docs/General/Module-Integration.md`.

### 4.5 State & recovery

State space and reorg cache: `tech-docs/General/StateSpace-Module.md`. Breaker / WAL
seams and what is not wired yet: `docs/DEFERRED_ISSUES.md` (DI-12).

## 5. Data & Observability

Shadow ledger and evidence under `evidence/`; daily Lark digest (`src/notify/`,
`src/bin/lark_daily_digest.rs`); Dune competitor monitoring (`scripts/dunesql/`).

## 6. Milestones

Milestones and issue drafts: `specs/06-milestones-and-issues.md`,
`specs/07-go-live-hardening.md`. Live state: the Linear project
`Mantle Arbitrage bots v2`. Issue titles carry the `[X.Y.Z]` version prefix, not a
milestone tag (`docs/agents/issue-template.md` § Title convention).

## 7. Rejected Alternatives

- Review verdicts and corrected conclusions: `specs/00-review-response.md`.
- Intentional designs not to "fix": `docs/DEFERRED_ISSUES.md` § Design notes.
- Later narrow decisions: `docs/adr/` (created lazily).

## 8. Known Risks & Open Questions

- Accepted debt: `docs/DEFERRED_ISSUES.md` § Open.
- Go-live risks and remaining gates: `specs/07-go-live-hardening.md`.
