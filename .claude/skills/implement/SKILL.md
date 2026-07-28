---
name: implement
description: "Implement a piece of work based on a spec or Linear issue."
disable-model-invocation: true
---

Implement the work described by the user in the spec or Linear issue(s), following this
repo's mandatory Git workflow (`CLAUDE.md` → "Git workflow (mandatory)").

## 0. Set up the worktree

**Never implement in the primary clone.** Per `CLAUDE.md`:

```bash
git fetch origin
git checkout dev && git pull --ff-only origin dev
git worktree add -b <fix|feat|chore>/whi-<id>-<topic> <worktree-path> origin/dev
cd <worktree-path>
```

Set the Linear issue to **`In Progress`** (see `docs/agents/issue-tracker.md` for the
Linear MCP calls). If no Linear issue exists yet for this work, ask the user before
inventing one.

## 1. Implement

Use `/tdd` where possible, at pre-agreed seams.

Run `cargo build --locked` (or `cargo check`) regularly, single test files regularly
(`cargo test --locked <name>`), and the full suite (`cargo test --locked`) once at the
end. If `contracts/` changed, also run the Foundry test suite and rebuild ABIs with
`SKIP_FORGE=0 cargo build`.

## 2. Review loop

Once the implementation and tests are green, use `/code-review` to review the work
(it runs its review sub-agents on Claude Opus 5 — no separate manual review pass is
needed afterwards).

Then close the loop, bounded at **three review rounds** followed by an Opus 5
escalation pass:

1. **Round 1** — fix the findings from the first review (or consciously decide not to,
   with a reason).
2. **Round 2** — re-run `/code-review` to verify the round-1 fixes didn't miss the point
   or introduce new issues; fix what it reports.
3. **Round 3** — re-run `/code-review` once more; fix what it reports. This is the
   **last** review round — do not run a fourth.
4. **Opus 5 escalation** — if any finding is still open after round 3, it goes to Claude
   Opus 5 to resolve rather than straight to the deferred registry. Run this pass on
   Opus 5: inline if this session already runs Opus 5, otherwise via one
   `general-purpose` sub-agent with `model: "opus"`. Hand it the diff command, the open
   findings verbatim, and the standards/spec sources, and have it fix them. Then rerun
   the full test suite.
5. The escalation pass is **single and terminal** — it fixes, it does not trigger
   another review round. Only a finding Opus 5 judges genuinely out of scope for this
   issue gets recorded in `docs/DEFERRED_ISSUES.md` (with that reason), per the format
   in that file.

Commit your work on the worktree's branch (small, typed commits — `feat:` `fix:`
`chore:` `docs:` `refactor:` `test:` — per `docs/GIT_WORKFLOW.md`).

## 3. Open the PR

```bash
git push -u origin HEAD
gh pr create --base dev --title "<type>(WHI-NNN): <summary>" --body "..."
```

Title and body must include `WHI-NNN`. Set the Linear issue to **`In Review`**.

Verify the PR reads **MERGEABLE / CLEAN**: if `dev` advanced since you branched,
`git merge origin/dev` inside the worktree, resolve, rerun the affected tests, and push
before continuing.

## 4. Merge authorization — funds-path gate

A completed three-round review loop (plus the Opus 5 escalation pass, when round 3 left
findings open) is this repo's definition of "PR approved" per `CLAUDE.md` step 4, and
**authorizes self-merge** — except for gated changes, which stop at `In Review` and wait
for a human:

**Gated — stop at `In Review`, do not self-merge:**

- Anything touching `src/execution/` (the `Executor`/`SwapExecutor`/nonce/gas-profile
  path that submits on-chain transactions), `contracts/executor/ArbitrageExecutor.sol`,
  or private-key handling (`*_PRIVATE_KEY` env vars, signer construction).
- Anything touching a `mainnet` gas profile (`config/gas_profiles/*mainnet*.json`) or
  otherwise changing behavior that would run against real funds once this bot is live
  (as opposed to Mantle Sepolia / shadow-runtime paths).
- Any `release/*` → `main` promotion (`docs/GIT_WORKFLOW.md` — always human-merged).

For everything else, once the review loop above is complete, run the **post-merge
cleanup** from `CLAUDE.md`, in order, from the **primary clone**:

1. `gh pr merge <N> --squash --delete-branch`
2. `git worktree remove <worktree-path>` then `git worktree prune`
3. `git branch -D <branch>` (fails until step 2 completes)
4. `git fetch origin --prune` then `git merge --ff-only origin/dev`
5. Linear → `Done`

Work that skipped the review loop, or that falls under the funds-path gate above, must
stop at `In Review` and wait for a human to run the merge + cleanup themselves.
