---
name: to-tickets
description: Break a plan, spec, or the current conversation into tracer-bullet tickets using the repo issue template — complexity, expected scope and native blocking relations included — and publish them to the configured tracker.
disable-model-invocation: true
---

# To Tickets

Break a plan, spec, or conversation into **tickets**: tracer-bullet vertical slices, each declaring the tickets that **block** it.

The ticket shape is **`docs/agents/issue-template.md`**, which also sets the title convention, metadata and `## Execution` section. Publishing mechanics are in **`docs/agents/issue-tracker.md`**. Read both. This skill has no ticket template of its own.

## Process

### 1. Gather context

Work from what is already in the conversation. If the user passes a reference (a spec path, an issue id or URL), fetch it and read its full body and comments.

### 2. Explore the codebase (optional)

If you have not explored it yet, do so now. Use the spec's domain vocabulary and respect ADRs. Look for prefactoring that would make the implementation easier: "make the change easy, then make the easy change."

### 3. Draft vertical slices

<vertical-slice-rules>

- Each slice cuts a narrow but COMPLETE path through every layer it needs (schema, API, UI, tests). Vertical, not a horizontal slice of one layer.
- A completed slice can be demonstrated or verified on its own.
- Each slice fits in a single fresh context window.
- Prefactoring comes first.

</vertical-slice-rules>

For each ticket, fill in:

- **Blocking edges:** the tickets that must finish before it can start. A ticket with none can start immediately.
- **Execution:** one complexity value (`medium` | `high`, per the template's definitions, independent of size and priority), a one-sentence reason, and the expected scope (files, modules and shared contracts). Two tickets whose scopes share a module, schema, public interface or generated file will be run serially even without a blocking edge. Say so, or re-cut the boundary.
- **Acceptance criteria:** objectively checkable.

**Wide refactors are the exception to vertical slicing.** A wide refactor is one mechanical change (rename a column, retype a shared symbol) whose blast radius spans the codebase, so no vertical slice can land green. Sequence it as **expand–contract**:

1. **Expand:** add the new form beside the old one.
2. **Migrate:** move call sites in batches sized by blast radius, one ticket per batch, each blocked by the expand.
3. **Contract:** delete the old form in a ticket blocked by every migrate batch.

If even the batches cannot stay green on their own, let them share an integration branch and have all of them block a final integrate-and-verify ticket.

### 4. Quiz the user

Present the breakdown as a numbered list. For each ticket, show its title, what blocks it, what it delivers, and its complexity and expected scope. Ask whether:

- the granularity is right;
- the blocking edges are real;
- the complexity calls are right;
- any tickets should be merged or split.

Iterate until the user approves.

### 5. Publish

Follow `docs/agents/issue-tracker.md` § Publishing ticket sets:

- Publish in dependency order, blockers first.
- Wire **native** `blocked-by` relations and mirror them in the body.
- Use the template skeleton, including `## Execution`.
- Version-scoped tickets carry the `[X.Y.Z]` title prefix **and** the matching Release. The two must agree. Milestone plays no part in routing.
- Governance tickets carry neither.
- Reach the tracker via MCP first, then the API fallback. If no tracker is reachable, report that publishing is blocked. Do not write local files as a stand-in.

Do NOT close or modify any parent issue.
