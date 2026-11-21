# Tervo Agent Working Rules

这些规则适用于所有在本仓库工作的 agent / Codex / 子 agent。

## 工作时间

- **大任务只在 18:00（本地时间）之后开始。**
- 白天（18:00 前）不做大功能开发、不做大规模重构、不启动多 agent 功能扩张。

### 白天允许

```text
小 bugfix（< 30 分钟、单文件/小范围）
测试、e2e、复测
文档、发布说明、看板更新
调研、方案、兼容性矩阵
内存/性能测量
代码审查
```

### 晚上 18:00 后

```text
新功能
插件系统 / AI / MCP
大规模 UI/架构改动
版本发布
长时 soak
```

如果用户在白天要求大任务：

1. 先做调研/方案/任务拆分；
2. 明确记录“18:00 后执行”；
3. 不在白天直接开工，除非用户明确说“现在开始/覆盖这条规则”。

## 大任务定义

满足任一即视为大任务：

- 预计 > 30–60 分钟
- 跨多个 crate / 改终端核心 / 改 UI 架构
- 新功能分支
- 插件、AI、MCP、WASM、市场
- 版本号/tag/发布包
- 15m/30m 长时 soak

## 每个大功能必须做内存门禁

- 2m soak 必跑
- 触及终端/渲染/循环/插件宿主：15m
- 发布候选：30m
- 记录 RSS/PSS、线程/FD、live heap
- 不得改 soak 阈值或用 `malloc_trim` 替代生命周期修复

## 开发流程

```text
独立 worktree
→ cargo check
→ tests + 对应 e2e
→ 2m/15m/30m 内存门禁（按规模）
→ commit（worktree）
→ 主进程审查后合 master
→ 打包/tag 按 docs/RELEASING.md
```

## 排障纪律（强制）

完整复盘见 [`docs/POSTMORTEM_2026-11-11_RENDER_RESIDUE.md`](docs/archive/POSTMORTEM_2026-11-11_RENDER_RESIDUE.md)。

```text
1. 先复现，保留 raw / Frame dump / 截图，不让用户反复重启。
2. 按层找第一处分歧：
   bytes -> filter -> Term -> Frame -> framebuffer -> Slint/窗口。
3. 没有失败测试，不写修复。
4. 禁止用默认整屏重绘、每帧新 buffer 掩盖根因；只能作为显式 debug 开关。
5. 图片/过滤层改动必须同时有逐字节或逐像素差分测试。
6. 连续两个修复不改变症状，停止猜，转插桩/二分层。
7. 多 agent 并行必须按层分工，各自交失败测试、复现命令、证据路径，
   由一个 integrator 统一合并。
```

调试开关：

```bash
TERVO_LINE_NUMBERS=1            # 终端行号 overlay（会盖住最左约 3 字符）
TERVO_FRAME_DUMP=/tmp/frame.txt # 每帧 Frame 文本落盘
TERVO_SIXEL_DEBUG=1             # 图片 placement 日志
```

## 输入 / IME 专项（2026-11-21 事故后强制）

完整复盘见 [`docs/POSTMORTEM_2026-11-21_INPUT_IME.md`](docs/POSTMORTEM_2026-11-21_INPUT_IME.md)。

```text
1. 输入法组合态只能由 winit Preedit/Commit/Disabled 权威维护；
   禁止单独用 TextInput.preedit-text 作为永久吞键条件。
   任何“组合中”锁必须有超时自愈；Ctrl/Alt/Meta 永不吞。
2. 没有真实 Wayland + fcitx5/RIME 的证据，不得说“输入法修好了”。
   X11/Xvfb/Xephyr 的 XIM 不产生 text-input preedit，不能作为 IME 验收。
3. 键盘/IME 改动必须先有失败测试：
   stale preedit 不吞键、Num/Caps Lock 不触发转义、快捷键与平台默认逐项对齐。
4. 先确认用户实际运行的二进制 commit（--version / 进程启动时间），
   不在旧进程上排查。
5. 快捷键默认值以本机平台为准，先读：
   gsettings list-recursively org.gnome.Terminal.Legacy.Keybindings:/org/gnome/terminal/legacy/keybindings/
   不抢 shell 自己的键（Ctrl+W/T/F 等）。
6. 一次只改一层；连续两个修复不改变症状，停止猜，转插桩/二分。
```

## 当前状态

```text
2026-11-09 起：大任务 18:00 后
当前待办：Tabby 插件兼容（API v2 / AI panel / MCP）
执行时间：18:00 后
```
