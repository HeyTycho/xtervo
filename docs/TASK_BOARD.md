# Tervo 任务看板

> 最后更新：2026-11-09 14:35  
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
| P0-05 | Codex P0：PTY 输入/写队列背压、pull 无超时导致 UI 冻结 | done | `.worktrees/p0-hardening` / `feat/p0-hardening` | commit `7b6b7ad`；合 master `f3e9e0e`；`.logs/p0-blocking/` | 474 workspace tests；慢 shell/10MiB 粘贴/20MiB 洪峰；UI 调用 <1ms |
| P0-06 | Codex P0：SFTP 元数据 `wait_with_output()` 无超时阻塞 UI | done | 同上 | `SftpOpJob` 后台化；10s kill 进程组；`.logs/p0-blocking/` | 假 sftp sleep 30：List 10.0s 超时；TabBar/New Tab 仍响应 |
| P0-07 | Codex P0 修复合并 + 全量回归 + P0 稳定 release | done | master `f3e9e0e` | release `target/release/tervo` 19:51 / `tervo-stripped-new` 50M | 474 tests；6 e2e 全 PASS（WM resize 首次偶发、重跑 PASS）；smoke rc=0 |
| P0-08 | Codex 审计报告归档 | done | `.logs/codex-architecture-review.md` | 19k 行原始输出；摘要见本文件 §6 | — |

## 3. P1：P0 稳定后再跑/合

| ID | 任务 | 状态 | worktree / branch | 说明 |
|---|---|---|---|---|
| P1-01 | 完整键盘协议：Kitty query/set/push/pop、alternate/event types、modifyOtherKeys、release/repeat | done | `.worktrees/keyboard-protocol` / `feat/keyboard-protocol` | commit `4805793`；合 master `1a55f09`；514 tests；Xvfb :123 + 真实 dsh kitty flags=1；`.logs/keyboard-protocol/` | 普通 shell/off 字节不变；`.logs/dsh-kbd-final2/` |
| P1-02 | 搜索匹配计数 + 鼠标可编辑搜索框 | done | `.worktrees/search-ux` / `feat/search-ux` | commit `2ac8943`；合 master；`.logs/search-ux/` | app 282；search e2e PASS；term_menu PASS |
| P1-03 | URL hover 手型 + 本地文件路径 Ctrl+click | done | `.worktrees/url-paths` / `feat/url-paths` | commit `ad63082`；合 master；app 284/core 94+63；Xvfb :128 PASS；`.logs/url-paths/` | URL/OSC8/路径/hover/失败提示全 PASS |
| P1-04 | 每 Profile 字体/颜色/命令/登录 shell/默认尺寸/word chars | done | `.worktrees/profile-session` / `feat/profile-session` | commit `51b6f2f`；合 master `1d14b20`；font_size/theme/cursor/blink/scrollback + session profile_id/自动恢复；app 326；Xvfb :136 e2e PASS；`.logs/profile-session/` | 剩余 login shell/default size/word chars 记录为后续增强 |
| P1-05 | Codex P1：copy/search/save 500ms UI 阻塞 | queued | 待建 | 与 P0-05 同源，可并入 P0 hardening |
| P1-06 | Codex P1：渲染 damage 路径失效、每帧全屏重绘 | done | `.worktrees/render-damage` / `feat/render-damage` | commit `8146aa7`；合 master `b8effd0`；单行 45.6×、连续按键 46.4×；Xvfb :137 e2e PASS；`.logs/render-damage/` | 逐像素等价；full/滚动 1.00× |
| P1-07 | Codex P1：400ms CoreEvent 轮询 → wake 桥接 | done | `.worktrees/event-wake` / `feat/event-wake` | commit `16b5135`；合 master；app 300/core 518 tests；Xvfb :138 e2e PASS：标题 0–1ms/Bell 1ms/OSC52 1ms/插件 3ms/SFTP 28ms；空闲 CPU 不退化；`.logs/event-wake/` | 400ms 兜底保留 |
| P1-08 | Codex P1：Vault Argon2 KDF 移出 UI 线程 | done | `.worktrees/ssh-sftp` / `feat/ssh-sftp` | 后台 `VaultTask` + `WakeSignal`：initialize/unlock/change_master 全部不占 UI 线程；busy 状态可重绘、串行防重入、线程异常恢复；worker 主密码/AEAD 明文/`Unlocked` Drop 尽力清零；app 351 tests；Xvfb :139 busy/完成截图；解锁/改密码期间 UI 不停顿 |
| P1-09 | Codex P1：`main.rs` 拆分 + `with_app` 重入不丢操作 | partial | master | 技术债：main.rs 仍大；本轮未重构，避免影响稳定包；已记录 | 后续分段迁移 |
| P1-10 | 右键菜单美化：字号加大、分类分组、图标/分隔线、窄窗不溢出 | done | `.worktrees/context-menu` / `feat/context-menu` | commit `bddf0ef`；合 master `3e5c270`；`.logs/context-menu/` | context-menu e2e PASS；term_menu PASS；字号/分组/窄窗/右下翻转截图 |
| P1-11 | Tab 级 Pin + 拖拽 ghost/插入线/让位动画 | done | `.worktrees/tab-pin-drag` / `feat/tab-pin-drag` | commit `feat(tabs): pin + 拖拽 ghost/插入动画`（分支头；基于 master `562773e`）；`.logs/tab-pin-drag/e2e/shots/` | app 375 tests；Xvfb :158 `scripts/e2e_tab_pin_drag.sh` PASS：pin/unpin、重启恢复 pinned、ghost+2px 插入线+让位、跨组 blocked、Esc 取消；截图 12 张 |

## 4. 周末大块任务

> 开始条件：P0 全绿 + P1 已合/明确暂停；每块独立 worktree，先设计/测试清单，再实现。  
> 详细目标/性能/安全见 `docs/PLUGIN_SYSTEM.md`、`docs/self-iteration.md`。

| ID | 大块 | 状态 | 范围 | 验收 |
|---|---|---|---|---|
| W-01 | Profiles + 会话恢复 | done | `.worktrees/profile-session` / `feat/profile-session` | 每 Profile 外观（font_size/theme/cursor_shape/cursor_blink/scrollback，additive v2，空=继承全局，Profile 显式 > 全局）；Settings→Profiles 表单共用 Appearance 区；session Snapshot 增加可选 profile_id，local Profile 恢复外观/shell/cwd，SSH/Serial/Telnet 不自动建连；`restore_session_on_startup`（默认 false）正常退出写快照，启动恢复 Tab/split/ratio/cwd/网格，失败回退新 Tab。验收：app 295 / workspace 489 tests；Xvfb :136 `scripts/e2e_profile_session.sh` PASS（表单/应用外观/3 Tab+split+cwd 重启恢复/损坏快照 fallback；截图 `.logs/profile-session/e2e/shots`） |
| W-02 | SSH/SFTP/Vault 产品化 | done | `.worktrees/ssh-sftp` / `feat/ssh-sftp` | **SSH**：Profile 增加 `proxy_jump`/`remote_forwards`/`dynamic_forwards`/`agent_forward`（additive v2，旧 profiles.toml 兼容），表单可编辑，`ssh_command` 生成 `-J/-R/-D/-A`；`auth=password` 显式 kbd-interactive + askpass 识别本地化密码 prompt/排除 OTP；known_hosts 连接前后台预检（10s、可取消）：首次显示 SHA256 指纹、变更并列旧/新指纹，Trust/Update 才写 known_hosts（0600，`ssh-keygen -R` 先删旧条目），否则回退终端确认；SSH 退出状态横幅 + Reconnect（不自动循环重连）。**SFTP**：上传/下载队列（FIFO、Clear queue）、失败 Retry、chmod（八进制校验）、元数据 Cancel、传输 Cancel；`ConnectTimeout=10`+`ServerAliveInterval=5/CountMax=2`，元数据 10s 硬超时；全部后台不阻塞 UI。**Vault**：Argon2/初始化/改密码移出 UI 线程（P1-08）。验收：app 351、core smoke 25；`scripts/e2e_ssh_sftp_vault.sh` Xvfb :139 PASS（19 张截图：SSH 表单/known_hosts 首次+变更/退出横幅/重连/Vault busy+完成/SFTP 队列/取消/chmod/失败+retry）；真实 sshd 不可用，改用假 ssh/sftp/keyscan + 真实 ssh -G/ssh-keygen |
| W-03 | iTerm2 IIP 图片 | done | `.worktrees/iip-image` / `feat/iip-image` | commit `4968a55`；合 master `b23476a`；core 194/app 327；Xvfb :140 PNG/JPEG 截图；`.logs/iip/` | 跨 chunk/滚动/resize/关闭/热更新；无 Kitty |
| W-04 | 插件系统 V1 | done | `.worktrees/plugin-v1` / `feat/plugin-v1` | commit 为分支头；外部进程 manifest（id/runtime/permissions/contributes）+ 常驻 process JSON Lines（2s 握手/10s 命令超时/重启上限 2/坏 JSON 禁用）+ Command（静态 args/返回文本浮层）+ Status（2s 轮询/底部状态栏/可点击 command）+ Theme Pack（TOML，可选）；旧 oneshot/旧扁平清单兼容；设置页显示 running/restarting/disabled 原因；UI 不新增复杂面板。测试 341 passed（基线 327）；fake Python/Node 插件覆盖命令/状态/崩溃/超时/坏 JSON/禁用；Xvfb :142 `scripts/e2e_plugin_v1.sh` PASS，截图 `.logs/plugin-v1/{command_result,status,plugins}.png` | 命令/状态/主题闭环；崩溃/超时/坏 JSON/坏 manifest 不拖死 Tervo；无沙箱（权限仅声明）、无安装/UI 开关/热重载，限制见 docs/PLUGIN_API.md |
| W-05 | 打包/自动更新/崩溃报告 | done | `.worktrees/packaging` / `feat/packaging` | commit `2ac3ac6`；合 master `585fd21`；327 tests；packaging 71 项 PASS；`dist/` tar/deb/sha | tar/deb/install/rollback/deb smoke；自动更新/崩溃报告明确未做 |
| W-06 | Wayland 原生 spike | done | `.worktrees/wayland-spike` / `feat/wayland-spike` | commit `f2e16e4`；合 master `e8ef824`；`docs/WAYLAND_SPIKE.md` 576 行；纯 Wayland/Xwayland headless 实测；DnD/全局快捷键/PRIMARY 硬阻断 | 只 spike，不承诺原生完整 |
| W-07 | 长时稳定性 soak | done | `.worktrees/soak` / `feat/soak` | `scripts/soak.sh` + `docs/SOAK.md`；2m PASS；30m 修复后 RSS 753.2→817.2 MiB（+64.0MiB，+0.78MiB/min，R²=0.13），6min 后平台；`.worktrees/mem-converge/.logs/soak/fix-30m/` | 30m PASS；不声称 24h |
| W-08 | 全量 e2e/回归矩阵 | done | 15 个脚本（SSH/SFTP、插件、设置、Sixel/IIP、键盘、事件、渲染、P0、URL/右键等） | 功能 e2e 全 PASS；URL/WM 各一次注入竞态重跑 PASS；内存 30m 单独 PASS | 功能+内存门禁绿 |
| MEM-01 | 长时内存增长修复 | done | `.worktrees/mem-converge` / `feat/mem-converge` | 根因 IIP/Sixel 图片锚点 + scrollback 封顶；`66ae0c6` 合 master `0cb84e8`；30m +64.0MiB（旧 +209.6），6min 平台；app 369/core 127 | 30m PASS；诊断 commit 1bec94f 未合发布 |
| DOC-01 | 最终人工复测清单 | done | `docs/RETEST_CHECKLIST.md` | 已填版本/哈希/e2e/30m 内存结果，待用户逐项签字 | 文档交付 |
| DOC-02 | UI/视觉设计规范 | done | `docs/UI_DESIGN.md` | 美学、token、图标、菜单、动效、双语、2m/15m/30m 内存门禁、验收清单 | 设计与验收统一口径 |

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
| 2026-11-06 19:55 | P0-05/06/07 done（474 tests，6 e2e，release 19:51）；P1-01/02/03/10 恢复；WM resize 首次偶发、重跑 PASS 记录 |
| 2026-11-06 20:05 | P1-02 搜索 UX 合入（282 tests，search e2e PASS）；注意 e2e 必须重建 target/debug |
| 2026-11-06 20:20 | P1-10 右键菜单合入（3e5c270）；context-menu + term_menu e2e PASS；开始 W-01 |
| 2026-11-09 14:05 | P1-11 Tab Pin + 拖拽 ghost/插入线/让位动画 done（feat/tab-pin-drag，未合 master）；app 375 tests；Xvfb :158 e2e PASS；截图 `.logs/tab-pin-drag/e2e/shots/` |
| 2026-11-06 21:00 | P1-01 键盘协议合入（1a55f09，514 tests）；P1-06 damage/P1-07 event-wake 开跑 |
| 2026-11-06 21:40 | P1-07 事件唤醒完成并提交（16b5135，feat/event-wake，未合 master）：语义事件/插件/SFTP 结果即时唤醒，400ms 兜底保留；Xvfb :138 e2e PASS；待父任务合入 |
| 2026-11-06 21:45 | P1-06 渲染 damage 恢复（feat/render-damage）：双缓冲行同步 + 真实 damage；单行/按键 rows=1、full_redraws 不增，release 45.6×/46.4×，full/滚动 1.00×；app 310 passed / workspace 527；Xvfb :137 5 张截图与 profile 断言 PASS；commit `8146aa7` |
| 2026-11-06 22:10 | P1-04/W-01 Profile+会话恢复合入；app 326；P1-06/07 合入；e2e 日志断言修正；准备 P1 稳定 release |
| 2026-11-06 22:25 | P1 稳定 release 构建 22:08（242M/51M，app 326）；W-02/W-03/W-05 三个周末大块开跑 |
| 2026-11-06 22:40 | W-05 打包合入（585fd21，327 tests，packaging 71 PASS）；W-04 插件 V1 开跑 |
| 2026-11-06 23:15 | W-03 IIP 合入（b23476a，core 194/app 327）；W-02/W-04/W-06 继续 |
| 2026-11-06 23:50 | W-06 Wayland spike 报告合入（e8ef824）；W-02/W-04/W-07 继续 |
| 2026-11-07 00:35 | W-02/W-04/W-07 合入；SSH/SFTP 19 截图 e2e PASS，plugin e2e PASS；soak 15m FAIL 内存，MEM-01 开跑 |
| 2026-11-07 04:35 | 最终候选 8823311；15 个功能 e2e 全 PASS；30m soak 仍 +209.6MiB 内存未收敛；dist tar/deb 生成；复测清单完成 |
| 2026-11-09 01:40 | 真实 SSH 172.27.133.14 验收：Tervo SSH Profile known_hosts/登录/远程命令 PASS；修首次启动不读 default_profile_id；最终包 3fafbc8 |
| 2026-11-09 11:35 | feat/i18n：Settings/终端右键/TabBar 中英双语 + Appearance 顶部 Language（运行中切换、写回、热更新）；app 376 passed（基线 369）；`scripts/e2e_settings_i18n.sh` PASS；未合 master |
| 2026-11-09 02:00 | 设置页修复：Dim/Opacity 仅左键拖拽（悬停 0 像素变化）、内容区随窗口放大（1080→1482）；最终包 5dc3c31 |
| 2026-11-09 12:45 | 内存收敛：IIP/Sixel 图片按单调滚动淘汰（66ae0c6）；30m +64.0MiB PASS；消融 full/noimg 10m +87.4/+60.0；最终包 0cb84e8 |
| 2026-11-09 14:35 | 合入 i18n、Tab pin/拖拽、Settings 齿轮/右键去图标；新增 `docs/UI_DESIGN.md`；app 382 tests |
| 2026-11-07 00:23 | soak 脚本交付；2m PASS，15m FAIL 内存增长（+16.7MiB/min）；内存专项开跑 |
| 2026-11-06 23:55 | W-04 插件 V1 完成（feat/plugin-v1，未合 master）：常驻 process 生命周期/命令结果/状态栏/Theme Pack/权限声明；app 341 passed（基线 327）；Xvfb :142 e2e PASS（command_result/status/plugins 截图）；限制见 PLUGIN_API.md §11 |
| 2026-11-06 21:35 | W-01 完成（feat/profile-session，未合 master）：每 Profile 外观 + session profile_id/可选启动自动恢复；app 295、workspace 489；Xvfb :136 e2e PASS；限制见 docs/self-iteration.md |
| 2026-11-06 23:56 | W-02 + P1-08 完成（feat/ssh-sftp，未合 master）：SSH 跳板/远程+动态转发/agent forwarding/known_hosts 预检/重连；SFTP 队列/chmod/retry/cancel/10s 连接超时；Vault 后台 KDF。app 351、core smoke 25；Xvfb :139 e2e 19 张截图 PASS；限制见 docs/self-iteration.md |
