---
name: code-review
description: Independent adversarial review, in a fresh REVIEWER context, of a fixed PR range or a release snapshot (baseline B to candidate H). Reports verifiable findings, marked blocking or suggestion, and a verdict tied to the reviewed SHA.
---

One independent reviewer, in a fresh context, looks for real defects in a fixed range. It
does not edit the code under review.

## 1. Pin the range

- **PR mode:** fixed point = the PR base (or the ref the user names); head = the PR's head
  commit.
- **Release mode:** baseline **B** and candidate **H**, from the
  `Release X.Y.Z — orchestration` document (`/orchestrate` § Release review).

Commit everything first. Resolve both ends (`git rev-parse`) and record the exact SHAs.
Use the three-dot form `git diff <fixed>...<head>` (against the merge-base; for B..H,
where B is an ancestor, it equals the plain range). Two-dot against a base that has moved
shows foreign commits in reverse. Check that the diff is **non-empty and contains the work
being claimed**. A bad ref, an empty diff, or a diff missing the claimed work stops here —
it is never a pass.

## 2. Gather the inputs

- The diff (inline it in the prompt; do not rely on the reviewer running git) and the
  commit list. The reviewer may read the full tree at the head to check interactions.
- The spec and **every** acceptance criterion in scope: the PR's issue, or every issue in
  the Release. Fetch them via `docs/agents/issue-tracker.md`, or ask the user where the
  spec is.
- The verification artifacts (commands, results, logs) and, in release mode, all earlier
  findings and their fixes.
- The repo's documented standards (`CONTRIBUTING.md`, `CODING_STANDARDS.md`, …), if any.

## 3. Dispatch

`scripts/agent-dispatch.sh REVIEWER <prompt-file> --effort high`, or a native sub-agent that
honours the same model and effort (`docs/agents/runtime.md`). It must never run in the
implementing context. Exit `3`, a failed call, or empty/transport-error output means **no
review happened**: record it and follow `docs/agents/runtime.md` § Reviewer unavailable.

Brief the reviewer to cover, in one pass:

- whether the change meets the spec and each acceptance criterion;
- error handling, edge cases, state recovery, security and data integrity;
- consistency of interfaces and behaviour across issues (release mode especially);
- whether the verification really proves the requirement — no vacuous gate, empty diff,
  or test that would still pass with the change reverted;
- simplicity per `/ponytail`: existing capability re-implemented, or an unneeded
  dependency or abstraction.

In later release rounds the reviewer still owns the whole candidate, with extra attention
on the fixes and what they could have broken.

## 4. Findings

Each finding has:

- a stable id: `R<round>-F<n>` in release mode (`R1-F1`), `PR-F<n>` in PR mode;
- a severity:
  - **blocking** — a real defect, a missing acceptance criterion, missing critical
    verification, a security or data problem, or a clear breach of an explicit simplicity
    constraint;
  - **suggestion** — style preference, optional cleanup or a future enhancement. Never
    blocks, never becomes a task by itself;
  - **unverified** — a finding with no reproduction or thin evidence. Not a proven defect,
    but one that touches a key acceptance criterion blocks until resolved;
- the location (file/line or behaviour), the trigger, the evidence or reproduction, the
  requirement it violates, and the smallest fix.

End with the **verdict** and the SHA it covers. **Pass** = no unresolved blocking finding
and acceptance valid on that SHA; suggestions may remain. A later commit needs its own
check.

The caller — the orchestrator, or the implementer in standalone mode — removes duplicates
and checks scope. A finding is not closed just because the implementer disagrees; a real
dispute goes back to the reviewer, then to a human.
