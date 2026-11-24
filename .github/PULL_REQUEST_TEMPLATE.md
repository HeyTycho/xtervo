# 变更说明

<!-- 一两段说明“为什么改”和“改完是什么行为”。 -->

## 关联 issue

<!-- Closes #123；没有 issue 时可写背景或复现链接。 -->

## 改动类型

- [ ] Bug 修复
- [ ] 新功能 / 行为变化
- [ ] 性能 / 内存 / 渲染
- [ ] 输入 / 快捷键 / IME
- [ ] 插件 / MCP / SSH / Vault
- [ ] 文档 / CI / 打包 / 重构（无行为变化）

## 失败测试与复现（Bug 修复必填）

<!-- 先写失败测试，贴出修复前的失败输出；无法自动化的，说明原因并给可复现步骤。 -->

```text
复现命令：
修复前失败输出：
修复后通过输出：
```

## 验证

- [ ] `cargo test -p xtervo-core`
- [ ] `cargo test -p xtervo-app --bin xtervo`
- [ ] `cargo fmt --all -- --check`
- [ ] 相关 e2e 已运行（列出脚本与证据路径）
- [ ] 内存门禁已按改动规模运行 2m / 15m / 30m（贴 `report.md` / `summary.json` 路径）

```text
e2e / soak 命令：
证据路径：
```

## 文档

- [ ] `python3 scripts/check_doc_links.py`
- [ ] `python3 scripts/check_docs_counts.py`
- [ ] 如改动了行为/配置/快捷键，已同步对应文档

## 检查清单

- [ ] 改动在独立 worktree/分支完成，未直接提交到 `main`
- [ ] 没有提交密钥、token、密码、完整日志、终端回滚或构建产物
- [ ] 提交信息说明了改动范围；PR 尽量小而聚焦
- [ ] 已阅读 [CONTRIBUTING.md](https://github.com/HeyTycho/xtervo/blob/main/CONTRIBUTING.md) 与 [AGENTS.md](https://github.com/HeyTycho/xtervo/blob/main/AGENTS.md)
