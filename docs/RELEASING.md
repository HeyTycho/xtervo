# xtervo 版本与发布管理

> 状态：v1（2026-11-09）
> 适用：`Cargo.toml`、`scripts/package.sh`、`dist/`、Git tag、CHANGELOG。

## 0. 一句话

**一个版本 = 一个 SemVer 号 + 一个不可变的 annotated tag + 一组可复现产物。**

不要用日期 tag、不要移动 tag、不要对同一版本重复发布不同二进制。

---

## 1. 版本号来源

唯一来源：

```toml
# Cargo.toml
[workspace.package]
version = "0.6.0"
```

派生位置：

| 位置 | 来源/行为 |
|---|---|
| `xtervo --version` | 编译期 `CARGO_PKG_VERSION` |
| tar.gz 文件名 | `xtervo-<version>-linux-<arch>.tar.gz` |
| .deb 版本 | `<version>-1`，可用 `DEB_VERSION` 覆盖 |
| BUILD-INFO / SHA256 | `scripts/package.sh` 自动生成 |
| commit / built | build.rs 写入 `--version`，dirty 时标 `(dirty)` |

发版前必须同时更新：

```text
Cargo.toml       workspace.package.version
Cargo.lock       随 cargo build/update 自动更新
CHANGELOG.md     对应版本条目
docs/RETEST_CHECKLIST.md  版本/commit/哈希
```

---

## 2. SemVer 规则（0.x 阶段）

xtervo 在 1.0 前采用“0.x 也要可预期”的规则：

| 变更 | 版本 |
|---|---|
| Bugfix、安全修复、文档/打包修复，无行为变化 | `PATCH` `0.1.1` |
| 新功能、明显行为变化、配置/CLI/菜单语义变化 | `MINOR` `0.6.0` |
| 稳定公共接口破坏（1.0 后） | `MAJOR` `1.0.0` |

0.x 约定：

- 新功能用 `MINOR`，不要塞进 `PATCH`。
- 破坏性变更在 `MINOR` 里也必须在 CHANGELOG 标 **Breaking**。
- 只有真正的 bugfix 才发 `PATCH`；不要为了“重新打包”升 PATCH。
- 同一版本不允许“修一下再发一次”；必须升版本。

预发布：

```text
v0.6.0-rc.1
v0.6.0-beta.2
```

- 预发布也占版本号，不能与正式版共用 tag。
- 排序按 SemVer；不要用 `+build` 承担排序语义。
- 构建 metadata `+gitsha` 可以出现在 `--version`，不要放进 tag。

插件 ABI 与应用版本分开管理；插件 manifest 的 `host_abi` 不跟随每次 app PATCH 跳变。

---

## 3. Tag 规范

### 3.1 格式

```text
vMAJOR.MINOR.PATCH
vMAJOR.MINOR.PATCH-rc.N
vMAJOR.MINOR.PATCH-beta.N
```

示例：

```text
v0.5.0
v0.6.0
v0.6.1
v0.7.0-rc.1
```

### 3.2 规则

- 必须是 **annotated tag**：

```bash
git tag -a v0.6.0 -m "xtervo 0.6.0"
```

- tag 必须指向**实际构建产物的 commit**（release commit）。
- tag 一旦推送，**不可移动、不可覆盖、不可删除后重打**。
- 一个版本只对应一个 tag；hotfix 升 PATCH 重新打 tag。
- 不再新增 `final-YYYY-MM-DD`、`stable-*`、`release-*` 这类日期/阶段 tag。
- 历史 tag 保留但视为 legacy，不作为当前发布依据。
- tag 推送：

```bash
git push origin v0.6.0
```

- 可选签名：

```bash
git tag -s v0.6.0 -m "xtervo 0.6.0"
```

### 3.3 当前状态

```text
当前版本：0.6.1 发布线
预发布 tag：v0.6.0-dev.N（历史）
正式 tag：v0.6.1（发布 commit 以 BUILD-INFO 为准；v0.6.0 为流水线失败的空发布）
历史 tag：v0.5.0 / v0.3.1 / v0.1.0 / final-2026-11-09 / stable-*-2026-11-06（legacy，不再使用）
下一个功能版本：0.7.0
```

---

## 4. 发布流程

### 4.1 冻结

1. 确认要发布的范围；feature 分支全部合入 `master`。
2. 跑完整门禁：

```bash
cargo test --workspace --locked
./scripts/e2e_all.sh   # 如存在；否则按 RETEST_CHECKLIST 的 e2e 列表
DURATION=2m ./scripts/soak.sh
# 大型改动/渲染/终端核心：15m；发布候选：30m
```

3. 更新 `CHANGELOG.md`，把 `Unreleased` 收到版本号下。
4. 更新 `docs/RETEST_CHECKLIST.md` 的预期版本与已知限制。
5. **文档对账**（ACT-037）：

```bash
python3 scripts/matrix_stats.py            # 矩阵现算
python3 scripts/check_docs_counts.py       # CLOSURE 数字必须与矩阵一致，否则退出 1
```

不一致时先手工改 `CLOSURE.md` §2/§3 与 `docs/internal/ACTION_ITEMS.md` 的状态，或对 §2 用
`--fix`；**不要**只改一处。上线前这两条必须 PASS。

### 4.2 升版本

```bash
# 修改 Cargo.toml workspace.package.version
cargo build --workspace        # 让 Cargo.lock 更新
cargo test --workspace --locked
git add Cargo.toml Cargo.lock CHANGELOG.md docs/
git commit -m "chore(release): v0.6.0"
```

### 4.3 打 tag

```bash
git tag -a v0.6.0 -m "xtervo 0.6.0"
git show v0.6.0 --stat
git describe --tags --exact-match   # 必须输出 v0.6.0
```

### 4.4 从 tag 构建

```bash
git checkout v0.6.0
git status --short                  # 必须为空
./scripts/package.sh
./dist/xtervo-stripped --version     # 版本/commit/built 必须匹配 tag
sha256sum -c dist/SHA256SUMS
```

- 发布产物必须来自 **clean worktree + exact tag**。
- 不要在 tag 后改源码再打包。
- 文档-only 的提交可以在 tag 后，但不能改变二进制行为。

### 4.5 发布

1. 先写 `docs/release-notes/<tag>.md`（例如 `docs/release-notes/v0.6.0.md`）：
   版本号、commit、主要变更 / Breaking、已知限制、反馈入口。推到 `main` 后再打 tag。
2. 打 tag 并推送后，`.github/workflows/release.yml` 会自动构建 `dist/` 并把
   tar.gz / .deb / `SHA256SUMS` 传到 GitHub Release，release body 取
   `body_path: docs/release-notes/<tag>.md`。
3. 如果 Actions 不可用，手工上传 `dist/` 三个产物 + `SHA256SUMS`，body 粘贴同一份
   release notes。
4. 保留上一版本产物，不要覆盖同名文件。

---

## 5. Hotfix

```bash
git checkout -b hotfix/v0.6.1 v0.6.0
# 只修 bug，不夹带 feature
cargo test --workspace --locked
# 升 PATCH 到 0.6.1，更新 CHANGELOG
git commit -m "chore(release): v0.6.1"
git tag -a v0.6.1 -m "xtervo 0.6.1"
./scripts/package.sh
```

- hotfix 从**出问题的 tag** 拉分支。
- 修完合回 master，再打 patch tag。
- 不回滚/移动旧 tag。

---

## 6. CHANGELOG 规则

使用 Keep a Changelog 风格：

```markdown
## [Unreleased]

### Added
### Changed
### Fixed
### Breaking

## [0.6.0] - 2026-12-01
```

- 每个用户可见变更都写；内部重构可不写。
- Breaking 单独标出。
- 版本条目按时间倒序。
- 不把“重新打包/文档错字”写成新版本。

---

## 7. 禁止事项

```text
□ 用日期当版本号/tag
□ git tag -f / 移动已发布 tag
□ 同一版本重新打包不同二进制
□ dirty worktree 打包发布
□ 把 feature 塞进 PATCH
□ tag 在 A commit，产物从 B commit 构建
□ 没有 CHANGELOG 就发布
□ 发布后改源码再覆盖 dist
```

---

## 8. 快速检查表

发布前：

- [ ] 版本号只改在 `Cargo.toml`
- [ ] `Cargo.lock` 已更新
- [ ] CHANGELOG 有对应条目
- [ ] workspace tests 全绿
- [ ] 2m/15m/30m 门禁按要求通过
- [ ] 复测清单版本/哈希已更新
- [ ] worktree clean

构建后：

- [ ] `dist/xtervo-stripped --version` 版本/commit 正确
- [ ] `SHA256SUMS` 可校验
- [ ] tar.gz/deb 文件名版本一致
- [ ] 没有 dirty 标记

打 tag 后：

- [ ] annotated tag
- [ ] `git describe --tags --exact-match` = 版本 tag
- [ ] tag 指向 release commit
- [ ] 已推送 tag
