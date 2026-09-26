# Issue tracker: Linear

Issues and PRDs for this repo live in **Linear**, project **"Mantle Arbitrage bots v2"**, team
`Whisker-Personal` (key `WHI`).

## How to reach Linear

Tracker state moves in lockstep with the PR (below), so this access path is **mandatory,
not a convenience** — a runtime that cannot reach the tracker cannot complete the git
workflow. Take the first rung of this ladder that your runtime actually offers:

1. **Linear MCP tools.** In this template's reference setup they are exposed via the
   `slim-tools` gateway rather than as top-level tools:
   - Discover the tool you need:
     `discover_tools({ query: "linear <capability>", detail: "typescript" })`
     (e.g. `"linear create issue"`, `"linear list issues"`, `"linear comment"`).
   - Call it from `execute_code` using the returned `codeApi.path`. Aggregate/filter in
     the sandbox; return only the final shape.

   If your runtime has the Linear MCP server mounted directly, the tool names below apply
   without the gateway indirection.
2. **Linear GraphQL API** over plain HTTP, with `LINEAR_API_KEY` from `.env`. Every
   operation named below exists as a GraphQL mutation/query (`issueCreate`,
   `issueUpdate`, `commentCreate`, `issues`, `workflowStates`). Use this when your runtime
   has shell/network access but no MCP.
3. **Neither available:** stop and report the step as blocked. Do **not** switch to a
   local-markdown tracker or `gh issue` — the tracker is shared state, and a divergent
   shadow copy is worse than an honest block. A project that genuinely uses another
   tracker replaces **this file** with a binding for it, so every skill agrees on where
   issues live. That binding must define the same operations, including
   [§ Release ↔ version binding](#release--version-binding) (or declare that it has no
   release entity — git routing reads that section) and
   [§ Release orchestration document](#release-orchestration-document).

Verify every operation you rely on — Release lookup, issue ↔ Release binding, native
relations, documents — against the target workspace's actual tools or API before depending
on it. Do not guess from a field name. A write you could not confirm is reported as not
done.

The rest of this file names Linear MCP tools (namespace `linear`); under rung 2, read each
as its GraphQL equivalent. Tool names and argument shapes differ between servers and
versions, so **discover the schema first** and use the calls below only where it matches.

### Reference calls (Linear MCP)

These were checked against the reference Linear MCP server. "Exercised" means a real call
succeeded; "schema-verified" means the argument or field was confirmed in the tool schema
but not called.

| Need | Call | Status |
| --- | --- | --- |
| Page through a project's or Release's issues | `list_issues({ project, release, limit, cursor })` — repeat with the returned `cursor` while `hasNextPage` | exercised |
| One issue with its Release and native relations | `get_issue({ id, includeRelations: true, includeReleases: true })` → `releases[]`, `relations.blocks` / `relations.blockedBy` | exercised |
| Bind an issue to a Release | `save_issue({ id, setReleases: [<release>] })` (also `addReleases` / `removeReleases`), then re-read with `get_issue` | exercised |
| Wire native dependencies | `save_issue({ id, blocks: [...], blockedBy: [...] })` | schema-verified, not exercised |
| Find a project document | `list_documents({ projectId, query, limit, cursor, fields })` — the filter is `projectId`, not `project` | exercised |
| Create / update a document | `save_document({ title, project, content })` creates; `save_document({ id, content })` updates | create exercised |

**`list_issues` results do not include Release or relations.** Never infer those from a
list page: call `get_issue` for each issue whose Release or dependencies matter. After a
write, re-read to confirm it.

Other core tools:

- **Create / update an issue**: `linear.save_issue({...})`. When creating, `title` and
  `team` are required; also set `project` to `"Mantle Arbitrage bots v2"` so it's scoped
  correctly. Omit `id` on create; pass `id` to update. Use `assignee` (a user id, name,
  email, or `"me"`) — not `assigneeId`. Set labels via the `labels` field (see
  `triage-labels.md` for the canonical strings), the Release via `setReleases`, and
  dependencies via `blocks` / `blockedBy` (table above).
- **Read one issue**: `linear.get_issue({ id, includeRelations: true, includeReleases:
  true })`. **List issues**: `linear.list_issues({...})` with the filters its schema
  offers (project, release, assignee, state, label), paging with `cursor`.
- **Comment**: `linear.save_comment({ issueId, body })` to start a thread;
  `linear.save_comment({ parentId, body })` to reply. Read with
  `linear.list_comments({ issueId })`.
- **Labels**: `linear.list_issue_labels({...})` to find existing labels;
  `linear.create_issue_label({...})` to create a missing one.
- **Workflow states**: `linear.list_issue_statuses({ team })` — Linear tracks progress
  as workflow states in addition to labels. Move an issue by setting its `state` via
  `linear.save_issue`.
- **Close**: set the issue's `state` to a completed/canceled workflow state via
  `linear.save_issue`, optionally after a `linear.save_comment` explaining why.

Prefer Linear's native **workflow states** for lifecycle (open → done) and **labels**
for the triage roles in `triage-labels.md`.

## Issue lifecycle ↔ Git (mandatory)

Implementation always uses a **git worktree** off the **resolved base** (see
`docs/GIT_WORKFLOW.md` — version-scoped work targets `release/v{version}`,
governance targets `dev`, hotfix targets `main`). Agents must keep Linear
state in lockstep with the PR:

| When | Linear `state` |
| --- | --- |
| Claimed, implementation started | `In Progress` |
| PR opened against the resolved base | **`In Review`** |
| PR merged into the resolved base and the required cleanup (and fan-out) done | **`Done`** |
| Waiting for a human, or verification failed | stays **`In Review`**, reason in a comment |
| Abandoned | `Canceled` |

**Who moves it.** Under `/orchestrate` the orchestrator owns every transition; implementers
return facts and PR links. In standalone `/implement`, the implementing agent owns them.
`Done` means merged, not released.

Do not mark `Done` when the PR is only opened, or because an agent reported it finished.
Do not leave an open PR in `In Progress`. If a PR opened before the orchestrator heard
about it, sync the state as soon as it does. On resume, read the real PR state first and
repair drift — never re-create or re-merge a PR. Triage labels (`ready-for-agent`, etc.)
stay orthogonal to these states.

## Release ↔ version binding

`docs/GIT_WORKFLOW.md` § Version determination resolves a version-scoped issue's
base branch from two signals: the `[X.Y.Z]` title prefix (primary) cross-checked
against **this tracker's release entity**. That entity is tracker-specific, so it
is defined here.

**For Linear, the release entity is the issue's `Release` field.** This repo's Releases
live in the release pipeline **`arbitrage-bot-v3`** (e.g. `0.2.2`; list them with
`list_releases`). Sharp edges,
all of which have already caused a real mis-resolution in the source project
(`mantle-stocks-arbitrage-bots`, WHI-1097):

- **`Release` is not `projectMilestone`.** Both render as a bare `X.Y.Z` in the
  UI and in API payloads, and reading the milestone as the Release let a title
  prefix of `[0.2.0]` sit against a milestone of `0.3.0` undetected for days.
  Milestone is *capability stage*; Release is *when it ships* and what routes
  git (`docs/GIT_WORKFLOW.md` § Version axis).
- **Linear does not enforce non-empty fields.** `issue-template.md`'s metadata
  table asks for a Release, but nothing rejects an issue without one — several
  shipped with the field empty. The enforcement is the agent's **refusal**, not
  the tracker.
- **Read it through the same access ladder as everything else** (above): via MCP,
  `get_issue({ id, includeReleases: true })` and read `releases[]` (a list page does not
  carry it); under rung 2, the GraphQL `issue` query — select the release field
  explicitly and confirm you did not select the milestone by accident. If the tracker is unreachable, the
  cross-check has not been performed: say so and stop, rather than proceeding on
  the title prefix alone as though both signals had agreed.

**Swapping trackers:** a replacement binding for another tracker must define this
section too — naming its own release
entity and how to read it — or state explicitly that it has **no** release
entity, which drops the cross-check per `docs/GIT_WORKFLOW.md` § Version
determination and leaves the title prefix standing alone. A missing or ambiguous
prefix still refuses.

## Pull requests as a triage surface

**PRs as a request surface: no.** Code review happens on GitHub; the Linear queue is not
fed from pull requests.

## When a skill says "publish to the issue tracker"

Create a Linear issue with `linear.save_issue` (`title` + `team` required, `project` set
to `"Mantle Arbitrage bots v2"`). Follow the canonical structure in
`docs/agents/issue-template.md` — title convention, body sections, and acceptance
criteria. All issue content is written in English.

## Publishing ticket sets (e.g. from `/to-tickets`)

When a skill produces a **set** of tickets with blocking edges, publish them to Linear
like this:

1. **Order**: create issues in dependency order — blockers first — so each later issue
   can reference real identifiers.
2. **Blocking edges**: wire them as Linear's native **`blocked-by` / `blocks`
   relations** (`save_issue({ id, blocks, blockedBy })`, or the dedicated relation tool
   if your server's schema has one), then confirm with `get_issue({ id,
   includeRelations: true })`. Also mirror
   each edge as a human-readable `## Blocked By` line in the body per
   `issue-template.md`; the native relation is the source of truth, the body line is the
   mirror.
3. **Scoping**: every issue gets `project: "Mantle Arbitrage bots v2"` and, if the
   set belongs to a version, the matching **Release** (`setReleases`, confirmed with
   `get_issue({ id, includeReleases: true })`; title carries the `[X.Y.Z]` prefix per
   `issue-template.md`). Milestone is orthogonal — attach it when the
   set is a capability stage, but do not put it in the title.
4. **State + labels**: state `Todo`, triage label `ready-for-agent` (unless the user
   says otherwise) — the tickets are agent-grabbable by construction. **Exception:** a
   ticket whose expected scope touches the funds path gets `ready-for-human` per
   `triage-labels.md` § High-risk-path extra caution.
5. **Body**: use the copy-paste skeleton in `issue-template.md`, including a filled
   `## Execution` section (one complexity value, reason, expected scope).

## When a skill says "fetch the relevant ticket"

Read it with `linear.get_issue({ id, includeRelations: true, includeReleases: true })`,
then pull discussion with `linear.list_comments({ issueId })`.

## Reading a release

`/orchestrate` works from an explicit project and Release — never a similar title or a
milestone. Page through **every** page of `list_issues({ project, release, limit,
cursor })` until `hasNextPage` is false. Then, for each issue, `get_issue({ id,
includeRelations: true, includeReleases: true })` for its state, `releases[]`,
`relations.blocks` / `relations.blockedBy`, the `## Execution` section and acceptance
criteria, plus `list_comments` for amendments. A list page alone is not enough. Native relations are the dependency source of truth; the body lines
mirror them — fix whichever is wrong before scheduling.

## Release orchestration document

Release-level state lives in **one** Linear project document named
`Release X.Y.Z — orchestration`, reused for the whole release (never one per round) and
linked from the issues it concerns. It records:

- workflow contract version (`v0.2`), and which issues, if any, started under an older one;
- the planned issue set, the fixed review baseline **B** and the integration branch;
- each review round: candidate SHA **H**, reviewer role/model/effort, outcome;
- every finding id → the fix issue that owns it (or its disposition), including
  governance fixes tracked as external blockers (no Release, routed to `dev`);
- current blocker, if any.

Look it up first with `list_documents({ projectId, query: "Release X.Y.Z — orchestration" })`.
Update it with `save_document({ id, content })` (or the server's patch form). Only if none
exists, create it with `save_document({ title, project, content })`, then link it from the
issues. GraphQL equivalents: `documents` / `documentCreate` / `documentUpdate`. If documents cannot be written, say so and stop the step that needed it —
do not keep the state only in chat or a temp file.
