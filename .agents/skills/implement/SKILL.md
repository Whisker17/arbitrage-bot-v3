---
name: implement
description: "Implement a piece of work based on a spec or set of tickets."
disable-model-invocation: true
---

Implement the work described by the user in the spec or tickets.

Use /tdd where possible, at pre-agreed seams.

Run typechecking regularly, single test files regularly, and the full test suite once at the end.

Once done, use /code-review to review the work (it runs its review sub-agents on Claude Opus 5 — no separate manual review pass is needed afterwards).

Then close the loop, bounded at **three review rounds** followed by an Opus 5 escalation pass:

1. **Round 1** — fix the findings from the first review (or consciously decide not to, with a reason).
2. **Round 2** — re-run /code-review to verify the round-1 fixes didn't miss the point or introduce new issues; fix what it reports.
3. **Round 3** — re-run /code-review once more; fix what it reports. This is the **last** review round — do not run a fourth.
4. **Opus 5 escalation** — if any finding is still open after round 3, it goes to Claude Opus 5 to resolve rather than straight to the deferred registry. Run this pass on Opus 5: inline if this session already runs Opus 5, otherwise via one `general-purpose` sub-agent with `model: "opus"` (currently resolves to Claude Opus 5). Hand it the diff command, the open findings verbatim, and the standards/spec sources, and have it fix them. Then rerun the full test suite and lint.
5. The escalation pass is **single and terminal** — it fixes, it does not trigger another review round. Only a finding Opus 5 judges genuinely out of scope for this issue gets recorded in `docs/DEFERRED_ISSUES.md` (with that reason) per CLAUDE.md.

Commit your work to the current branch.

A completed review loop means the work is ready to hand off for merge — take the PR up to `In Review` and stop:

1. Push and open the PR: `git push -u origin HEAD`, then `gh pr create --base dev` (title/body include `WHI-NNN`). Linear → `In Review`.
2. Verify the PR reads **MERGEABLE / CLEAN** (if `dev` advanced, `git merge origin/dev`, resolve, rerun affected tests, push) and that the full test suite and lint pass.
3. **Do not merge your own PR.** Per `docs/GIT_WORKFLOW.md`'s Agent / 自动化约束 #4, every PR stops at `In Review` and waits for the user's explicit go-ahead in that conversation — a completed review loop is what makes a PR *ready* to merge, not authorization to self-merge. Once the user explicitly says to proceed, run `gh pr merge <N> --squash --delete-branch` and the post-merge cleanup from CLAUDE.md (remove worktree, delete local branch, fast-forward local `dev`); Linear → `Done`.

Work that skipped the review loop must say so plainly when handing the PR off — the user's merge decision should account for that.
