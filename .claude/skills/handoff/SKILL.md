---
name: handoff
description: Compact the current work into a short handoff for another agent or session, pointing at durable evidence instead of restating it.
argument-hint: "What will the next session be used for?"
disable-model-invocation: true
---

Write a handoff so a fresh agent can continue the work. Save it in the OS temporary directory (or the output path your caller binds), not in the workspace.

A temporary handoff is a courier, never the only record. Before writing it, make sure the facts a resume depends on already live somewhere durable: the PR body or issue comments for issue-level results, and the `Release X.Y.Z — orchestration` document (`docs/agents/issue-tracker.md`) for release-level state (baseline B, round, reviewed SHAs, finding → fix mapping).

Include only:

- issue, spec, PR, branch and worktree links/paths, and the resolved base;
- the current commit SHA and what it implements;
- acceptance commands, their actual results, and artifact locations;
- open questions, failed or not-run checks, and the next step;
- effects on dependent tasks, with evidence for any corrected conclusion;
- the role, model and effort actually used;
- suggested skills for the next session.

Reference specs, plans, issues, commits and diffs by path or URL; do not copy them. Redact secrets and personal data.

If the user passed arguments, treat them as what the next session will focus on and tailor the handoff to that.
