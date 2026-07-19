# Issue tracker: Linear

Issues and PRDs for this repo live in **Linear**. Skills reach Linear through the
**Linear MCP tools**, which are exposed via the `slim-tools` gateway rather than as
top-level tools.

## How to call Linear

1. Discover the tool you need:
   `discover_tools({ query: "linear <capability>", detail: "typescript" })`
   (e.g. `"linear create issue"`, `"linear list issues"`, `"linear comment"`).
2. Call it from `execute_code` using the returned `codeApi.path`. Aggregate/filter in
   the sandbox; return only the final shape.

Core tools (namespace `linear`):

- **Create / update an issue**: `linear.save_issue({...})`. When creating, `title` and
  `team` are required; omit `id`. To update, pass `id`. Use `assignee` (a user id, name,
  email, or `"me"`) — not `assigneeId`. Set labels via the `labels` field (see
  `triage-labels.md` for the canonical strings).
- **Read / list issues**: `linear.list_issues({...})`. Filter by `assignee`
  (`"me"` / `"null"`), team, state, or label. For a single issue, list with the id/filter
  and read the returned record.
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

Implementation always uses a **git worktree** off latest `dev` (see `docs/GIT_WORKFLOW.md`).
Agents must keep Linear state in lockstep with the PR:

| When | Linear `state` |
| --- | --- |
| Claimed / coding in worktree | `In Progress` |
| PR opened against `dev` (awaiting review) | **`In Review`** |
| PR squash-merged into `dev` | **`Done`** |
| Abandoned | `Canceled` |

Do not mark `Done` when the PR is only opened. Do not leave an open PR in `In Progress`.
Triage labels (`ready-for-agent`, etc.) stay orthogonal to these states.

## Pull requests as a triage surface

**PRs as a request surface: no.** Code review happens on GitHub; the Linear queue is not
fed from pull requests. `/triage` processes Linear issues only.

## When a skill says "publish to the issue tracker"

Create a Linear issue with `linear.save_issue` (`title` + `team` required). Follow the
canonical structure in `docs/agents/issue-template.md` — title convention, body sections,
and acceptance criteria. All issue content is written in English.

## When a skill says "fetch the relevant ticket"

Read it with `linear.list_issues` (filter to the id/identifier), then pull discussion
with `linear.list_comments({ issueId })`.

## Wayfinding operations

Used by `/wayfinder`. The **map** is a parent issue (or a Linear document) whose
**children** are the tickets.

- **Map**: a single issue labelled `wayfinder:map`, holding the Notes / Decisions-so-far
  / Fog body. Create with `linear.save_issue` and apply the label.
- **Child ticket**: an issue linked to the map as a **sub-issue** (set the map as the
  parent on `linear.save_issue`). Labels: `wayfinder:<type>`
  (`research` / `prototype` / `grilling` / `task`). Once claimed, assign it to the
  driving dev (`assignee`).
- **Blocking**: use Linear's native **issue relations** — a `blocks` / `blocked-by`
  relation between the child and its blocker. A ticket is unblocked when every blocker
  reaches a completed workflow state. Where relations can't be set, fall back to a
  `Blocked by: <identifier>` line at the top of the child body.
- **Frontier query**: list the map's open children (`linear.list_issues` filtered to the
  map's sub-issues, non-completed states), drop any with an unresolved blocker or an
  assignee; first in map order wins.
- **Claim**: set `assignee: "me"` via `linear.save_issue` — the session's first write.
- **Resolve**: `linear.save_comment` with the answer, move the child to a completed
  state, then append a context pointer to the map's Decisions-so-far.
