# Git Workflow

This repo uses the **main + dev + version-integration + issue-worktree** model.

## The one-sentence rule

**Every issue's implementation = a dedicated git worktree branched off the
resolved base + a short-lived branch → PR into that same base → tracker state
moves in lockstep with the PR.**

The resolved base is **not** always `dev`. Version-scoped work targets
`release/v{version}`; `dev` is the trunk for **repo-wide governance** and the
place validated versions merge back to. **Never default to `dev`.** If the
issue does not fall into exactly one row of the [resolution table](#resolving-the-base-branch),
stop and surface it.

Never develop an issue directly in the primary clone's working tree (it dirties the
`dev` workspace and blocks parallel work on multiple issues). The primary clone is only
for: pulling `dev`/`main`/live `release/v*`, creating worktrees, post-merge
verification, and fan-out merges.

## Branch roles

| Branch | Role | Who writes |
|--------|------|------------|
| `main` | **Equals production.** Always equals the last deployed tag. Only accepts merges from `release/*` and `hotfix/*`. | PR only |
| `dev` | **Validated, shippable trunk.** Receives hotfix backmerges, repo-wide governance, and a finished version-integration branch. Not the integration point for unreleased version work — *except* before the first production tag exists, when it is also the integration branch for version-scoped work (see the [bootstrap clause](#before-the-first-production-tag-bootstrap)). | PR from governance / merge of a finished `release/v*` |
| `feat/*` `fix/*` `chore/*` | Single-issue implementation branches (inside worktrees) | Short-lived |
| `hotfix/*` | Emergency production fixes (branched from `main`) | Sync back to `dev` after merging to `main`, then fan out |
| `release/vX.Y.Z` | **Two lifecycles, same prefix** — see [below](#two-release-lifecycles) | Depends on lifecycle |

`main` ≡ production is what makes the two promotion lanes distinguishable: a release
ships everything on `dev`, a hotfix ships *only* your fix on top of what is already
live. Treat `origin/main` as the definition of "the code currently running".

## Two `release/` lifecycles

Same prefix, different job. The rule **never merge `release/*` → `main` without
human approval** applies to both.

| | Temporary cut | Long-lived version integration |
|---|---|---|
| Example | `release/v0.1.4` | `release/v0.2.0` |
| Cut from | `dev`. A multi-commit hotfix *batch* may also cut from `main`. That is not the `hotfix/*` lane — a single `hotfix/*` PR still goes straight to `main` per [§ Hotfix](#hotfix). | `dev` |
| Receives feature PRs? | **No** | **Yes** — this is the version's integration branch. It may hold work that has not passed release review yet, so it is **not releasable** until it does |
| Lifetime | Until the `main` PR merges, then deleted | The whole version's development |
| When done | PR → `main` (human gate), delete | Once the **current** SHA passed full acceptance and the release review (`/orchestrate` § Release review), **merge into `dev`** (merge commit, **human gate** — `dev` is the shippable trunk). If that merge needs a new conflict resolution or code change, verify the merge result and review the changed part; the earlier pass does not cover it. Then promote `dev` → `main` via a **fresh** temporary cut. If the integration branch still occupies `release/vX.Y.Z`, delete it first (its history is on `dev`). Do **not** PR the integration branch to `main` directly — that leaves `dev` vestigial. |

Cutting a version-integration branch is a **deliberate act** (it comes with the
merge-back decision above). An agent must not create `release/v*` as a side
effect of picking up a ticket. First push of a newly cut `release/v*` (no
PR yet) uses `ALLOW_DIRECT_PUSH=1` — the hook protects the name.

## Resolving the base branch

Derive the base from this table, then **assert it immediately**. Do not store a
base branch on the issue — derived values drift.

**Every issue falls into exactly one category:**

| Category | How it is recognised | Worktree base | PR base |
| --- | --- | --- | --- |
| Hotfix | `hotfix` label | `origin/main` | `main` |
| Repo-wide governance | touches **only** the carve-out file list | `origin/dev` | `dev` |
| Version-scoped work | everything else; version from the title prefix, cross-checked against the tracker's release binding | `origin/release/v{version}` | `release/v{version}` |

**If an issue does not fall cleanly into exactly one category, refuse to start
and surface it.** Do not guess. Do not fall back to `dev`.

### Before the first production tag (bootstrap)

> **Before the first production tag exists** (no deploy has ever happened, so
> there is no hotfix lane and nothing for `dev` to stay shippable *against*), the
> version-scoped row resolves to `dev` and no `release/v*` integration branch
> exists yet. This is a resolved value from this table, not a default. The first
> long-lived integration branch is cut — deliberately, by the owner — when work on
> the next version begins while a deployed version is live. From that point the
> table applies in full and falling back to `dev` is forbidden.

**This repo is in bootstrap state** (owner decision, WHI-1497). `main` has never
received a release promotion: the existing tags (`v0.2.0`, `v0.2.1`,
`v0.2.2-rc1`, `dryrun-0.1.0`) all point at `dev` commits, and the mainnet
deployment is a signerless shadow run, not a funded production send path. So
version-scoped issues resolve to `dev` today. The bootstrap state ends with the
first `release/v*` → `main` merge and its tag; from then on `main` ≡ production
and the table applies in full. Update this paragraph (governance PR) in the same
session that ends it.

### Repo-wide governance carve-out

These files govern **every** branch. Pinning them to one release would mean
hotfix branches run under stale rules:

- `AGENTS.md` (the single agent-instruction entry) and `CLAUDE.md` (a one-line
  `@AGENTS.md` import for Claude Code clients older than v2.1.281)
- `docs/GIT_WORKFLOW.md`
- `docs/agents/` (issue template, tracker binding, triage labels, runtime)
- `.githooks/`
- CI config (`.github/workflows/`)
- `.github/pull_request_template.md`
- `config/agent-roles.conf`
- `scripts/agent-dispatch.sh`
- `.claude/skills/` (and `.agents/skills`, a symlink to it)
- `skills-lock.json`

An issue that mixes this list with version-scoped code **must be split**: the
governance part goes to `dev` and [fans out](#fan-out-dev-into-live-version-branches);
the feature part goes to its release branch.

### Version determination (version-scoped row only)

Two independent signals, because either one alone has already failed in
practice (empty Release fields; two tracker fields that both render `0.2.0`
confused for each other; a title prefix disagreeing with the milestone,
undetected for days — `mantle-stocks-arbitrage-bots`, WHI-1097). Tracker
reachability is a fallback ladder — do not hard-bind a git operation to MCP.

1. **Primary: the issue-title prefix `[X.Y.Z]`** (e.g. `[0.2.0] [Scheduler] …`).
   Tracker-independent, always visible, no API call.
2. **Cross-check: the tracker's release/version binding**, as defined in
   `docs/agents/issue-tracker.md` § Release ↔ version binding. If the active
   tracker binding defines no release entity, the cross-check is vacuous and the
   title prefix stands alone — a missing or ambiguous prefix still refuses.

If the two signals disagree, or the cross-check exists and either signal is
missing, **refuse to start and surface it. Never default to `dev`.** A silent
fallback recreates the trunk pollution this rule exists to prevent, and does it
invisibly. If `origin/release/v{version}` does not exist, refuse and ask — do
not create it as a side effect of picking up a ticket.

The issue template asks for the release field; that is a prompt, not a gate —
trackers generally do not enforce non-empty fields. Enforcement is this refusal.

### Base check (mandatory, immediately after worktree creation)

```bash
git merge-base HEAD origin/<resolved-base>
git rev-parse origin/<resolved-base>
```

These two values must be equal. A mis-resolved base must fail in seconds, not
at PR time.

The PR body **must** state the resolved base and the signals it was derived
from (hotfix label / carve-out file list / title prefix + release field), so
a wrong resolution is auditable.

## Issue lifecycle (mandatory)

Aligned with the tracker workflow states: `Todo` → `In Progress` → `In Review` → `Done`.

```text
         resolve base (table; fail closed)
                      │
                      ▼
         git worktree add (branch off origin/<resolved-base>)
                      │
                      ▼
              implement one issue
                      │
                      ▼
         push + gh pr create --base <resolved-base>
         tracker: state = In Review
                      │
                      ▼
         merge authorization for this lane
         (§ Merge authorization; fail closed)
                      │
                      ▼
         merge PR → <resolved-base> (lane's strategy)
         remove worktree + prune branch
         fan-out if that base was dev
         tracker: state = Done
```

### 1. Start: new worktree off the resolved base

From the primary clone (adjust paths to your machine; a sibling directory is
recommended). **Resolve the base first** — see [the table](#resolving-the-base-branch).
The example below uses `origin/dev` (the governance row); substitute
`origin/release/v0.2.0` or `origin/main` when the table says so.

```bash
git fetch origin

# Branch name: type/whi-<id>-short-topic (all lowercase, dash-separated)
ISSUE=whi-123
BRANCH=feat/${ISSUE}-short-topic
WT="../arbitrage-bot-v3-wt/${ISSUE}"
BASE=origin/dev   # or origin/release/vX.Y.Z / origin/main — from the table
PR_BASE=${BASE#origin/}

git worktree add -b "$BRANCH" "$WT" "$BASE"
cd "$WT"

# Verify the base immediately — these two values must be equal
git merge-base HEAD "$BASE"
git rev-parse "$BASE"
```

Tracker: set the issue to **`In Progress`**. Optionally note the worktree path,
branch name, and **resolved base** on the issue.

If the tracker suggests a branch name (e.g. Linear's `gitBranchName`), you may use it —
but the base **must** be the latest `origin/<resolved-base>`, never a stale tip
and never a guessed `dev`.

> ⚠️ **Base trap.** Never assume your tooling's default base. Run the `merge-base` /
> `rev-parse` check above right after creating **any** worktree and confirm the two values
> match the base you intended.
>
> *Runtime aside:* Claude Code's `EnterWorktree` branches from
> `origin/<default-branch>` — which is `main` in this layout, so it is
> **right for the hotfix lane** and **wrong for governance and version-scoped
> work**. Other wrappers have other defaults, which is precisely why the check
> is unconditional.

### 2. Implement

- **One worktree / one branch / one issue / one PR** (never bundle unrelated issues)
- Small commits (`feat:` `fix:` `chore:` `docs:` `refactor:` `test:`)
- Verification has three tiers. Every result must come from the final HEAD; an
  earlier HEAD's result does not carry over:
  1. **Required project checks.** CI required checks, plus any check the project declares
     for every merge in `AGENTS.md` § Build, test, run. These always pass before any
     merge. Nothing in this workflow overrides or skips them.
  2. **Relevant issue checks.** The affected tests, lint/type checks and targeted E2E that
     can catch a failure in what changed. Run them for every PR.
  3. **Full suite.** The project's complete test suite and lint, including any full E2E,
     plus every acceptance criterion in scope. Run it at each release candidate and after
     each fix batch (`/orchestrate` § Release review). PRs that no release acceptance will
     cover (governance, standalone `/implement`, hotfix) also run it before merge.
     Ordinary version issues under `/orchestrate` do not repeat it per issue.
- Never commit secrets, `.env`, large logs, or local data files

### 3. Open PR → the resolved base, enter review

```bash
git push -u origin HEAD
gh pr create --base "$PR_BASE" \
  --title "feat(WHI-123): short description" \
  --body "$(cat <<'EOF'
## Summary
- ...

## Tracker
Closes WHI-123

## Base resolution
- Category: version-scoped | governance | hotfix
- Signals: title prefix `[X.Y.Z]` / Release `X.Y.Z` / hotfix label / carve-out paths
- Worktree base: origin/<resolved-base>
- PR base: <resolved-base>

## Evidence
- Commit: <sha>
- Commands + results: ...
- Role / model / effort: ...
EOF
)"
```

`$PR_BASE` is derived from `$BASE` in step 1 (`${BASE#origin/}`). It is
`dev` only for the governance row, `main` for hotfix, and
`release/v{version}` for version-scoped work. Do not leave it unset —
`gh` would then target the repo default (`main`).

Tracker: set the issue to **`In Review` immediately** (when the PR opens — not after
merge).

PR conventions:

- **base is the resolved base** (version-scoped work never targets `dev`;
  features/fixes never target `main` except an issue labelled `hotfix`, see
  [§ Hotfix](#hotfix))
- Title carries `WHI-NNN`
- Body links the tracker issue **and states the resolved base plus the
  signals it was derived from**
- Merge strategy: **squash and merge** into `dev` or a long-lived
  `release/v*` (per-lane rules:
  [§ Merge strategy](#merge-strategy-per-lane))
- Remote branch is auto-deleted on merge (`delete_branch_on_merge`)
- `gh` must target the right repo and head: if `gh repo view --json nameWithOwner`
  differs from `origin`, pass `--repo <owner/repo>` on every `gh` call; from a worktree
  nested under the primary clone, also pass `--head <owner/repo>:<branch>` and confirm
  `gh pr view <N> --json headRefName` equals the worktree branch. A PR whose head is the
  trunk is closed and recreated, never pushed onto

### 4. Merge authorization

What must hold before a PR merges, and whether an agent may merge it. **Fail closed:** if a
PR fits no row, or fits a row whose conditions you cannot show, it stays at `In Review`.

| Work | Before merge | Agent may merge? |
| --- | --- | --- |
| Ordinary version issue → an existing version-integration `release/v*`, under `/orchestrate` | The issue's acceptance, the required project checks and the relevant issue checks pass on the final HEAD; PR MERGEABLE/CLEAN; the orchestrator has verified the evidence | **Yes** (the orchestrator). Independent review is deferred to the release review of the whole version |
| Bootstrap issue → `dev`, before the first production tag, under `/orchestrate` | Same as above; `dev` is explicitly the first version's integration branch | **Yes**. The first release still needs a full release review |
| Repo-wide governance → `dev` | Required project checks, relevant checks and the full suite, plus **one independent PR review** (`/code-review`, `REVIEWER`) that passed on the final commit | **Yes**, then fan out |
| Standalone `/implement`, no release orchestration taking over | Required project checks, relevant checks and the full suite, plus **one independent PR review** that passed on the final commit | **Yes**, unless a human-gated row below applies. A promised future review does not count |
| Anything touching **the funds path (`AGENTS.md` § High-risk paths)** | The checks of its lane, plus a documented verification approach | **No** — human |
| `hotfix/*` → `main` | Required project checks, relevant checks, the full suite and one independent review, then a human | **No** — every production entry is human-approved |
| Finished version-integration `release/v*` → `dev` | The full suite (complete acceptance) and a passed release review on the **current** SHA (`/orchestrate` § Release review), then a human | **No** |
| Temporary `release/*` cut → `main` | Existing release flow ([§ Releasing to `main`](#releasing-to-main)) | **No** — human |

- "Passed" means no unresolved blocking finding and acceptance valid on the reviewed SHA
  — not zero suggestions (`/code-review` § Findings). A review of an earlier commit does not
  cover a later one: if anything changed after the review, the reviewer checks the final
  commit before merge.
- A human-gated or unreviewable PR stays at `In Review` with the reason recorded. If
  `REVIEWER` is unavailable, see `docs/agents/runtime.md` § Reviewer unavailable.
- Who merges: under `/orchestrate`, the orchestrator (serially, from the primary clone);
  in standalone `/implement`, the implementing agent. Either way, run the post-merge
  cleanup below.

#### Waiving an exception (owner decision)

An owner may waive one human-gated row of the table above for a bounded issue set. A waiver that
lives only in tracker labels does not work — agents obey these docs, not labels —
so a waiver is a **PR against every doc site that states the rule it overrides**,
and it must carry, explicitly: **(1) scope** recognized by the same
machine-checkable signals as the [base-resolution table](#resolving-the-base-branch)
(title prefix + tracker release + label — never prose alone); **(2) expiry** bound
to that tracker Release closing — it does not carry into the next version;
**(3) what it does not relax** — every other row of the table, and any
runtime/operational gate, named one by one; **(4) the compensating control**:
because no human reads the waived diffs, *no acceptance criterion in a waived issue
may depend on a reviewer noticing anything — every criterion must be
machine-checkable* (a test asserting the invariant, a CI check, an exit code);
**(5) a rationale document** under `docs/references/` that the waiver cites.

For the human-gated rows, a human reviewer:

1. Reviews the PR (code + whether it stays within the issue's scope)
2. Verifies against the latest resolved base before merging (primary clone or a
   clean worktree): run the relevant tests / dry-run
3. Merges the PR with the lane's strategy ([§ Merge strategy](#merge-strategy-per-lane))
4. Runs the post-merge cleanup below, plus the lane's own follow-ups (fan-out; for a
   hotfix the tag, deploy and backmerge in [§ Hotfix](#hotfix))
5. Only then sets the tracker to **`Done`** (merged and cleaned up — not released)

### Post-merge cleanup (mandatory, in order)

Drive these from the **primary clone**. Never commit **feature work** to
`dev` or a `release/v*` integration branch directly. Three merges are the
documented exceptions — they are not PR-gated and they push with
`ALLOW_DIRECT_PUSH=1`: fan-out of `dev` into live integration branches
(step 5), the hotfix backmerge of `main` into `dev`
([§ Hotfix](#hotfix) step 7), and the first push of a freshly cut
`release/v*` ([§ Releasing to `main`](#releasing-to-main)).

0. **If the PR is CONFLICTING** (the resolved base advanced since you
   branched): inside the feature worktree,
   `git merge origin/<resolved-base>`, resolve, rerun the affected tests,
   and `git push`. The PR must read **MERGEABLE / CLEAN** before you merge.
1. **Merge with the lane's strategy + drop the remote branch**
   ([§ Merge strategy](#merge-strategy-per-lane)): `gh pr merge <N> --squash
   --delete-branch` into `dev` or a long-lived `release/v*`; `--merge` (a merge commit)
   for `hotfix/*` or `release/*` → `main` and a finished integration branch → `dev`
2. **Remove the worktree:** `git worktree remove <worktree-path>` then
   `git worktree prune`
3. **Delete the local branch:** `git branch -D feat/whi-123-topic`
   (fails while the worktree still holds the branch — do step 2 first)
4. **Fast-forward the resolved base:** `git fetch origin --prune` then
   `git merge --ff-only origin/<resolved-base>` (must fast-forward — if it
   would create a merge commit, your local copy diverged: reset it to
   `origin/<resolved-base>` rather than merging)
5. **Fan-out whenever `dev` advanced:** if this merge — or a backmerge that
   followed it — moved `dev`, merge `dev` into every live version-integration
   branch, in this same session. See
   [§ Fan-out](#fan-out-dev-into-live-version-branches) for the trigger list and
   the query; it is not a shorter list than that one.
6. **Tracker → `Done`**

### State mapping (tracker ↔ git)

| Stage | Tracker state | Git |
|-------|---------------|-----|
| Not started | `Backlog` / `Todo` | no branch |
| Implementing | `In Progress` | worktree + branch exist, no PR (or draft) |
| PR open, awaiting verification / merge | **`In Review`** | open PR → resolved base |
| Waiting for a human, or verification failed | **`In Review`**, reason recorded on the issue | open PR, not merged |
| Merged | **`Done`** | merged into the resolved base with the lane's strategy, cleanup done, fan-out done if that base was `dev`, worktree removed |
| Abandoned | `Canceled` | PR closed, worktree removed, not merged |

Triage labels (`ready-for-agent` / `ready-for-human` / …) are **orthogonal** to workflow
state: labels say *who* does the work, state says *how far along* it is.

## Parallel issues

- One worktree per issue → naturally parallel, no dirty-workspace collisions
- Issues touching the **same module, shared schema, public interface or shared generated
  file** must not run in parallel; serialize, or wait for the earlier PR to merge and
  branch the next worktree off the new resolved base. "Git would merge it cleanly" is not
  evidence that two issues are independent, and a missing `blocks` relation is not either
- An issue whose work builds on another issue's code starts only after that PR merged
- Always `git fetch` + base on `origin/<resolved-base>` before opening a new worktree
- Version-scoped issues on different `release/v*` branches do not collide with
  each other in git; they still collide in the tracker if they share a module

## Fan-out: `dev` into live version branches

Merging `dev` into every live `release/v*` **integration** branch is a
**standing cadence**, not a pre-release step. A long-lived version branch
accrues merge debt faster than features at hotfix cadence, and the overlap
concentrates in the semantically hottest files — in the source project
(`mantle-stocks-arbitrage-bots`), 9 hotfixes in ~2 days put a version branch
11 commits behind with 15 overlapping files. A conflict resolved wrongly
there will not be caught by review when no human reads the diff.

**When:** whenever `dev` advances, in that same session — **any** merge landing on
`dev` triggers it, not just some of them: a hotfix backmerge, a governance PR, and a
finished version-integration branch merging back (which must reach the *other* live
version branches) alike. If the resolved base was `dev`, fan out. A version branch
more than **one release** behind `dev` is a defect: fix the fan-out before further
feature work merges into it.

**What is "live":** every `origin/release/v*` that is not the head of an
open PR into `main` (those are temporary cuts in flight). A freshly cut
integration branch has no unique commits yet — still fan out, or the
first issue on it branches off stale governance. A main-based hotfix
cut (not yet PRed to `main`) is the branch whose commits-not-on-`dev`
are **all** already on `main`: **refuse and surface it**, do not merge.
Do **not** snapshot branch names into this document. Query it:

```bash
git fetch origin
# Fail closed: an unreachable `gh` would leave open_to_main empty, classifying every
# temporary cut in flight as "live" and merging dev's unreleased work into a branch
# queued for main. No answer is not the same as "no open PRs".
if ! open_to_main=$(gh pr list --base main --state open \
                      --json headRefName --jq '.[].headRefName'); then
  echo "refuse: cannot list open PRs into main — fan-out targets are unknown" >&2
else
  for ref in $(git for-each-ref --format='%(refname:short)' \
                 'refs/remotes/origin/release/v*'); do
    name=${ref#origin/}
    if printf '%s\n' "$open_to_main" | grep -qx "$name"; then
      continue   # temporary cut in flight
    fi
    unique=$(git rev-list --count origin/dev.."$ref")
    only_on_ref=$(git rev-list --count origin/dev.."$ref" ^origin/main)
    if [ "$unique" -gt 0 ] && [ "$only_on_ref" -eq 0 ]; then
      echo "refuse: $name is a main-based cut (unique-vs-dev commits all on main)" >&2
      continue
    fi
    echo "$name"
  done
fi
```

(No `exit` in that block — it is meant to be pasted into an interactive shell, and
`exit` would close it.)

For each name, from the **primary clone** (this is the documented
exception to "never commit to a `release/v*` integration branch
directly" — it is a fan-out merge, not feature work):

```bash
# Chained deliberately: if the --ff-only fails (diverged local branch — exactly the
# case it exists to catch), the dev merge must not run anyway.
git checkout "$name" && git merge --ff-only "origin/$name" && git merge origin/dev
# resolve, run the affected tests
ALLOW_DIRECT_PUSH=1 git push
```

The `--ff-only` step is not optional: merging `dev` into a stale local copy of the
branch produces a merge whose "resolution" was made against code that is no longer
there, and the push then either fails or lands a wrong tree.

A green suite after resolving a fan-out conflict is **not** evidence the
resolution is semantically right — the tests were green on both sides of the
merge too. Rerun the affected tests *and* ask of any regression test involved:
*"would this fail if the fix were reverted?"* If the answer is no, the
resolution silently dropped the fix.

`main` is **deliberately excluded**. `main` equals the last deployed tag and
picks rules up at the next release. A hotfix agent branching from `main`
still routes correctly for its own lane even if that copy of these docs is
stale.

**Governance in particular:** a governance rule is only in force on branches that carry it.
A rule landed in `AGENTS.md` on `dev` alone is not in force anywhere else.
Agents working a `0.2.0` issue read
`release/v0.2.0`'s copy. Landing governance on `dev` alone recreates the
problem the next version-scoped PR. Fan out in the same session.

## Choosing a promotion lane: release or hotfix?

Both lanes end in a PR into `main`, and picking the wrong one is how unreviewed work
reaches production. The decision rule is mechanical:

```bash
git fetch origin
git log --oneline origin/main..origin/dev
```

**If that list contains a single commit you are not willing to ship right now, you must
take the hotfix lane.** Cutting a release would drag every one of those commits into
production along with your fix. Only when you are happy to ship the whole list is a
release correct.

| | Release lane | Hotfix lane |
|---|---|---|
| Ships | everything on `dev` | only your fix, on top of what is already live |
| Base | `dev` | **`origin/main`** |
| Use when | `dev` is fully shippable | production is broken and `dev` holds unshippable work |

## Releasing to `main`

**Never** open a PR with `dev` as the head branch into `main` (with
`delete_branch_on_merge` enabled, merging would delete `dev`). Cut a temporary release
branch from `dev`:

```bash
git fetch origin
git checkout dev && git pull --ff-only origin dev
git checkout -b release/v0.1.0

# Bump the project version to match the tag you are about to create
#   (`Cargo.toml` [package] `version`, then `cargo build --locked` so Cargo.lock
#   follows), commit it on the release branch
# release/v* is hook-protected; a new cut has no PR yet.
ALLOW_DIRECT_PUSH=1 git push -u origin HEAD
gh pr create --base main --title "release: v0.1.0" --body "..."
```

**Open the PR immediately after the push.** Until it exists, the fan-out query has
no way to tell this temporary cut from a live integration branch — it discriminates
on the open PR into `main` — and a concurrent fan-out in that window would merge
`dev`'s unreleased work into the branch you are about to ship.

After the PR reads MERGEABLE/CLEAN and a human has approved it (`release/*` → `main` is
**always** a human gate):

1. **Merge with a merge commit, not squash** (see
   [§ Merge strategy](#merge-strategy-per-lane))
2. Create the annotated tag and the GitHub Release: `git tag -a v0.1.0 -m "v0.1.0"` +
   `git push origin v0.1.0`
3. **Deploy from the tag**, never from a branch
4. Backfill the tracker Release's `commitSha` (see [§ Version axis](#version-axis))

Temporary `release/*` branches may be auto-deleted; `dev` lives forever.

## Hotfix

**When to take this lane:** production is broken *and* `dev` already holds work that must
not ship yet — see [§ Choosing a promotion lane](#choosing-a-promotion-lane-release-or-hotfix).
`origin/main` is by definition the code running in production, so it is the only correct
base.

```bash
git fetch origin
git worktree add -b hotfix/whi-123-short-topic \
    ../arbitrage-bot-v3-wt/hotfix-whi-123 origin/main

# Verify the base immediately — these two values must be equal
git -C ../arbitrage-bot-v3-wt/hotfix-whi-123 merge-base HEAD origin/main
git rev-parse origin/main
```

Then:

1. Fix **only** this one issue
2. **Bump the project version to a patch release** (`0.1.5` → `0.1.5.1`) — otherwise tag
   `v0.1.5.1` points at a tree that calls itself `0.1.5`. Cargo rejects a four-segment
   `version`, so `Cargo.toml` carries it as semver build metadata:
   `version = "0.1.5+hotfix.1"` for tag `v0.1.5.1`
3. `gh pr create --base main`, title/body carry `WHI-NNN`; tracker →
   `In Review`
4. Required project checks, relevant checks, the full suite and one independent review
   (`/code-review`) pass, then **a human
   approves** — every hotfix is a production entry
   ([§ Merge authorization](#4-merge-authorization)). **Merge with a merge commit, not
   squash** (see [§ Merge strategy](#merge-strategy-per-lane))
5. Tag `v0.1.5.1` + create the GitHub Release
6. **Deploy from the tag**, not from a branch
7. **Merge `main` back into `dev` — and push it.** From the **primary clone** (inside
   the hotfix worktree `git checkout dev` fails — `dev` is checked out elsewhere), and
   only after a fetch, or `origin/main` still predates the merge GitHub just made and
   this merges nothing:
   `git fetch origin && git checkout dev && git merge --ff-only origin/dev && git merge origin/main`,
   then `ALLOW_DIRECT_PUSH=1 git push`. The `--ff-only` is chained deliberately, as in
   fan-out: if the local `dev` diverged, the `origin/main` merge must not run. Like
   fan-out, this backmerge is a **documented direct-merge exception**, not PR-gated
   (see [§ Branch protection](#branch-protection) if a server-side ruleset blocks the
   push). Skip the merge and the next release re-ships the bug you just fixed; skip
   the **push** and step 8 fans out an `origin/dev` that never received the fix — the
   fan-out then silently ships nothing, which is the failure step 8 exists to prevent
8. **Fan out `dev` into every live version-integration branch** (see
   [§ Fan-out](#fan-out-dev-into-live-version-branches)). Skip this and the
   next version ships without the hotfix. State the query, never a name list.
9. Tracker: issue → `Done`, the corresponding Release → `Released`, and fill in that
   Release's `commitSha`

When production is live, hotfixes outrank regular issues — anything on the funds
path (`AGENTS.md` § High-risk paths) takes this route.

## Version axis

The version number is **pinned in advance** and appears in three places that must agree.
If any one of them disagrees, a step was skipped:

| Place | Artifact | Question it answers |
|-------|----------|---------------------|
| Tracker | a Release / version entity (`0.1.5`, `0.1.5.1`, `0.1.6`, …) | **Plan**: which issues are in this version |
| Git | annotated tag `v0.1.5` | **Fact**: which tree this version is |
| GitHub | the GitHub Release on that tag | **Ship**: public changelog and artifacts |

- Every version-scoped issue belongs to exactly one tracker Release, and that
  Release **must match** the `[X.Y.Z]` title prefix. A missing or disagreeing
  Release is an implementation refuse, not a default-to-`dev`. Governance
  issues have no Release. Once a Release has issues attached, those issues
  leave `Backlog` for `Todo`.
- After shipping, backfill the tracker Release's **`commitSha`** (the commit the tag points
  at). This is the only authoritative binding between "a version" and "some code".
- Milestones and Releases are **orthogonal axes**: a milestone is *which capability stage*,
  a Release is *when it ships* (and how git routes). Don't use milestones to
  express release batches, and don't put the milestone in the title.
- Hotfixes use a four-segment version (`0.1.5.1`) in the tracker and the tag. Cargo only
  accepts three-segment semver, so `Cargo.toml` spells it `0.1.5+hotfix.1` (see
  [§ Hotfix](#hotfix) step 2). **Don't** reuse the next minor number; the tracker has
  already promised that number to a feature batch.

> **Footgun: annotated tags don't `rev-parse` to commits.** `git rev-parse v0.1.5`
> returns the *tag object* SHA, not the commit it points at — comparing it against
> a branch head is always unequal and reads as "the branch has moved past the tag"
> even when it hasn't. Dereference explicitly: `git rev-parse 'v0.1.5^{commit}'`,
> or ask the real question with `git merge-base --is-ancestor v0.1.5 <branch>`.

## Merge strategy (per lane)

| PR | Merge with | Why |
|----|-----------|-----|
| `feat/*` `fix/*` `chore/*` → `dev` | **squash** | Governance: one issue, one commit; clean `dev` history |
| `feat/*` `fix/*` `chore/*` → long-lived `release/v*` | **squash** | Same, on the version integration branch |
| Finished integration `release/v*` → `dev` | **merge commit** | Preserves the version's ancestry on the trunk |
| `release/*` → `main` (temporary cut) | **merge commit** | Preserves ancestry |
| `hotfix/*` → `main` | **merge commit** | Same |

**Why releases and hotfixes must not be squashed:** squashing creates a brand-new commit
on `main` that exists nowhere in `dev`'s history, so the tag lands on a branch
disconnected from `dev`. The damage is silent but permanent:
`git merge-base --is-ancestor v0.1.5 origin/dev` returns false, and
`git log v0.1.5..origin/dev` — "what's changed since the last release" — returns garbage.
Both of those are the queries you most want to trust at release time.

This means the GitHub repo must have **both** `Allow squash merge` and
`Allow merge commit` enabled, with the lane deciding which you use.

## Branch naming

| Type | Format | Example |
|------|--------|---------|
| Feature | `feat/whi-<id>-<topic>` | `feat/whi-101-user-auth` |
| Fix | `fix/whi-<id>-<topic>` | `fix/whi-112-race-condition` |
| Chore | `chore/whi-<id>-<topic>` | `chore/whi-108-lint-config` |
| Hotfix | `hotfix/whi-<id>-<topic>` | `hotfix/whi-140-login-loop` |
| Release | `release/v<version>` | `release/v0.1.0`, `release/v0.1.5.1` |

- All lowercase, words joined with `-`
- **Must include the tracker id** (`whi-NNN`) for PR ↔ issue tracing —
  hotfixes included, they are tracked issues too
- One PR does one thing

## Recommended worktree layout

```text
~/Work/src/personal/arbitrage-bots/
  arbitrage-bot-v3/              # primary clone (stays on dev)
  arbitrage-bot-v3-wt/
    whi-101/  # worktree
    whi-105/
    hotfix-…/
```

- Worktree directories go **outside** the primary repo (avoid nested-git confusion)
- The primary repo's `.gitignore` does not need to ignore sibling worktrees

## Forbidden

- Developing an issue in the primary clone's working tree (always use a worktree)
- Pushing feature commits directly to `main` / `dev` (always via PR)
- Force-pushing to `main` / `dev`
- Long-lived giant branches with no PR
- Bundling multiple unrelated issues in one PR
- Leaving the tracker in `In Progress` after the PR opens (must be `In Review`)
- Leaving the tracker un-`Done` after the PR merges
- Continuing work on a branch not based on latest `origin/<resolved-base>`
  (drop the worktree, re-branch from the fresh resolved base)
- **Defaulting a version-scoped issue to `dev`** because the release field is
  empty or the release branch does not exist — refuse and surface it
- **Creating `release/v*` as a side effect of picking up a ticket**
- Committing `.env`, private keys, or API secrets
- **Deploying from a branch.** The only valid deploy source is a tag — otherwise nobody
  can say which tree production is running
- **Cutting a release to ship one urgent fix.** If `git log origin/main..origin/dev`
  contains anything that must not ship yet, take the hotfix lane
- **Landing a hotfix on `main` without merging `main` back into `dev`.** The next release
  will re-ship the bug you just fixed
- **Backmerging a hotfix to `dev` without pushing `dev`.** The fan-out that follows
  reads `origin/dev`, so an unpushed backmerge makes step 8 ship nothing at all
- **Landing anything on `dev` without fan-out** to live `release/v*` integration
  branches in the same session — a hotfix backmerge, a governance PR, or a finished
  version-integration merge-back alike. The next version ships without it
- **Squash-merging `release/*` or `hotfix/*` into `main`** (breaks the tag's ancestry with
  `dev` — see [§ Merge strategy](#merge-strategy-per-lane))

## Agent / automation constraints

Implementing agents (including unattended ones) **must**:

1. Change code only inside a worktree — never commit directly to `dev` in the primary clone
2. Move the tracker in lockstep: `In Progress` on start → `In Review` when the PR opens →
   `Done` only after confirming the merge and the post-merge cleanup
   (under `/orchestrate` the orchestrator owns these transitions and the implementer
   reports facts — `docs/agents/issue-tracker.md` § Issue lifecycle)
3. Resolve the PR base from the [resolution table](#resolving-the-base-branch)
   **before** creating the worktree. Open the PR against that base.
   Version-scoped work targets `release/v{version}`; governance targets
   `dev`; `hotfix` targets `main`. **Never default to `dev`.** If the
   issue does not fall into exactly one row, refuse to start. State the
   resolved base and the signals in the PR body
4. Merge only what [§ Merge authorization](#4-merge-authorization) lets an agent merge,
   with the evidence that row requires on the final HEAD; then run post-merge cleanup and
   set the tracker to `Done`. Everything else stops at `In Review` with the reason.
   **Independent review means a context other than the implementing one**
   (`docs/agents/runtime.md`); if `REVIEWER` is unavailable, a row that needs it is not
   satisfied — stop at `In Review` and say which role was missing. A report of "done"
   without a merged PR is not `Done`
5. Respect module isolation when several issues run in parallel (see
   [§ Parallel issues](#parallel-issues))
6. **Never** self-merge or deploy a change touching **the funds path (`AGENTS.md` § High-risk paths)**, even with a
   green test run — stop at `In Review` for human confirmation. This gate **overrides every
   agent-merge row in #4.** No waiver of it is in force; if the owner ever
   grants one it must take the shape in
   [§ Waiving an exception](#waiving-an-exception-owner-decision) and amend this rule
   here, not merely a tracker label
7. Never merge `release/*` → `main` or `hotfix/*` → `main` without human approval.
   Never merge a finished version-integration branch into `dev` without human approval
   and a passed release review on its current SHA.

## Branch protection

Configure server-side protection on `main` and `dev` as soon as the repo's plan
allows it: require a PR, forbid force-pushes, forbid deletion. On live
`release/v*` integration branches, forbid force-pushes and deletion only —
**do not** require a PR server-side. Fan-out is a direct merge of `dev` from
the primary clone; a require-PR ruleset has no `ALLOW_DIRECT_PUSH` equivalent
and would make the standing cadence impossible.

Require-a-PR on `dev` is still right, but it blocks one legitimate push: the
hotfix backmerge ([§ Hotfix](#hotfix) step 7) lands on `dev` directly too. Keep a
bypass for the owner (classic protection: leave *Do not allow bypassing the above
settings* unchecked; rulesets: a bypass entry) — or, where no bypass is available,
land that backmerge as a PR from a throwaway branch cut off `origin/main` into
`dev`, merged with a **merge commit**, and fan out after it.

> GitHub returns `403 Upgrade to GitHub Pro or make this repository public` for both
> classic branch protection and rulesets on **private repos on the Free plan**. If that
> applies, server-side protection is simply unavailable — the remote is genuinely
> unprotected and workflow discipline is the only real safeguard.

### Fallback: the local `pre-push` hook

While server-side protection is unavailable, `.githooks/pre-push` (checked in) blocks the
most common mistake — pushing straight to `main`, `dev`, or a `release/v*`
integration branch. Enable it **once per clone and per worktree**:

```bash
git config core.hooksPath .githooks
```

Deliberate override, when you genuinely need it — the three documented cases, all
from the primary clone: the fan-out merge of `origin/dev` into a live `release/v*`,
the hotfix backmerge of `main` into `dev` ([§ Hotfix](#hotfix) step 7), and the first
push of a freshly cut `release/v*`:

```bash
ALLOW_DIRECT_PUSH=1 git push ...
```

This guards only the local clone. It is **not** a server-side gate.

## Repo-specific notes

- **Archive points:** tag `v0.1.0-archive` (unusable, not a production baseline) and the
  `archive/feat-for-ubuntu` / `archive/feat-support-all-in-one` tags. Do not branch
  from them.
- **GitHub merge settings** required by [§ Merge strategy](#merge-strategy-per-lane):
  squash **and** merge commit allowed, rebase off, delete branch on merge. Set with
  `gh repo edit --enable-squash-merge --enable-merge-commit --enable-rebase-merge=false --delete-branch-on-merge`.
- **Contract changes:** a PR touching `contracts/` also runs the Foundry suite and
  `SKIP_FORGE=0 cargo build --locked`, and commits any regenerated `src/amms/abi/`
  (`AGENTS.md` § Build, test, run).
