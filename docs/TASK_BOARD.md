# Tervo 任务看板

> 最后更新：2026-11-06 19:25  
> 规则：每个任务开始/完成/阻塞都要更新本文件；记录 worktree、branch、commit、测试/截图、限制。  
> 当前 master：`a82ac52`（Settings Tab 持久化）；当前 release：`target/release/tervo` 19:07。

## 1. 状态图例

| 状态 | 含义 |
|---|---|
| `done` | 已合 master，测试/e2e 通过 |
| `running` | agent 正在 worktree 工作 |
| `paused` | 已停止，代码/上下文保留，等前置条件 |
| `queued` | 已排期，未开始 |
| `blocked` | 有明确阻塞，不能自动继续 |
| `dropped` | 按约束明确不做 |

## 2. P0：稳定性优先

| ID | 任务 | 状态 | worktree / branch | 交付/证据 | 门禁 |
|---|---|---|---|---|---|
| P0-01 | 设置 UI：Settings Tab、图标分类、下拉/分段、搜索跳转、legacy 回退 | done | `.worktrees/settings-ui` / `feat/settings-ui` | commit `561e71e`；合 master `4afb827`；`.logs/settings-ui/shots/` | app 277 tests；5 e2e |
| P0-02 | 设置 Tab 持久化：切终端只 suspend，不自动关闭 | done | master | commit `a82ac52`；`.logs/settings-persist/` | behavior e2e；手动截图 |
| P0-03 | 外部 `config.toml` 热更新（500ms、回环保护、失败保留） | done | `.worktrees/config-hotreload` / `feat/config-hotreload` | commit `d6e5676`；主仓补齐 `1228748`；`.logs/config-hot-reload/` | 276 tests；Xvfb :126 |
| P0-04 | find 外部点击自动收起 + 搜索条防穿透 | done | master | commit `69801cc`；`.logs/find-dismiss/` | app tests；手动 |
| P0-05 | Codex P0：PTY 输入/写队列背压、pull 无超时导致 UI 冻结 | running | `.worktrees/p0-hardening` / `feat/p0-hardening` | agent 运行中；等 commit | 慢 shell/10MB 粘贴/输出洪峰；UI 停顿 <50ms |
| P0-06 | Codex P0：SFTP 元数据 `wait_with_output()` 无超时阻塞 UI | running | 同上 | 同上 | 假 SFTP 挂起 5s/永不退出；10s 超时；窗口仍响应 |
| P0-07 | Codex P0 修复合并 + 全量回归 + P0 稳定 release | queued | master | 等 P0-05/06 | workspace tests + 5 e2e + smoke |
| P0-08 | Codex 审计报告归档 | done | `.logs/codex-architecture-review.md` | 19k 行原始输出；摘要见本文件 §6 | — |

## 3. P1：P0 稳定后再跑/合

| ID | 任务 | 状态 | worktree / branch | 说明 |
|---|---|---|---|---|
| P1-01 | 完整键盘协议：Kitty query/set/push/pop、alternate/event types、modifyOtherKeys、release/repeat | paused | `.worktrees/keyboard-protocol` / `feat/keyboard-protocol` | 已有未提交 `core/src/keyboard.rs`；等 P0 稳定 |
| P1-02 | 搜索匹配计数 + 鼠标可编辑搜索框 | paused | `.worktrees/search-ux` / `feat/search-ux` | 未开工/无改动 |
| P1-03 | URL hover 手型 + 本地文件路径 Ctrl+click | paused | `.worktrees/url-paths` / `feat/url-paths` | 未开工/无改动 |
| P1-04 | 每 Profile 字体/颜色/命令/登录 shell/默认尺寸/word chars | queued | 待建 | 依赖 P0 设置 UI merged |
| P1-05 | Codex P1：copy/search/save 500ms UI 阻塞 | queued | 待建 | 与 P0-05 同源，可并入 P0 hardening |
| P1-06 | Codex P1：渲染 damage 路径失效、每帧全屏重绘 | queued | 待建 | 先做双缓冲脏行复制，再评估持久纹理 |
| P1-07 | Codex P1：400ms CoreEvent 轮询 → wake 桥接 | queued | 待建 | 目标语义事件 <50ms，空闲 CPU 不退化 |
| P1-08 | Codex P1：Vault Argon2 KDF 移出 UI 线程 | queued | 待建 | 解锁期间 UI 不停顿 |
| P1-09 | Codex P1：`main.rs` 拆分 + `with_app` 重入不丢操作 | queued | 待建 | 大重构，分小步 |
| P1-10 | 右键菜单美化：字号加大、分类分组、图标/分隔线、窄窗不溢出 | done | `.worktrees/context-menu` / `feat/context-menu` | commit `feat(ui): 右键菜单字号/分类/美化`；app 281 tests；Xvfb :135 7 张截图 + bbox 断言；term_menu e2e 全 PASS；限制：短窗需滚动、无菜单内键盘导航 |

## 4. 周末大块任务

> 开始条件：P0 全绿 + P1 已合/明确暂停；每块独立 worktree，先设计/测试清单，再实现。  
> 详细目标/性能/安全见 `docs/PLUGIN_SYSTEM.md`、`docs/self-iteration.md`。

| ID | 大块 | 状态 | 范围 | 验收 |
|---|---|---|---|---|
| W-01 | Profiles + 会话恢复 | queued | 每 Profile 外观/命令/env/login shell/默认尺寸；Tab/分屏/cwd/滚动恢复 | 重启恢复；Profile CRUD；e2e |
| W-02 | SSH/SFTP/Vault 产品化 | queued | Jump host、端口转发、agent forwarding、known_hosts、重连、SFTP 队列/取消/权限 | 真实 SSH 测试；断网/超时；UI 不冻结 |
| W-03 | iTerm2 IIP 图片 | queued | OSC 1337 File base64 PNG/JPEG；与 Sixel 并存 | dsh/真实终端图片；滚动/resize |
| W-04 | 插件系统 V1 | queued | manifest/权限；Connection/Middleware/Observer/Command/Workspace/Integration；最小 UI 插槽；AOT | 4 个真实插件验证；崩溃/超时不拖终端 |
| W-05 | 打包/自动更新/崩溃报告 | queued | .deb/AppImage/install 脚本、版本迁移、原子更新 | 干净机安装；升级/回滚 |
| W-06 | Wayland 原生 spike | queued | text-input-v3 IME、剪贴板、DnD、resize、全局快捷键 | 只 spike/报告，不承诺合并 |
| W-07 | 24h 稳定性 soak | queued | 输出洪峰、Sixel 大图、emoji cache、多 Tab/多窗口内存 | 无崩溃/无持续内存增长；报告 |
| W-08 | 全量 e2e/回归矩阵 | queued | 现有 5 e2e + 新功能 e2e + 性能门禁 | 全部绿；写入 selfcheck |
| DOC-01 | 最终人工复测清单 | queued | `docs/RETEST_CHECKLIST.md`：IME/GPU/设置/右键/图片/剪贴板/SSH/SFTP/Profile/插件/打包 | 交付前填好步骤、预期、日志/截图、已知限制 |

## 4.1 自主执行规则（2026-11-06 起）

用户已授权：**周末任务不再中途确认**。

- P0 修复完成后自动进入 P1，再进入 W-01..W-08；
- 每项独立 worktree，tests + 对应 e2e 全绿才合；
- 失败/不可逆阻塞写看板并继续下一项，不等待用户确认；
- 只有需要真实凭据/真机/发布签名的外部依赖才标记 `blocked`；
- 最终统一输出 `docs/RETEST_CHECKLIST.md`，列出用户需要人工复测的项；
- 不把未验证代码合进稳定包；release 仍保留回滚点。

## 5. 明确不做

| 项 | 原因 |
|---|---|
| Bidi / shaping | alacritty core 不支持；改动架构大 |
| 非 UTF-8 编码 | 架构缺口，现代环境收益低 |
| rewrap-on-resize | alacritty 不支持 |
| OSC 133/633 shell integration | 按“alacritty 不支持就不做”暂缓 |
| Kitty graphics | Tabby 不支持；Sixel 已覆盖 dsh/GNOME |
| 完整 Tabby npm/Electron UI 插件兼容 | Rust/Slint 模型不同；仅能力子集 |
| 原生 dylib 插件 | ABI/安全/稳定性不可接受 |
| Accessibility 完整实现 | 排后面；不在本周末范围 |

## 6. Codex 审计摘要与修复映射

完整报告：`/home/yt/AIworkspace/rusttabbyterminal/.logs/codex-architecture-review.md`

| 优先级 | 问题 | 修复任务 |
|---|---|---|
| P0 | PTY 输入/写队列背压冻结 UI | P0-05 |
| P0 | SFTP 元数据同步等待网络 | P0-06 |
| P1 | copy/search/contents 500ms 阻塞 | P1-05 |
| P1 | 渲染每帧全屏、damage 失效 | P1-06 |
| P1 | 400ms CoreEvent 轮询 | P1-07 |
| P1 | Vault KDF UI 线程 | P1-08 |
| P1 | main.rs 巨型 + with_app 静默丢操作 | P1-09 |
| P2 | 热路径 alloc/空轮询开销 | 后续 |
| P2 | 启动同步 IO 无超时 | 后续 |
| P2 | Save Contents 同步写盘 | 后续 |

## 7. 更新记录

| 日期 | 变更 |
|---|---|
| 2026-11-06 19:15 | 建立看板；P0-01..04 完成；P0-05/06 running；P1 冻结；周末大块排期 |
