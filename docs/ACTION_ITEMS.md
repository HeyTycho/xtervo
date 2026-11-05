# 行动清单（ACTION_ITEMS）

> 生成日期：2026-11-05 ｜ 依据：[`FEATURE_MATRIX.md`](FEATURE_MATRIX.md)（228 条，20.2% 加权覆盖）、[`UI_INTERACTION_MATRIX.md`](UI_INTERACTION_MATRIX.md)、[`TABBY_UPSTREAM_AUDIT.md`](TABBY_UPSTREAM_AUDIT.md)、[`COMPATIBILITY_MATRIX.md`](COMPATIBILITY_MATRIX.md)。
> 排序原则：需求 D 部分的实施顺序（UI 壳 → 工作区 → 终端 → 连接 → 设置/插件/平台），同层按“用户每天可感知 + 是否阻塞后续”排序。预估：S ≤ 0.5 天，M 1–3 天，L > 3 天（单人熟手）。
> **P0 前 10 项以 UI 壳/工作区交互为主**（仅 ACT-002 是为解除 UI 行为待核而插入的上游审计）；连接、插件、安全排在后面。每项验收标准均可执行/可观察，不以“代码写完”作为完成。

## 0. 先做验收，不重复造轮子（已有实现只需补测）

以下功能代码已存在且有部分记录，**不要重写**；先按验收步骤补证据，缺口用最小补丁修（标“需小修”的才动代码）。

| # | 已有实现 | 现状 | 验收方式 | 对应行动 |
|---|---|---|---|---|
| A1 | 无边框窗口、品牌位、TabBar、─/□/❐/✕、拖空白区、双击最大化、8 向边缘缩放 | 已实现，人工截图记录 | 启动→拖窗→双击→最小化→任务栏恢复→最大化→拖四边/四角；留 6 张截图 | ACT-001 |
| A2 | Tab 新建（+/Ctrl+T）、切换、关闭（×/Ctrl+W） | 已实现；关闭生命周期待 ACT-009 | 开 3 Tab→切换→逐个关→关最后一个退出；检查子进程消失 | ACT-001 |
| A3 | 嵌套分屏、分隔线拖动（10%–90%）、Close Pane 回填、焦点转移 | 已实现，有单测/截图 | 右键 Split Right→Split Down→拖动到极限→关闭中间 pane→再分 4 pane | ACT-001 |
| A4 | 鼠标上报 1000/1002/1003/1005/1006、Shift 绕过、滚轮 64/65 | 已有 16 例单测 + XTest；1002 右键拖动编码需小修（ACT-013） | `cargo test -p rusttabby-core --test mouse_report`；vim `:set mouse=a` 点击/滚轮/Shift 点击 | ACT-001、ACT-013 |
| A5 | 拖选/跨行/反向选择/自动复制、Ctrl+Shift+C/V | 有 smoke 测试 + 人工记录 | 反向拖到行首复制；多行复制；粘贴到 `cat > /tmp/x` 比对 | ACT-001 |
| A6 | 搜索（增量/高亮/Enter/Shift+Enter/Esc） | 有 smoke 测试 | Ctrl+F 搜 `200`→Enter/Shift+Enter→Esc；检查高亮清除 | ACT-001、ACT-013 |
| A7 | 滚轮回滚、scrollback 配置、alternate screen | 有单测 + phase0 vim/htop 记录 | 普通 shell 滚历史；`vim` 滚轮翻页；改回滚后新 pane 生效 | ACT-001 |
| A8 | OSC 0/2 标题（仅活动 Tab 首 pane） | 有事件测试；后台/非首 pane 不更新，需小修（ACT-005） | 前台/后台 tab 各发 `printf '\033]0;X\007'`，记录差异（后台不更新是已知缺口） | ACT-001、ACT-005 |
| A9 | 12 套内置主题、6 格式导入、Auto 跟随系统、UI chrome 同步 | 有 12 例测试 + 截图 | 切 12 套；放 iTerm2/WT/kitty/Xresources/YAML 各一；GNOME 切深浅色 | ACT-001 |
| A10 | 字号 ±、回滚 ±、即时保存 | 已实现 | 改字号看网格重排；改回滚后新 pane 验证；重启后配置保留 | ACT-001 |
| A11 | 会话快照 + Restart Session（布局/ratio/cwd） | 有 5 例测试 + 真机记录；进程/Session 解耦待 ACT-008 | 分屏+`cd /usr`→Restart→新实例 tab/pane/cwd 一致；快照文件被消费 | ACT-001、ACT-008 |
| A12 | CJK 宽字符/中文回退、X11 XIM 中文输入 | 有 glyph 测试 + IME 脚本 | `你好世界` 显示；fcitx/ibus 输入中文；候选窗跟随光标 | ACT-001 |
| A13 | 配置 schema v1、迁移前备份、未知字段保留、原子写（b80e265） | 有 7 例测试 | 写无 version 的旧配置→启动→检查 `.v0.bak` 与 `version = 1`；坏 TOML 不被覆盖 | ACT-001、ACT-026 |

## 1. P0：UI 壳 + 工作区交互（前 10 项，需求 D1/D2）

| ID | 标题 | 为什么现在做 | 涉及文件 | 验收标准（可测） | 预估 |
|---|---|---|---|---|---|
| ACT-001 | UI 壳/交互验收补测（不写新功能） | D 顺序第一；已有实现缺统一、可重复的验收证据，先锁基线防止后续改动回归 | `docs/UI_INTERACTION_MATRIX.md`、`docs/FEATURE_MATRIX.md`、`scripts/selfcheck.sh`、`.logs/ui-audit/` | 按 UI 矩阵 §9 完成 6 组验收；每项留截图/录屏；A1–A13 全部有结果；至少补 3 个自动断言（焦点/标签/分屏） | M |
| ACT-002 | 完成上游 Tabby 审计（clone + SHA + 模块逐项） | 21 条 `needs_verification` 与 30+ 条待核语义阻塞 UI 决策；不审计只能猜 | `docs/TABBY_UPSTREAM_AUDIT.md`、`docs/FEATURE_MATRIX.md`、`docs/COMPATIBILITY_MATRIX.md` | 记录上游 SHA/日期；21 条全部有文件级证据并收敛状态；14 个代表插件至少确认仓库/维护状态 | M |
| ACT-003 ✅（2026-11-05 完成） | Pane 焦点可视状态（接入 `PaneView.focused`） | 当前 `focused` 是死字段，4 pane 下无法判断输入归属；小改动直接提升可用性 | `crates/app/ui/main.slint`、`crates/app/src/main.rs`（`refresh_views`/`PaneView`） | 4 pane 下焦点 pane 有 1px accent 边框/标题条；点击/切 Tab/分屏后立即变化；截图对比；人工步骤入矩阵 | S |
| ACT-004 ✅（2026-11-05 完成） | Tab per-tab 右键菜单 + Close Others/Duplicate | 当前只有 TabBar 空白菜单，无法对单个 Tab 操作；B2-006/010 缺口 | `main.slint`、`main.rs`、`workspace.rs` | Tab 右键出现菜单；Close/Close Others/Duplicate 可用；仅 1 Tab 时 Close Others 禁用；Esc/点外关闭；人工+单测 | M |
| ACT-005 ✅（2026-11-05 完成：导航 + F2/双击重命名） | Tab 键盘导航 + 重命名 | 日常高频；B2-007/011；重命名触发方式依赖 ACT-002 结论 | `main.rs`（`handle_input`）、`main.slint`、`workspace.rs`、`session.rs` | Ctrl+Tab/Shift+Ctrl+Tab/Ctrl+1..9/最近 Tab 可用；F2 或双击进入内联编辑，Enter 保存/Esc 取消；标题进快照 | M |
| ACT-006 ✅（2026-11-05 完成：拖拽排序；拖出窗口未做） | Tab 拖拽排序 + 拖出窗口策略 | B2-004 完全缺失；用户整理工作区只能重开 | `main.slint`、`main.rs`、`workspace.rs`、`session.rs` | 3 Tab 拖动后顺序变化并持久化；拖出按 ACT-002 结论执行（新窗口或明确 no-op 提示）；单测覆盖重排/越界 | M |
| ACT-007 ◐（全屏 + 设置开关 + TabBar 上下位已完成；DPI 跟随与 Window 设置页待做） | 全屏 + TabBar 位置 + DPI/scale + Window 设置 | B1-005/006/007、UI-SET-011 成组缺口；影响所有用户 | `main.slint`、`main.rs`、`config.rs` | F11 全屏/退出；TabBar 四向（若上游确认）切换并持久化；scale factor 变化后 cell/布局正确；设置页可改；人工+截图 | M |
| ACT-008 ◐（detach/reattach + 后台列表已通；独立 Registry 待做） | Session 独立重构（Registry 与 Pane/Tab 解耦） | 需求 A3 硬约束；关闭生命周期/后台保留/断线重连/自动恢复的前置 | `pane.rs`、`main.rs`、新 `session_registry.rs`、`workspace.rs` | 关 Tab 不杀 Session（可 re-attach）；Session 有稳定 ID；3 个 pane 关/开不丢 shell；单测覆盖 detach/attach/reap | L |
| ACT-009 ◐（确认已完成；生命周期策略待做） | 关闭生命周期策略 + 关闭确认 | B1-008/B2-005；现在 ✕/Ctrl+W 直接杀进程，误关代价高 | `main.rs`、session registry、`config.rs`、`main.slint` | 设置可选“仅关视图/结束会话/后台保留”；关闭窗口/最后 Tab 有确认；取消不杀进程；三策略各有测试 | M |
| ACT-010 | Command Palette UI + 命令分发接线（命令表已完成） | B4-005/C9-001/005；`commands.rs` 已有 21 命令/快捷键解析/冲突检测/模糊搜索（8327e20），**不要重写**；缺 UI 与真实分发 | `commands.rs`、`main.rs`、`main.slint`、`config.rs` | Ctrl+Shift+P 打开面板，搜索/键盘选择/执行；main.rs 的 Ctrl+T/W/F/C/V 改走命令 ID；禁用态/上下文；命令绑定持久化；人工+单测 | L |
| ACT-011 ◐（分类 + Config File 已做；搜索与保存提示待做） | Settings 框架：导航/搜索/分类/Config File | B7 引言+14 项；单卡片无法承载后续设置 | `main.slint`、`main.rs`、`config.rs` | 左侧分类（Application/Appearance/Terminal/Color Scheme/Hotkeys/Window/Config File…）；搜索过滤；Config File 显示路径/打开目录，不显示 secret；人工 | L |
| ACT-012 | Hotkeys 设置页（录制/冲突/multi-chord） | C9-002/003/007；命令系统完成后必须让用户改键 | `main.slint`、`main.rs`、`commands.rs`、`config.rs` | 录制单键/两段键；冲突提示；清除/恢复默认；持久化；人工+单测 | M |

## 2. P1：终端完整交互（需求 D4）

| ID | 标题 | 为什么现在做 | 涉及文件 | 验收标准（可测） | 预估 |
|---|---|---|---|---|---|
| ACT-013 ◐（菜单 8 项已补；搜索大小写/正则/计数、缩放菜单待做） | 终端菜单/搜索/选择/缩放补齐 | B6-003/004/010/012；当前 5 项菜单、搜索无大小写/计数、无缩放 | `main.slint`、`main.rs`、`pane.rs` | 菜单含 Copy/Paste/Find/Clear/Select All/Split/Close；Ctrl+F 支持大小写/正则开关/计数；Ctrl+=/-/0 缩放并显示；人工 | M |
| ACT-014 ◐（多行确认 + Copy on select 已完成；右键粘贴开关、bracketed 端到端待做） | 粘贴安全与可配置 | B6-005/006/007/008；多行直接执行有风险 | `main.rs`、`main.slint`、`config.rs` | 多行粘贴确认（可记住）；copy-on-select 开关；右键粘贴开关；bracketed paste 端到端测试 | M |
| ACT-015 | 超链接 UI + Linkifier 接口（core 识别已完成） | TERM-012/B6-011；core 已有 `link.rs` 识别与 OSC 8 透出（94baefb/99d7895），只缺 hover/Ctrl+点击/插件接口，不应重写识别器 | `core/src/frame.rs`/`pane.rs`（hyperlink 元数据）、`main.rs`、新 `link.rs` | OSC 8/URL/IP/路径识别；Ctrl+点击打开；插件 linkifier 接口；路径存在性/远端提示；单测+人工 | L |
| ACT-016 | 字体/主题完善（family/weight/line-height/ligatures/emoji/透明度回退） | C10/THEME-005/006、TERM-007/008；当前固定字体 | `glyph.rs`、`main.rs`、`config.rs`、`themes.rs` | 可选字体族/字重/行距/字距；连字开关；Nerd fallback；emoji 有/无提示；透明不支持时回退；截图 | L |
| ACT-017 | 进度/通知/后台 Tab 标记 | C11；Bell/Exited 已有事件但用户不可见 | `pane.rs`、`main.rs`、`main.slint`、新 `notify.rs` | OSC 进度/命令完成检测（先 ACT-002 定协议）；Tab 进度/完成/失败标记；桌面通知权限失败提示；静音；人工 | L |

## 3. P2：工作区/Profile 管理（需求 D2/D5）

| ID | 标题 | 为什么现在做 | 涉及文件 | 验收标准（可测） | 预估 |
|---|---|---|---|---|---|
| ACT-018 | Sidebar/Toolbar/Connection Manager/Profile Selector 壳 | B4/B5 入口；UI 壳先于 SSH | `main.slint`、`main.rs`、新 UI 模型 | Sidebar 显隐/宽度；Toolbar 按钮（真实动作或 disabled+原因）；Connection Manager 入口；Profile Selector 新 Tab 下拉；人工 | L |
| ACT-019 ◐（Local Profile 模型/存储/面板已通；编辑表单与类型扩展待做） | Profile 模型与存储 | B5/C8；SSH/Serial/Telnet 前置 | 新 `profile.rs`、`config.rs`、`session.rs` | Profile schema（type/name/params/group/hotkey/default）；增删改查/复制/分组/排序；迁移/校验；单测 | M |
| ACT-020 | Profile 编辑 UI（动态表单） | B5-010..013；没有 UI 的 Profile 不可用 | `main.slint`、`main.rs`、`profile.rs` | 按类型显示字段；Local shell cwd/env/args/title；SSH General/Ports/Advanced/Ciphers/Login Scripts；Serial 参数；保存/取消/错误提示；人工 | L |
| ACT-021 | 多窗口 + Tab 跨窗口移动 | B1-009/B2-004；单窗口限制工作流 | `main.rs`、`main.slint`、`workspace.rs` | New Window；Tab 拖出成新窗口；窗口间移动；关闭一个窗口不影响另一个；人工+单测 | L |

## 4. P3：连接后端（需求 D6/D7）

| ID | 标题 | 为什么现在做 | 涉及文件 | 验收标准（可测） | 预估 |
|---|---|---|---|---|---|
| ACT-022 | SSH 垂直切片（连接/认证/known_hosts） | C3 核心；安全认证/known_hosts 必须原生 | 新 `ssh.rs`、`pane.rs` transport、`config.rs` | 连接/断开/超时；密码/密钥/agent；known_hosts 首次确认/变更告警；错误可见；本地 sshd 集成测试 | L |
| ACT-023 | SSH 进阶：jump/proxy/forward/X11/login scripts/config import | C3-006..014 | `ssh.rs`、`profile.rs`、`main.slint` | ProxyJump 两跳；local/remote/SOCKS 转发；agent/X11 状态；login scripts；ssh_config 映射；集成测试 | L |
| ACT-024 | SFTP + Port Forwarding UI | C4/B8 | 新 `sftp.rs`、`main.slint`、`main.rs` | 目录导航/上传下载/进度/取消；转发规则增删启停；危险操作确认；集成测试 | L |
| ACT-025 | Serial/Telnet/Zmodem | C6/C7/C5 | 新 `serial.rs`/`telnet.rs`/`zmodem.rs`、`profile.rs` | 串口枚举/参数/输入输出模式/重连；Telnet IAC；Zmodem 识别/进度/取消；硬件/本地回环测试 | L |

## 5. P4：安全/插件/运维（需求 D8–D10）

| ID | 标题 | 为什么现在做 | 涉及文件 | 验收标准（可测） | 预估 |
|---|---|---|---|---|---|
| ACT-026 | Config 导入/导出 + Tabby 配置迁移 + CLI/URI | C8-008/009、C14-004/006；迁移报告是交付项 | `config.rs`、新 `migrate_tabby.rs`、`main.rs` | 导入 Tabby `config.yaml`（profiles/hotkeys/theme 可映射）；不兼容字段报告；导出/备份/恢复；`--profile`/URI 启动；测试夹具 | L |
| ACT-027 | Vault/Keyring + secret 脱敏 | C12；SSH 前必须定凭据边界 | 新 `vault.rs`、`config.rs`、`ssh.rs` | Secret Service 存储/锁定；配置/日志脱敏测试（注入假密码不出现）；插件无权限读；文件权限 0600 | L |
| ACT-028 | 插件 Host API（WIT/权限/生命周期/扩展点） | C13-001..006；先定 API 再引 runtime | 新 `crates/plugin`、`docs/plugin-api.md` | 版本化 API；6 类最小扩展点；权限声明/拒绝；WIT 定义；宿主崩溃隔离；契约测试 | L |
| ACT-029 | 插件管理器 UI + 包扫描/签名/权限 | B11/C13-007/008 | `main.slint`、`main.rs`、新 `plugin_manager.rs` | 扫描 keyword/入口/版本；列表/安装/卸载/启停；签名/权限展示；Open Directory；崩溃诊断；人工+夹具 | L |
| ACT-030 | 旧插件 Node sidecar + shim + 14 插件 fixtures | C13-009..013；兼容性报告必须按插件名 | 新 `plugin-sidecar/`、`fixtures/`、`docs/COMPATIBILITY_MATRIX.md` | sidecar 启停/超时/崩溃隔离；tabby-core/terminal/local 最小 shim；14 插件逐项等级+fixture；无 UI 插件不启 Node/WebView | L |
| ACT-031 | 诊断/结构化日志/更新检查 | C14；出问题无法定位 | `main.rs`、新 `diagnostics.rs`、`scripts/` | 模块/session 关联日志；DNS/TCP/SSH 阶段；插件诊断；脱敏单测；版本/更新检查失败提示 | M |
| ACT-032 | 性能/回归/打包验收 | A5/D2；4 pane RSS/CPU/60fps/高吞吐未验收 | `scripts/`、`docs/phase0-report.md`、`docs/selfcheck-baseline.json` | 4 pane RSS≤220MiB、空闲 CPU≤1%、P95≤50ms、60fps、50MiB/s 有界；CI 门禁；数据写文档 | M |
| ACT-033 | 文档/矩阵持续维护 + 验收截图库 | D2 交付项；避免再次出现“文档与代码不符” | 5 份 docs、`.logs/ui-audit/` | 每次功能变更同步矩阵；`implemented` 有测试/截图；上游证据有 SHA/路径；CI 校验矩阵计数 | S |

## 6. 依赖关系（先做什么，避免返工）

```
ACT-001 验收基线
  ├─ ACT-003 焦点可视
  ├─ ACT-004 Tab 菜单 ── ACT-010 命令注册表 ── ACT-012 Hotkeys
  ├─ ACT-005 键盘导航/重命名 ── ACT-002 上游审计（触发方式）
  ├─ ACT-006 拖拽 ── ACT-021 多窗口
  ├─ ACT-007 全屏/位置/DPI
  └─ ACT-008 Session Registry ── ACT-009 关闭生命周期
                                └─ ACT-022 SSH / ACT-025 Serial（传输层）
ACT-010 命令注册表 ── ACT-011 设置框架 ── ACT-018 Sidebar/Profile Selector
ACT-019 Profile 模型 ── ACT-020 Profile UI ── ACT-022 SSH / ACT-024 SFTP/Serial
ACT-027 插件 Host API ── ACT-028 管理器 ── ACT-029 旧插件桥
ACT-002 上游审计 ──（解除 21 条 needs_verification 的决策阻塞）
```

**关键前置**：
- 没有 ACT-010，不要做 ACT-004/005/012/028（菜单/快捷键会各写一套）。
- 没有 ACT-008，不要做 ACT-009/022/025 的“保留后台 Session/断线重连”。
- 没有 ACT-019，不要做 ACT-020/022/024（字段会反复改）。
- 没有 ACT-028，不要做 ACT-029（旧插件桥会绑定不稳定 API）。
- 没有 ACT-002，ACT-005/006/007/013/017/030 的具体行为只能猜；先按“明确 no-op + 待核提示”落地，不伪装支持。

## 7. 明确不要现在做（避免 AI 式铺开）

1. 不要先写 SSH/SFTP/Serial 后端再接 UI——UI 壳与 Profile 模型未就绪，接口必返工（需求 D 明确禁止）。
2. 不要为了“看起来完整”加假按钮：未实现的功能必须 `disabled` 并显示原因（需求 A3）。
3. 不要在没有命令注册表时给每个菜单项写独立回调。
4. 不要在没有 Session 抽象时做“后台保留/重连/自动恢复”。
5. 不要承诺“兼容 Tabby 插件”；先按 `COMPATIBILITY_MATRIX.md` 的 L0–L4 分级逐项验证。
6. 不要把需求文档当上游证据；所有 `U-AUDIT` 条目等 ACT-002 收敛后再改状态。
