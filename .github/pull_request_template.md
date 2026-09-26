## Summary

<!-- What this PR does and why. Tracker issue: WHI-NNN -->

## Base branch

Resolved from `docs/GIT_WORKFLOW.md` § Resolving the base branch.
**Never default to `dev`.**

- [ ] `release/vX.Y.Z` (version-scoped) → merge with **squash**
- [ ] `dev` (repo-wide governance carve-out only) → merge with **squash**
- [ ] `dev` (finished version-integration `release/v*` merge-back) → **merge commit**,
      never squash — **human gate**
- [ ] `main` (hotfix, or temporary `release/*` cut) → merge with a **merge commit**,
      never squash — **human gate**

<!-- Signals this base was derived from: title prefix `[X.Y.Z]` / tracker
Release / hotfix label / carve-out paths. Targeting `main`? If
`git log origin/main..origin/dev` holds anything that must not ship yet,
this has to be a hotfix off `origin/main`, not a release. -->

## Merge authorization

Row of `docs/GIT_WORKFLOW.md` § Merge authorization this PR is under:

- [ ] Ordinary version issue → integration branch, under `/orchestrate` (release review later)
- [ ] Bootstrap issue → `dev`, under `/orchestrate`
- [ ] Governance → `dev` / standalone `/implement` — independent review passed on commit: `<sha>`
- [ ] Human gate (high-risk path, hotfix, `release/*` → `main`, finished `release/v*` → `dev`)

<!-- Finished `release/v*` → `dev`: link the `Release X.Y.Z — orchestration` document,
baseline B, reviewed candidate H and the final review round. -->

## Evidence

- Commit SHA verified:
- Commands and results (actual output or a link, not "tests pass"):
- Artifacts (reports, logs, screenshots), if any:
- Role / model / effort used:
- New dependency or significant abstraction, and why it is needed (omit if none):

## Fan-out (any PR landing on `dev`)

- [ ] After this merges, `dev` fans out into every live `release/v*` integration
      branch **in this same session** (`docs/GIT_WORKFLOW.md` § Fan-out) —
      a governance rule is only in force on branches that carry it.
      N/A if this PR does not target `dev`.

## Release / hotfix only

- [ ] Project version bumped to match the tag being created
- [ ] Tag + GitHub Release planned; deploy will come **from the tag**, not a branch
- [ ] Hotfix: `main` will be merged back into `dev` **and `dev` pushed** after this
      lands, then `dev` fans out per the section above (an unpushed backmerge makes
      the fan-out ship nothing)
- [ ] Tracker Release `commitSha` will be backfilled after tagging

## Type

- [ ] feat
- [ ] fix
- [ ] chore
- [ ] docs
- [ ] hotfix
- [ ] release

## Checks

Tiers per `docs/GIT_WORKFLOW.md` § 2 Implement, all on the final HEAD:

- [ ] Required project checks (CI `verify-toolchain` / `cargo-locked` / `forge-and-abi`,
      plus what `AGENTS.md` § Build, test, run marks for every merge) pass
- [ ] Relevant issue checks (affected tests, lint/type checks, targeted E2E) pass
- [ ] Full suite (complete tests + lint, incl. any full E2E) — governance, standalone
      `/implement`, hotfix and release candidates only; N/A for an ordinary issue under
      `/orchestrate`
- [ ] If this touches `contracts/`: Foundry suite run and `SKIP_FORGE=0 cargo build --locked`
      leaves `src/amms/abi/` unchanged (or the regenerated ABI is committed)
- [ ] If this touches the funds path (`AGENTS.md` § High-risk paths): verification
      approach documented (fork / Sepolia / shadow dry-run / mocked)
- [ ] No new tunable parameter without a source (spec section or measured evidence), or
      the deviation is explained in the Summary

## Notes

<!-- Risks, rollback plan, follow-up TODOs. Review findings consciously left unfixed
go to docs/DEFERRED_ISSUES.md in this PR, with the reason. -->
