---
name: to-spec
description: Turn the current conversation into a spec — no interview, just synthesis of what has already been discussed. Fills the numbered docs/DESIGN.md skeleton by default; publishes only as the user directs.
disable-model-invocation: true
---

This skill turns the current conversation and your understanding of the codebase into a spec (you may know it as a PRD). Do NOT interview the user — synthesize what you already know. Use `/grill-me` first if the goal, constraints or acceptance are still unclear.

## Project spec (default): `docs/DESIGN.md`

`docs/DESIGN.md` is the spec of record, and its numbered sections are stable references. Issues cite `§2.x`, `[Component]` comes from `§4.2`, milestones from `§6`, and agents check `§7` and `§8` (`AGENTS.md`, `docs/agents/issue-template.md`, `docs/agents/domain.md`). So:

- Read the existing file and fill its sections **in place**. Never replace it with another layout, and never renumber its sections.
- Replace each _(fill in)_ guidance with content, following the guidance.
- Delete a section only where the file itself says it may be deleted. Keep §7 and §8 even when short.
- When updating an existing spec, change the affected sections only. Keep `§` numbers stable: add new subsections at the end of a section, and never reuse a deleted number.

Where things go:

- Goal, scope, non-goals and overall success criteria → §1.
- Each requirement goes in §2 as a numbered subsection, with an objectively checkable acceptance condition and how it will be verified. Prefer external behaviour and existing test seams, and name the smallest check that would catch a real failure. Any tunable parameter cites its source.
- Architecture and interfaces → §4. Keep the module layout in §4.2 in step with `AGENTS.md` § Architecture.
- Rejected options → §7. Risks and open questions → §8.

Avoid file paths and code snippets, which go stale. The exception is a prototype snippet that pins a decision (a state machine, a schema, a type shape): trim it to the decision-rich part.

## Template or tooling spec

A spec about the template or tooling itself goes in its own document under `docs/references/`, never over `docs/DESIGN.md`. Its headings are free. It must still state the problem, the requirements with acceptance, the decisions, what is out of scope, and the risks.

## Publishing

Publish to the tracker (`docs/agents/issue-tracker.md`) only as the user instructs. Do not create a ready-for-agent issue by default: tickets come from `/to-tickets`.
