# Git Workflow

本仓库采用 **main + dev + issue worktree** 模型。

历史归档点：`v0.1.0-archive`（不可用 / 非生产基线）。当前实现快照：`v0.2.0`。

## 一句话规则

**每个 Linear issue 的实现 = 从最新 `dev` 开一个独立 git worktree + 短生命周期分支 → PR 合入 `dev` → Linear 状态随 PR 走。**

禁止在主 clone 里直接 `git checkout -b` 开发 issue（会污染 `dev` 工作区、无法并行多 issue）。  
主 clone 只用于：拉 `dev`/`main`、开 worktree、做 Reviewer 合并后的验证。

## 分支角色

| 分支 | 角色 | 谁可以写 |
|------|------|----------|
| `main` | 稳定发布线。只接受来自 `dev` 的 release 合并与 hotfix。 | PR only |
| `dev` | 当前版本的活跃集成分支。日常开发的 **唯一 PR 目标**。 | PR from issue branches |
| `feat/*` `fix/*` `chore/*` | 单 issue 实现分支（在 worktree 内） | 短生命周期 |
| `hotfix/*` | 线上紧急修复（从 `main` 拉出） | 合并回 `main` 后同步 `dev` |
| `release/*` | 临时晋升分支（从 `dev` 切，PR → `main`） | 短生命周期 |

## Issue 开发生命周期（强制）

与 Linear workflow states 对齐：`Todo` → `In Progress` → `In Review` → `Done`。

```text
                fetch latest dev
                      │
                      ▼
         git worktree add (branch off origin/dev)
                      │
                      ▼
              implement one issue
                      │
                      ▼
         push + gh pr create --base dev
         Linear: state = In Review
                      │
                      ▼
              review + CI green
                      │
                      ▼
         squash-merge PR → dev
         Linear: state = Done
         remove worktree + prune branch
```

### 1. 开工：基于最新 `dev` 新开 worktree

主 clone 内执行（路径可按本机调整，建议 sibling 目录）：

```bash
# 主 clone
git fetch origin
git checkout dev
git pull --ff-only origin dev

# 分支名：类型/whi-<id>-短描述（全小写，- 连接）
ISSUE=whi-502
BRANCH=fix/${ISSUE}-dynamic-gas
WT="../arbitrage-bot-v3-wt/${ISSUE}"

git worktree add -b "$BRANCH" "$WT" origin/dev
cd "$WT"
```

Linear：

- 将 issue 设为 **`In Progress`**
- 可在 PR/注释里贴 worktree 路径与 branch 名

可选：使用 Linear 建议的 `gitBranchName`（issue 详情里），但 **必须以最新 `origin/dev` 为 base**，不要基于过期 tip。

### 2. 实现

- **一个 worktree / 一个分支 / 一个 issue / 一个 PR**（禁止塞多个无关 issue）
- 小步 commit（`feat:` `fix:` `chore:` `docs:` `refactor:` `test:`）
- 在 worktree 内跑相关测试；合并前 Reviewer 会再跑全量门禁
- 不要 commit 密钥、`.env`、大日志

### 3. 开 PR → `dev`，进入 Review

```bash
git push -u origin HEAD
gh pr create --base dev \
  --title "fix(WHI-502): dynamic gas with fail-closed estimate" \
  --body "$(cat <<'EOF'
## Summary
- ...

## Linear
Closes WHI-502  # 或链接 https://linear.app/.../WHI-502

## Test plan
- [ ] cargo test …
EOF
)"
```

Linear：**立刻**把 issue 状态设为 **`In Review`**（PR 已开、等 review，不是合并后才改）。

PR 约定：

- **base 必须是 `dev`**（功能/修复绝不直接打 `main`）
- 标题带 `WHI-NNN`
- body 链到 Linear issue
- 合并策略：**Squash and merge**（仓库已开；merge commit / rebase 已关）
- 合并后远程分支自动删除（`delete_branch_on_merge=true`）

### 4. Review 通过并 merge → Done

Reviewer / 维护者：

1. Review PR（代码 + 是否只触及该 issue 范围）
2. 合并前在最新 `dev` 上下文验证（可在主 clone 或干净 worktree）：

   ```bash
   cargo build --all-targets && cargo test --all-targets
   ```

   （`WHI-522` 差分测试落地后一并跑）

3. Squash-merge PR 到 `dev`
4. Linear：issue 状态设为 **`Done`**
5. 清理本地 worktree：

   ```bash
   # 在主 clone
   git worktree remove "../arbitrage-bot-v3-wt/whi-502"
   git fetch --prune
   git branch -d fix/whi-502-dynamic-gas 2>/dev/null || true
   ```

### 状态对照（Linear ↔ Git）

| 阶段 | Linear state | Git |
|------|----------------|-----|
| 未开工 | `Backlog` / `Todo` | 无分支 |
| 实现中 | `In Progress` | worktree + branch 存在，尚无 PR 或 PR draft |
| PR 已开待审 | **`In Review`** | open PR → `dev` |
| 已合入 | **`Done`** | squash-merged into `dev`，worktree 已删 |
| 放弃 | `Canceled` | 关 PR、删 worktree，不 merge |

Triage labels（`ready-for-agent` / `ready-for-human` / …）与 workflow state **正交**：label 管“谁来做”，state 管“做到哪一步”。

## 并行多 issue

- 每个 issue 独立 worktree → 天然并行，互不脏工作区
- **同一文件域**的 issue 不要并行改（见 `docs/guides/implementation-roadmap.md` 的 lane 划分）；串行或等前序 PR merge 后再从新 `dev` 开 worktree
- 开新 worktree 前始终 `git fetch` + base `origin/dev`，避免基于过期分支

## 发布到 `main`

**不要**直接把 `dev` 当 PR head 合进 `main`（`delete_branch_on_merge` 会删掉 `dev`）。从 `dev` 切临时 release 分支：

```bash
git fetch origin
git checkout dev && git pull --ff-only origin dev
git checkout -b release/v0.3.0
git push -u origin HEAD
gh pr create --base main --title "release: v0.3.0" --body "..."
```

- 合并后打 tag / GitHub Release
- 临时 `release/*` 可被自动删除；`dev` 长期保留
- `main` 上有 hotfix 时，合并后必须同步回 `dev`

## Hotfix

```bash
git fetch origin
git worktree add -b hotfix/critical-bug ../arbitrage-bot-v3-wt/hotfix-critical origin/main
# … fix, PR → main …
# merge 后把修复同步回 dev（merge 或 cherry-pick）
```

## 分支命名

| 类型 | 格式 | 示例 |
|------|------|------|
| 功能 | `feat/whi-<id>-<topic>` | `feat/whi-510-market-snapshot` |
| 修复 | `fix/whi-<id>-<topic>` | `fix/whi-502-dynamic-gas` |
| 杂项 | `chore/whi-<id>-<topic>` | `chore/whi-508-dead-code` |
| 热修 | `hotfix/<topic>` | `hotfix/nonce-stuck` |
| 发布 | `release/v<semver>` | `release/v0.3.0` |

- 全小写，单词用 `-` 连接
- **必须含 Linear id**（`whi-NNN`），便于 PR ↔ issue 追踪
- 一个 PR 只做一件事

## Worktree 布局建议

```text
~/Work/src/personal/
  arbitrage-bot-v3/              # 主 clone（常驻 dev）
  arbitrage-bot-v3-wt/
    whi-502/                     # worktree
    whi-505/
    hotfix-…/
```

- worktree 目录 **不要** 放进主 repo 内部（避免嵌套 git 混乱）
- 主 repo 的 `.gitignore` 不需要忽略 sibling worktree

## 禁止事项

- 禁止在主 clone 工作区直接开发 issue（必须 worktree）
- 禁止向 `main` / `dev` 直接 push 功能提交（一律 PR）
- 禁止 force-push 到 `main` / `dev`
- 禁止长期存活、无 PR 的巨型分支
- 禁止一个 PR 塞多个无关 Linear issue
- 禁止 PR 已开却不把 Linear 设为 `In Review`
- 禁止 PR 已 merge 却不把 Linear 设为 `Done`
- 禁止在未基于最新 `origin/dev` 的旧分支上继续开发（丢 worktree，从新 `dev` 重开）

## Agent / 自动化约束

实现类 agent（含 AFK）**必须**：

1. 只在 worktree 内改代码，不在主 clone 的 `dev` 上直接 commit
2. 开工设 Linear → `In Progress`；开 PR 后 → `In Review`；确认 squash-merge 后 → `Done`
3. PR base = `dev`，标题/正文含 `WHI-NNN`
4. 不 merge 自己的 PR（除非用户明确授权）；默认停在 `In Review` 等 review
5. 多 issue 并行时遵守 lane/文件域隔离（见 implementation roadmap）

## 历史分支归档

```bash
git checkout archive/feat-for-ubuntu
git checkout archive/feat-support-all-in-one
```

## main / dev 分支保护

当前仓库为 **private**，GitHub Free 可能无法启用 Branch protection。约定仍然生效：不直推 `main`/`dev`。

启用后推荐：`main`/`dev` 均 Require PR、禁 force-push、禁删除；`main` 可加 conversation resolution。

## 当前仓库合并设置

| 设置 | 值 |
|------|-----|
| Default branch | `main` |
| Allow merge commit | off |
| Allow squash merge | on |
| Allow rebase merge | off |
| Delete branch on merge | on |
