# 本轮夜班进度与续跑指引（2026-11-17）

目的：把这几天连续推进的状态、证据、待办和坑位写在一处，换 session / 换人也能接着做。
**以 `git log` 与 `docs/ACTION_ITEMS.md §8` 为准**，本文只是索引。

## 1. 验证基线（本轮实测）

```text
cargo test -p tervo-app --bin tervo     571 passed / 0 failed / 1 ignored
cargo test -p tervo-core --lib           144 passed（含 4 条 transport/pump_reader）
DURATION=2m soak（sandbox 需可写目录）   PASS ×4：.logs/soak/20261117-133652 / 135443(HEAD 8ed890f)
                                        / 此前两次；最新 HEAD(e431deb) PASS：RSS 355.6→392.6MiB，线程 112/FD 97 不变，fatal=0）
DURATION=15m soak                        PASS（RSS +3.15 MiB/min，R²=0.29，线程/FD 不变）
PTY→可见 10 MiB                          178.35 ms ≈ 56.1 MiB/s
渲染 P95（200x50）                       damage 路径 37–40 µs；full 1.66 ms
```

沙箱里跑 GUI/soak 需要可写目录：

```bash
export CARGO_HOME=$PWD/.cargo CARGO_NET_OFFLINE=true
DURATION=2m BIN=$PWD/target/release/tervo SOAK_NO_SHOTS=1 \
  XDG_RUNTIME_DIR=/tmp/soak-rt TERVO_INSTANCE_DIR=/tmp/tervo-inst ./scripts/soak.sh
```

## 2. 已交付（按卡片）

| 卡片 | 状态 | 关键提交/证据 |
|---|---|---|
| ACT-034 分屏首帧 | 观察项，**0.5.0 不可复现**（旧证据来自 0.4.0-rc.2） | `scripts/e2e_split_repaint.py`（像素判定；single 8238 / dual 8040·14276） |
| ACT-035 恢复矩阵 | 写失败/截断/`.bak` 回退已覆盖，缺真实 ENOSPC | `session::tests::write_failure_keeps_previous_snapshot_and_leaves_no_tmp`、`truncated_primary_recovers_from_previous_generation` |
| ACT-037 文档对账 | 完成 | `scripts/check_docs_counts.py` + `RELEASING.md §4.1` 第 5 步 |
| ACT-038 MCP | 完成（文档 + `split` 未知 pane 报错） | `docs/MCP.md`「请求格式与窗口路由」 |
| ACT-046 工作区壳 | 侧栏可用：config/骨架/拖宽/Profile 列表/设置开关/状态 pill/工具行/命令 | `.logs/sidebar_*.png`、`.logs/status_unclean.png` |
| ACT-047 Profile 热键 | 模型+分发+校验+冲突提示 | `profile::tests::profile_hotkey_*`、`commands::default_command_for_chord` |
| ACT-036 | 恢复提示 + **连续崩溃计数**（≥3 次状态区告警） | `session::tests::crash_streak_bumps_on_unclean_and_resets_on_clean`；伪造 `session.running` 可见 pill |

回退点：本地 tag `v0.6.0-dev.1`（侧栏壳 + Profile 热键）与
**`v0.6.0-dev.2`（当前 HEAD：命令默认键分发 + 侧栏快捷键 e2e + release 2m 门禁 PASS）**，均未发布、未推送。

## 3. 未完成（按建议顺序）

```text
1. ~~ACT-048 前置：把 commands.rs 的 default_shortcut 接进键盘分发~~ ✅（已在 PTY 写入前兜底）
2. ~~ACT-047 设置页 Profile 热键绑定输入~~ ✅（表单 Hotkey 行）
3. ~~ACT-047 Workspace 保存/打开/选择器~~ ✅ ｜ ~~断线重连 v1~~ ✅（仅 ssh）
4. ACT-046 剩余：独立顶部 Toolbar（需先修 IME 坐标链里的 TABBAR_H 常量）、
   Connection Manager（与 ACT-041 合并）、插件注入点
5. ACT-036 剩余：crash marker + safe mode + 崩溃循环熔断
6. ACT-052 transport seam：`PaneTransport` + `SocketPairTransport` + `SplitTransport` + `pump_reader` ✅
   **接线施工单见 `docs/TRANSPORT_WIRING.md`**（18:00 起按 §1→§5 机械执行）
   （`crates/core/src/transport.rs`，4 条单测）；剩 PaneHandle 接线、宿主权限/审计/限流，
   再上 SSH/Serial/Telnet 插件（ACT-039..045）
7. ACT-049 插件生态、ACT-050 终端小件、ACT-051 平台/性能（24h、Wayland、emoji）
```

## 3.1 safe mode（v1 已实现，配方留档）

现状：**v1 已落地**——连续 ≥3 次异常退出时，本次启动用
`PluginStore::suppressed_for_safe_mode()` 屏蔽所有插件并打 `[safe-mode]` 日志；
状态区 pill 仍在。剩下的只有 GPU/WASM 显式降级与“恢复正常启动”入口。

```text
1. main() 在 `let compact` 附近（compute unclean 的地方，约 main.rs `let unclean = ...`）
   把 streak 提前算出来并保存，避免后面 bump 两次：
     let streak = if unclean { session::bump_crash_streak() } else { session::reset_crash_streak(); 0 };
     let safe_mode = streak >= 3;
   同时删掉 build_window_state 之后那段重复的 bump。
2. 门禁点：`plugin_v3::PluginStore::load()`（main.rs 约 14757）之前
     let plugins = if safe_mode { PluginStore::empty() } else { PluginStore::load() };
   （若没有 empty()，就加一个只返回空 store 的构造函数；不要复用 load_from(不存在路径)，
     那会打一堆 io 错误日志。）
3. GPU/WASM：WASM 过滤器只由插件注册，插件不加载即自动关闭；GPU 降级可以先不做，
   等有真实崩溃栈证据再决定要不要强制 `TERVO_GPU_NATIVE=0`。
4. UI：状态区 pill 已有；safe mode 生效时文案改为
   “安全模式：本次未加载插件（连续 N 次异常退出）”。
5. 验收：伪造 `session.running` + `session.crashes=3` 启动，断言
   - 日志出现 `[safe-mode] 连续 3 次异常退出：本次不加载插件`
   - `list` 能正常出窗口、pane 可交互
   - 退出后 `session.crashes` 清零
```

## 4. 坑位（都踩过，别再踩）

- **日志只打首帧**：`[gpu-native] pane=…` 只在 `frames == 1` 或 `TERVO_GPU_NATIVE_DEBUG` 时输出，
  不能拿“没有新日志行”判断没重绘；要用像素（`scripts/e2e_split_repaint.py`）。
- **XWD 解码**：`xwd` 头 `byte_order=0` 是 LSBFirst，按大端读会把 R/G 换位、蓝通道拿到 alpha(=255)，
  整屏发蓝。必须按 `byte_order` 选 `'<I'`/`'>I'`，并尊重 `bits_per_pixel`。
- **后端回退会 exec 自身**：无头/GPU 初始化失败时同一 pid 会重跑一遍启动并再写一次
  启动头，任何“每次启动 +1”的计数都会被算两次；计数需带时间戳去重（`session.crashes`
  现在是 `<次数> <unix 秒>`，10 秒内重复 bump 视为同一次启动）。
- **沙箱里 `/run/user/1000` 只读**：soak/实例需要 `XDG_RUNTIME_DIR` + `TERVO_INSTANCE_DIR` 指到可写目录。
- **MCP 参数必须放 `args`**；平铺会被忽略（曾因此收到 `pane_id required`）。
- **窗口标题累积**：已修（`secondary.rs::base_window_title`），别再往 `WM_NAME` 拼 id。
- ~~commands 默认键不参与分发~~ 已补兜底分发；`view.sidebar` 已绑 `ctrl+shift+b`，
  Xvfb 里 XSetInputFocus + XTEST 实测能切侧栏（日志出现 `[ui] sidebar_visible=`）。
  注意：无 WM 的 Xvfb 里不设焦点则按键丢失。

## 5. 续跑前先确认的三个选择

1. 直接提交 `main`（当前做法，每卡一 commit）还是走 `AGENTS.md` 的 worktree + 审查合并。
2. `mcp_tabs` 是否保持 `true`（现为 true，用于 socket 验证）。
3. `restore_session_on_startup` 是否打开（现为 false，重启不恢复布局）。
