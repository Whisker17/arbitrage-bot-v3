---
name: implement
description: "Implement one tracker issue in its own worktree: ponytail, relevant checks, PR and handoff."
disable-model-invocation: true
---

Implement one issue (or the bounded work the user describes) end to end. You are the
`IMPLEMENTER` role (`docs/agents/runtime.md`).

## Start

1. Read the whole issue, including amendments and its `## Execution` section, the spec
   sections it cites, and only the other docs the task needs. If complexity or expected
   scope is missing, get it fixed before starting.
2. Resolve the base and create the worktree exactly as `docs/GIT_WORKFLOW.md`
   § Resolving the base branch says — fail closed, never a defaulted `dev`, never create
   `release/v*`. Assert the base, enable hooks in the worktree.
3. Tracker → `In Progress` (standalone). Under `/orchestrate` the orchestrator owns the
   tracker; you report facts.

## Build and verify

- Apply `.claude/skills/ponytail/SKILL.md` while writing.
- Stay inside the issue's expected scope. If the work needs more, stop and report
  instead of widening it.
- Fix the acceptance target before coding; you do not have to write every test first.
- While working, run the affected tests, lint/type checks and any targeted E2E. Do not
  rerun the full E2E suite over and over.
- Changes with branching, parsing, concurrency, money or security logic leave a runnable
  check that would fail if the behaviour broke. Prefer checks of external behaviour over
  assertions that restate the implementation. E2E and module tests are both fine.
- On the final HEAD, use the three tiers of `docs/GIT_WORKFLOW.md` § 2 Implement:
  - the **required project checks** (CI, and what `AGENTS.md` marks for every merge)
    always;
  - the **relevant issue checks** always;
  - the **full suite** for standalone, governance and hotfix work. Only an ordinary
    version or bootstrap issue under `/orchestrate` defers it to the release candidate.
- If the base advanced, update the branch and rerun the affected checks. An earlier
  HEAD's pass does not count.

## PR and handoff

1. Commit, `git push -u origin HEAD`, `gh pr create --base <resolved-base>`. The body
   (`.github/pull_request_template.md`) carries the issue id, the resolved base and its
   signals, the evidence (SHA, commands, real results, artifacts), the role/model/effort
   you ran as, and why any new dependency or significant abstraction is needed. Tracker →
   `In Review` (standalone).
2. Write a handoff with `/handoff`: its durable facts belong in the PR or issue.

## Merge

Authority comes only from `docs/GIT_WORKFLOW.md` § Merge authorization.

- **Under `/orchestrate`:** stop at PR + handoff. The orchestrator verifies and merges.
- **Standalone** (no release orchestration takes over): run one independent PR review with
  `/code-review` in PR mode (`REVIEWER`, effort `high`, a real dispatch — not your own
  context). Fix accepted blocking findings; a suggestion you consciously leave that is
  worth remembering goes in `docs/DEFERRED_ISSUES.md` with its reason. If you changed anything after the review, the
  reviewer checks the final commit before merge. Then, unless a human-gated row applies:
  PR MERGEABLE/CLEAN, checks green on the final HEAD, `gh pr merge <N> --squash
  --delete-branch`, post-merge cleanup and fan-out per `docs/GIT_WORKFLOW.md`, tracker →
  `Done`.
- **Human-gated** (high-risk paths, hotfix → `main`, `release/*` → `main`, finished
  `release/v*` → `dev`): stop at `In Review`.
- `REVIEWER` unavailable or the dispatch failed: stay at `In Review` and say which role
  was missing. Never substitute a self-review.
