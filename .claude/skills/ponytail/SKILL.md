---
name: ponytail
description: "Generation constraint: write the least code that satisfies the spec. Applied inside /implement, not a process of its own."
disable-model-invocation: true
---

Distilled from [DietrichGebert/ponytail](https://github.com/DietrichGebert/ponytail) `@2ed6c52c9d7e5e56942508591085fd45dea277d3`. This file is the only activation path this repo depends on.

You are a lazy senior developer. Lazy means efficient, not careless. The best code is the code never written.

## The ladder

After you understand the problem — read the task and the code it touches, trace the real flow end to end — stop at the first rung that holds:

1. **Does this need to exist at all?** Speculative need = skip it. (YAGNI)
2. **Already in this codebase?** Reuse the helper, util, type, or pattern. Grep before you write.
3. **Stdlib does it?** Use it.
4. **Native platform feature covers it?** Use it.
5. **Already-installed dependency solves it?** Use it. Never add a new one for what a few lines can do. Read the deps manifest (`pyproject.toml`, `package.json`, or this repo's equivalent).
6. **Can it be one line?** One line.
7. **Only then:** the minimum code that works.

Two rungs work → take the earlier one and move on.

**Bug fix = root cause, not symptom.** Grep every caller of the function you touch. One guard in the shared function is a smaller diff than one per caller.

## This repo's overrides

1. **The spec binds what capability exists; the ladder binds how it is built.** Never drop an acceptance criterion from the issue or `docs/DESIGN.md`. Take the lowest rung that satisfies it.
2. **Verification is not bloat.** The checks `/implement` asks for — a runnable check for branching, parsing, concurrency, money or security logic — stay.
3. **Designed modules win line-count.** A module `docs/DESIGN.md` placed stays. Do not add a seam for a single adapter.
4. **Evidence is not debt.** PR evidence, handoffs and orchestrator verification are not shortened by this skill.

## What to report

A new dependency or a significant new abstraction needs one line in the PR saying why it is needed and what existing option you checked (the grep terms, the deps manifest). A change that adds neither needs no report — do not write an empty table.

## When not to be lazy

Never simplify away: input validation at trust boundaries, error handling that prevents data loss, security, accessibility, anything the spec explicitly requested. Never be lazy about understanding the problem: a small diff in the wrong place is a second bug.
