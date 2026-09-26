> **On-demand reference.** Read this file when the work goes near a trap listed here.
> `/orchestrate` passes only the entries relevant to an issue into that implementer's
> launch prompt, not the whole list. Rules that every agent needs are in the
> authoritative docs: entries 1–2 → `docs/GIT_WORKFLOW.md` § 3 (PR conventions), 3 and 5 →
> `/code-review` § Pin the range / § Dispatch, 4 → `docs/agents/runtime.md` § Preflight.
> Appending here is a version-scoped docs change. Downstream projects seed this file
> with *their* measured traps; do not copy another repo's list in wholesale.

# Trap registry — append-only, repo-specific

Each entry is cited against the repo so it can be checked. An entry that stops being true
moves to `docs/DEFERRED_ISSUES.md`'s resolved section. It is not silently deleted.

**Trims declare themselves.** An entry leaves this file in one of two ways: it moves to
that resolved section, or it is merged into another entry covering the same failure.
Either way, the PR that removes it must say which entry left and why.

The numbered entries below are **process-layer** traps measured while the orchestration
workflow was dogfooded. Stack-specific traps belong in the downstream project's copy.

1. **`gh pr create` (and every later `gh` call) needs `--repo <owner/repo>` whenever
   `gh` would resolve the wrong GitHub repo.** A fork, a `no_push` `upstream`, or
   `gh repo view` reporting a different `nameWithOwner` than `origin` are the usual
   causes. Verify with `gh repo view --json nameWithOwner` before the first `gh pr
   create` of an issue.
2. **Nested worktrees under the primary clone need `--head <owner/repo>:<feature-branch>`
   as well.** `gh pr create` without `--head` infers the primary clone's checked-out
   branch (often `dev` or `main`), not the worktree's. After create,
   `gh pr view --repo <owner/repo> <N> --json headRefName` must equal the worktree
   branch. If it is the trunk, close the PR and recreate — do not push more commits
   onto a PR whose head is the trunk.
3. **Commit before dispatching a reviewer, then hand it a three-dot diff, and verify it
   is non-empty before dispatching.** `/code-review`'s canonical command is
   `git diff <fixed-point>...HEAD` (three-dot, against the merge-base). Two-dot
   (`git diff <base>`) looks like the fix for an empty three-dot on an uncommitted
   branch, but once `<base>` advances it shows the reviewer those foreign commits,
   reversed. Commit first; use three-dot; refuse to dispatch on an empty range.
4. **`--probe` is not a real call.** `scripts/agent-dispatch.sh --probe` only checks that the
   binary resolves. A role whose binary is on `PATH` but whose auth/quota/flags fail
   still reports `ok`. The gate is a real one-line dispatch that must print
   `DISPATCH-OK` and exit 0 (`docs/agents/runtime.md` § Preflight). If it fails, the
   review did not happen: stay at `In Review`, never self-review.
5. **Empty reviewer stdout is a vacuous gate, not a clean pass.** Exit 0 with no
   findings-shaped content is indistinguishable from "the dispatch never ran." Inline
   the three-dot diff (and any file the spec needs) in the reviewer prompt rather than
   depending on the reviewer to run git; if stdout is empty or a transport error,
   retry once, then stop and report.
6. **`pgrep -fl` dumps this machine's entire shell-snapshot environment** instead of the
   one process you meant to find. Use `pgrep -f <pat> | head -1` to get the pid, then
   `ps -o pid,etime,command -p <pid>`.
7. **Generate committed measurement artifacts once, after the code under measurement is
   final, from a clean committed sha.** Reviewers read `git diff <fixed-point>...HEAD`,
   not generated reports — regenerating a report per fix turns every prose finding into a
   full re-measurement. If the tree is dirty at generation time, stamp provenance accordingly
   rather than silently attributing a dirty run to a later commit.
