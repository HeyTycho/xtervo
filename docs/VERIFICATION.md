# 怎么验证 xtervo（验收手册）

> 面向“我要自己确认这些数字/功能是真的”的场景。所有命令都在仓库根目录执行。
> 约定：`export CARGO_HOME=$PWD/.cargo`（沙箱/CI 里 `~/.cargo` 只读时）。

## 1. 先复核数字（不需要编译）

```bash
python3 scripts/matrix_stats.py        # 矩阵口径：implemented/partial/not_started + 加权
python3 scripts/scope_tiers.py         # 按产品必要性分档（核心/P1/连接插件/不建议）
python3 scripts/check_docs_counts.py   # CLOSURE 与矩阵是否一致（发版前必跑）
python3 scripts/check_doc_links.py     # 文档相对链接
python3 scripts/gen_status_page.py     # 生成 .logs/status.html（路标/矩阵/计划/提交）
```

期望：

```text
matrix_stats:  weighted 61.4% / implemented 110 / 证据过短 0
scope_tiers:   P0 终端本体 97.5%（100 条）/ 连接插件 32.7%（可选）
check_docs_counts: PASS
check_doc_links:   checked: broken=0
```

逐条证据在 `docs/FEATURE_MATRIX.md` 的 **Test** 列（每条 implemented 都带可复跑命令或证据路径）。

## 2. 构建 + 测试门禁

```bash
cargo build --release -p xtervo-app
cargo test -p xtervo-core                 # 含 VT 兼容套件与 fuzz（约 1s 默认轮次）
cargo test -p xtervo-app --bin xtervo      # 期望 666 passed / 2 ignored
./scripts/selfcheck.sh                   # 构建 + 单测 + 启动预算
RUN_IME=1 ./scripts/selfcheck.sh         # 加中文冒烟
```

注意：`plugin_v3` 里有 1–2 个既有时序用例在机器高负载时会偶发失败（wasmtime epoch 超时），
单跑或复跑即过；判定前先看失败用例名，别把它算成本轮回归。

## 3. 内存门禁（2m/15m）

```bash
env DURATION=2m  BIN=$PWD/target/release/xtervo SOAK_NO_SHOTS=1 \
    XDG_RUNTIME_DIR=/tmp/soak-rt XTERVO_INSTANCE_DIR=/tmp/xtervo-inst ./scripts/soak.sh
env DURATION=15m BIN=$PWD/target/release/xtervo SOAK_NO_SHOTS=1 \
    XDG_RUNTIME_DIR=/tmp/soak-rt XTERVO_INSTANCE_DIR=/tmp/xtervo-inst ./scripts/soak.sh
```

判定看报告的 `verdict`、`RSS/PSS`、`threads/fds`、`fatal=0`：

```text
PASS 基线（参考）：RSS 355→392 MiB/2m（+45 MiB/min 预热段），threads 112 恒定，fd 97–98 恒定
红旗：线程/FD 持续增长、fatal>0、RSS 斜率 >150 MiB/min 且 15m 仍不收敛
```

报告落在 `.logs/soak/<时间戳>/report.md`（含 samples.csv）。

发布候选/严格门禁可在命令前加 `SOAK_STRICT=1`：线程增长 >32 或 FD 增长
>64 会进 `issues`（FAIL）。默认仍只记 `warnings`；这是为了不改既有
2m/15m PASS 阈值（AGENTS.md 的“不得改 soak 阈值”）。

## 4. 三个“环境卡点”怎么验（当前 97.5% 剩下的 3 条）

这三条**代码已就绪，但需要特定桌面/硬件环境**；下面给出拿到环境后的完整验证步骤。

### 4.1 TERM-016：Wayland IME / 锁键 / 平台键

**需要**：Wayland 合成器（GNOME/mutter、sway、weston 任一）+ fcitx5 与中文引擎
（或 ibus + libpinyin）。

```bash
# 0) 环境侦察（任一不满足就先装）
echo $XDG_SESSION_TYPE $WAYLAND_DISPLAY
ls /run/user/1000/wayland-*
command -v fcitx5 && fcitx5 --version

# 1) 启动输入法
fcitx5 -d
export GTK_IM_MODULE=fcitx QT_IM_MODULE=fcitx XMODIFIERS=@im=fcitx

# 2) 跑终端并开键盘日志
XTERVO_KEYLOG=1 SLINT_BACKEND=winit-software ./target/release/xtervo --shell /bin/cat

# 3) 点一下 pane，输入 nihao 再按空格
# 期望日志： [ime] "你好"        ← 提交文本
#          [ime] spot pane=… cell=r,c win=x,y …   ← 候选锚点跟着光标

# 4) 组合键：在终端里跑 cat -v，依次按 Shift+Tab / Ctrl+Shift+J / Super 组合
#    默认模式期望： ^[[Z（Shift+Tab）
#    开启 kitty 协议（printf '\x1b[>1u'）后： CSI 9;2u / CSI 106;6u 形式

# 5) 锁键（caps/num）
#    caps lock 开时，kitty 协议的 CSI-u 应带 caps 位（;64），num lock 带 num 位（;128）
#    对照 scripts/e2e_keyboard_protocol.sh 的注入用例
```

判定：候选窗贴住光标格（不是鼠标旧位置）、切 pane/Tab 后输入不丢、`[ime] spot` 随之更新。

### 4.2 UI-TERM-001：候选窗像素位置

**需要**：同上环境 + `grim`（Wayland 截图）。

```bash
TerVO_KEYLOG=1 SLINT_BACKEND=winit-software ./target/release/xtervo --shell /bin/cat
# 点击终端中部 → 输入 nihao（不按空格，先出候选窗）
grim -g "$(slurp)" /tmp/ime.png        # 或整屏 grim /tmp/ime.png
```

判定：候选窗左上角应落在 `[ime] spot` 日志给出的 `win=x,y` 附近（误差 ≤ 一个 cell 高度）；
把 pane 拖动/分屏后再试，`spot` 与截图应同步变化。

### 4.3 UI-WIN-005：真机 GPU 流畅度

> **注意**：agent 沙箱里看不到 `/dev/dri`/NVIDIA 设备节点（bwrap 隔离），所以这条必须在**宿主机**上跑。
> 仓库提供一键脚本：`./scripts/verify_gpu_host.sh`（只读，产物在 `.logs/gpu-host-verify/`）；
> 加 `--check` 直接输出 PASS/FAIL（退出码 0/1），不用自己看日志：

```bash
./scripts/verify_gpu_host.sh --check
# PASS  GPU 可见：NVIDIA GeForce RTX 4060
# PASS  未回退：actual_renderer=winit-skia
# PASS  GPU 渲染生效：actual_renderer=winit-skia
# PASS  damage 路径 draw_p95=36.9µs（阈值 <500µs）
# PASS  full 200x50 total_p50=1603µs（阈值 <5000µs）
# RESULT: PASS（UI-WIN-005 的真机 GPU 部分可转 implemented）
```

判定阈值：damage 路径 `draw_p95_us < 500`、`full_200x50 total_p50_us < 5000`；
WGPU 显示链路失败但 Skia GPU 仍生效时只给 WARN（`XTERVO_GPU_NATIVE` 属实验路径）。


**需要**：有可用驱动的 NVIDIA/AMD/Intel GPU 机器（`nvidia-smi -L` 或 `vulkaninfo --summary` 正常）。

```bash
nvidia-smi -L                      # 或 glxinfo -B / vulkaninfo --summary
./target/release/xtervo             # 默认 GPU（Skia / wgpu）；日志确认没有 exec 回退
XTERVO_GPU_NATIVE=1 XTERVO_GPU_NATIVE_DEBUG=1 ./target/release/xtervo   # 帧统计
XTERVO_GPU_NATIVE=1 XTERVO_GPU_NATIVE_DUMP=/tmp/frame ./target/release/xtervo  # 首帧回读 PPM

./scripts/bench_render.sh          # 渲染 P95（damage / full）
./scripts/bench.sh                 # PTY→可见吞吐
```

判定（参考基线，见 `docs/PERFORMANCE.md` §2.5）：

```text
damage 路径 P95 约 37–40 µs；full 200x50 约 1.66 ms；PTY→可见 10 MiB ≈ 178 ms ≈ 56 MiB/s
真机 GPU 下不得出现“1s 内确认不到 GPU API → exec 回退软件”的日志
```

## 5. 功能冒烟（手验，10 分钟）

| 功能 | 操作 | 期望 |
|---|---|---|
| TabBar `+` / `>_` | 一键运行 `./scripts/e2e_tabbar_plus_profile.sh` | `+` 直接开默认 Tab；`>_` 纯 Profile 列表、↓+Enter 可打开第二个 Profile；截图在 `.logs/tabbar-plus-profile/e2e/shots/` |
| 分屏拖拽 | 两个 pane，拖右上角手柄到另一个 pane | 交换位置，`Ctrl+Alt+Z` 可撤销 |
| 广播危险命令 | 开广播输入（右键菜单），输入 `rm -rf /` 回车 | 红条 + 二次确认，可只发当前 pane |
| 工作区 | 命令面板 `Save Layout as Workspace` → 起名 → 设置页 Workspaces 列表 | 可打开/重命名/删除/导出 JSON |
| 竖排 TabBar | 设置 → Window → Left/Right | Tab 竖排、滚动、按钮固定、上/下布局不变 |
| 录制 | 命令面板 `Start Recording` → 跑点输出 → `Export Recording…` | 导出原始字节，重放能看到颜色/图片序列 |
| 菜单装饰 | 安装示例插件（`docs/examples/plugin-v3-echo`），清单加 `menu = true` | 右键菜单末尾出现该命令 |
| 安全模式 | `echo 3 > ~/.local/state/xtervo/session.crashes` + 留 `session.running` 后启动 | 日志 `[safe-mode]`，本次不加载插件 |
| 崩溃应用后的键盘 | shell 里跑 `sh -c 'printf "\033[>7u"; kill -9 $$'`，回到提示符后按 `Ctrl+G` | 屏幕不出现 `7;5u`/`9;5:3u` 等 CSI-u 尾巴；日志有 `[keyboard] 前台进程变化：复位 …` |
| 内联图片 | `img2sixel`/`chafa` 输出图片；点击图片 | 位置正确，灯箱可缩放 |

## 6. 证据在哪

```text
.logs/                    门禁报告、e2e 截图、状态页（gitignored，可重跑）
docs/evidence/            长期证据（如 ssh-plugin-poc 的 banner/错误截图与 probe 日志）
docs/FEATURE_MATRIX.md    每条目的 Test 列（命令 + 证据路径）
docs/CLOSURE.md           汇总数字与口径
```

## 7. 如果你想复跑“今晚这批”的全部验证

```bash
cargo test -p xtervo-core && cargo test -p xtervo-app --bin xtervo
python3 scripts/check_docs_counts.py && python3 scripts/check_doc_links.py
python3 scripts/scope_tiers.py
env DURATION=2m BIN=$PWD/target/release/xtervo SOAK_NO_SHOTS=1 \
  XDG_RUNTIME_DIR=/tmp/soak-rt XTERVO_INSTANCE_DIR=/tmp/xtervo-inst ./scripts/soak.sh
./scripts/e2e_workspace_ui.sh          # 需要 Xvfb + openbox（脚本自带）
./scripts/vt_compat_suite.sh           # 32 PASS / 0 FAIL / 9 XFAIL
./scripts/fuzz_vt.sh                   # 确定性 fuzz
```
