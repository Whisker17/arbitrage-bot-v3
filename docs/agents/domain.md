# Domain Docs

How the engineering skills should consume this repo's domain documentation when exploring
the codebase.

This repo is **single-context**. Its spec of record is **`docs/DESIGN.md`** — today an
index with the template's stable section numbers, pointing into `specs/` and
`tech-docs/` where the design actually lives (most of it written in Chinese).

## Before exploring, read these

- **`docs/DESIGN.md`** — background, scope, requirements, architecture, milestones,
  rejected alternatives (§7), and known risks (§8), each section pointing at its source
  document. Follow the pointers the task needs; do not re-litigate a decision recorded
  there without flagging it explicitly (see below).
- **`specs/`** — the audited recovery and go-live plan (`specs/README.md` is the index);
  **`tech-docs/`** — module and protocol deep dives.
- **`docs/DEFERRED_ISSUES.md`** — accepted debt and intentional designs ("Design notes"),
  so you do not rediscover or "fix" them.
- **`docs/references/`** — prior research or external material the project's parameters
  and decisions are inherited from. Treat sourced numbers as validated inputs, not
  something to re-derive.
- **`docs/adr/`** (created lazily; may not exist yet) — narrower decisions made *after*
  the initial version ships that don't belong in the PRD itself (e.g. a specific library
  choice, a schema migration). Read any ADR that touches the area you're about to work
  in.

If `docs/adr/` is empty or missing, **proceed silently**. Don't flag its absence; don't
suggest creating it upfront — it gets created the first time a post-v1 decision actually
needs recording.

## File structure

```
/
├── docs/
│   ├── DESIGN.md          # spec of record (index into specs/ and tech-docs/)
│   ├── references/        # prior research the project inherits from
│   ├── adr/               # narrower post-v1 decisions (created lazily)
│   │   ├── 0001-....md
│   │   └── 0002-....md
│   └── agents/            # this directory — agent operating conventions
├── specs/                 # audited recovery / go-live plan
├── tech-docs/             # module and protocol deep dives
└── src/                   # modules per AGENTS.md § Architecture
```

## Use the spec's vocabulary

`docs/DESIGN.md` fixes specific domain terms. When your output names a domain concept
(in an issue title, a refactor proposal, a test name), use the term as defined there.
Don't drift to synonyms.

## Flag design-doc conflicts

If your output contradicts a decision recorded in `docs/DESIGN.md` §7 (rejected
alternatives) or would introduce a parameter not backed by §2 / `docs/references/`,
surface it explicitly rather than silently overriding:

> _Contradicts §7 ("X was rejected because Y") — but worth reopening because…_
