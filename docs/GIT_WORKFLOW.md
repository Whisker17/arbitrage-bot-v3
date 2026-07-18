# Git Workflow

本仓库采用 **main + dev + feature branches** 模型。

历史归档点：`v0.1.0-archive`（不可用 / 非生产基线）。

## 分支角色

| 分支 | 角色 | 谁可以写 |
|------|------|----------|
| `main` | 稳定发布线。只接受来自 `dev` 的 release 合并与 hotfix。 | PR only（保护后禁止直推） |
| `dev` | 当前版本的活跃集成分支。日常开发的默认目标分支。 | PR from `feat/*` / `fix/*` |
| `feat/*` | 功能迭代 | 开发者自由分支，短生命周期 |
| `fix/*` | Bug 修复 | 开发者自由分支，短生命周期 |
| `chore/*` | 工具/文档/构建等非功能改动 | 开发者自由分支，短生命周期 |
| `hotfix/*` | 线上紧急修复（从 `main` 拉出） | 开发者，合并回 `main` 后需同步 `dev` |

## 日常开发流程

```text
main  ─── 仅 release / hotfix
  ▲
  │  release PR（squash）
dev  ─── 当前版本集成
  ▲
  │  feature PR（squash）
feat/xxx
```

1. **从最新 `dev` 开分支**（不要从 `main` 开功能分支）

   ```bash
   git fetch origin
   git checkout dev
   git pull --ff-only origin dev
   git checkout -b feat/short-description
   ```

2. **小步提交**，commit message 建议：

   - `feat: ...` 新功能
   - `fix: ...` 修复
   - `chore: ...` 杂项
   - `docs: ...` 文档
   - `refactor: ...` 重构
   - `test: ...` 测试

3. **推送并开 PR → `dev`**

   ```bash
   git push -u origin HEAD
   gh pr create --base dev --title "feat: ..." --body "..."
   ```

4. **PR 合并策略**

   - 默认 **Squash and merge**（仓库已开启；merge commit / rebase 已关闭）
   - 合并后远程 feature 分支会自动删除（`delete_branch_on_merge=true`）
   - 本地可清理：`git fetch --prune && git branch -d feat/xxx`

5. **发布到 `main`**

   - 从 `dev` 开 release PR → `main`
   - 合并后打 tag / 发 GitHub Release，例如 `v0.2.0`
   - 若需要，从 `main` 回合同步到 `dev`（通常 fast-forward 或 merge back）

## Hotfix 流程

```bash
git fetch origin
git checkout main
git pull --ff-only origin main
git checkout -b hotfix/critical-bug
# ... fix ...
git push -u origin HEAD
gh pr create --base main --title "hotfix: ..."
```

合并进 `main` 并打补丁 tag 后，**必须**把修复同步回 `dev`：

```bash
git checkout dev
git pull --ff-only origin dev
git merge origin/main   # 或 cherry-pick hotfix commit
git push origin dev
```

## 分支命名

| 类型 | 格式 | 示例 |
|------|------|------|
| 功能 | `feat/<topic>` | `feat/moe-lbt-gas` |
| 修复 | `fix/<topic>` | `fix/tick-sync-reorg` |
| 杂项 | `chore/<topic>` | `chore/ci-clippy` |
| 热修 | `hotfix/<topic>` | `hotfix/nonce-stuck` |

- 全小写，单词用 `-` 连接
- 一个 PR 只做一件事，避免巨型分支

## 禁止事项

- 禁止向 `main` 直接 push（保护开启后会强制）
- 禁止长期存活、无 PR 的巨型 `feat/*` 分支
- 禁止 force-push 到 `main` / `dev`
- 禁止在未同步 `dev` 的旧分支上继续开发（先 rebase/merge 最新 `dev`）

## 历史分支归档

分支清理时，未完全合入 `main` 的 tip 已打 archive tag，需要时可检出：

```bash
git checkout archive/feat-for-ubuntu
git checkout archive/feat-support-all-in-one
```

## main 分支保护（需 GitHub Pro 或公开仓库）

当前仓库为 **private**，GitHub Free 无法启用 Branch protection / Rulesets。

启用方式二选一：

1. 升级账户到 **GitHub Pro**，或
2. 将仓库改为 **public**（若可接受）

然后在 GitHub 上设置（或让有权限的人执行下方 API）：

**推荐规则（`main`）：**

- Require a pull request before merging
- Require approvals: ≥ 1（若单人开发可设 0 但保留 PR 门禁）
- Dismiss stale reviews when new commits are pushed
- Require status checks to pass（配置 CI 后）
- Require conversation resolution before merging
- Do not allow bypassing the above settings
- Restrict who can push（仅维护者）
- Block force pushes
- Block deletions

**推荐规则（`dev`，可选但建议）：**

- Require a pull request before merging
- Block force pushes
- Block deletions

### 启用后可用的 CLI 示例

```bash
# main protection（需 Pro / public）
gh api -X PUT repos/Whisker17/arbitrage-bot-v3/branches/main/protection \
  --input - <<'EOF'
{
  "required_status_checks": null,
  "enforce_admins": true,
  "required_pull_request_reviews": {
    "dismiss_stale_reviews": true,
    "require_code_owner_reviews": false,
    "required_approving_review_count": 0
  },
  "restrictions": null,
  "allow_force_pushes": false,
  "allow_deletions": false,
  "required_conversation_resolution": true
}
EOF
```

在保护启用前，请**约定**不要直推 `main`，一律走 PR。

## 当前仓库合并设置

| 设置 | 值 |
|------|-----|
| Default branch | `main` |
| Allow merge commit | off |
| Allow squash merge | on |
| Allow rebase merge | off |
| Delete branch on merge | on |
