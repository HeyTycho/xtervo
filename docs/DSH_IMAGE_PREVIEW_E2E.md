# dsh-tui 图片预览 e2e（隔离 harness）

目标：在**不碰真实 :1 会话、不依赖手工操作**的前提下，可复现地验证
“dsh-tui 的图片预览在 xtervo 里能打开、能缩放/平移、关闭后无残留”。

状态图例：`PASS` 有截图/原始字节证据；`FAIL` 有失败证据；`BLOCKED` 为无法
在该环境完成的部分。**本文不把任何 FAIL 写成 PASS。**

## 1. 隔离模型

| 维度 | 做法 |
| --- | --- |
| X 显示 | 脚本自建 `Xvfb`，TCP-only `127.0.0.1:<280+pid%20>.0`，屏幕 `1400x950x24`；不回退 Xephyr，不使用 `:1` |
| 终端 | xtervo `--cols 130 --rows 40`，固定网格；窗口约 `1170x912`（9x22 字体格） |
| HOME | `$E2E_OUT/home` 私有副本；`DSH_HOME=$E2E_OUT/home/.dsh` |
| DSH_HOME | 复制 `profiles/dsh-tui`、`profiles/node_modules`、指定 session、`attachments`、`.credentials.yaml`（chmod 600）与 theme;不写回真实 `$HOME` |
| session | 复制真实 dsh 会话 `35378219-8b26-48b1-b67c-9ede5f0bf6bc`（含 8 张 transcript 图片）到副本；`--resume` 只读副本 |
| 输入 | 真实 X11 PRIMARY selection（`xsel` 持有真实 PNG 路径）+ xtervo 中键粘贴 → bracketed paste → dsh `[Image #N]`；不使用假 `xclip` |
| 进程回收 | 只 kill 本脚本记录的 Xvfb / xsel / xtervo 进程组（`setsid`）；不做全局 kill |
| 产物 | `$E2E_OUT/{shots,pty,logs,frame-snapshot.json}`；`.logs/` 已 gitignore |

## 2. 运行

```bash
cd .worktrees/agent-e2e
cargo build -p xtervo-app --bin xtervo --bin xwin --bin xclick --bin xkey

bash scripts/e2e_dsh_image_preview.sh
```

本 worktree 没有独立构建产物时，可显式指向同 commit 的构建：

```bash
T=/path/to/rusttabbyterminal/target/debug
E2E_BIN=$T/xtervo E2E_XWIN=$T/xwin E2E_XCLICK=$T/xclick E2E_XKEY=$T/xkey \
E2E_XVFB_BIN=/path/to/.perf/xvfb/usr/bin/Xvfb \
bash scripts/e2e_dsh_image_preview.sh
```

依赖：`Pillow`、`xsel`、`node`（dsh-tui profile）、`zstd`、`xdpyinfo/xwininfo/xwd`。

可覆盖：`E2E_OUT`、`E2E_BIN`、`E2E_XVFB_BIN`、`E2E_SESSION_ID`、
`E2E_DSH_SRC_HOME`、`E2E_DSH_TUI_SRC`、`E2E_SCRATCH_HOME`。

## 3. 断言与证据

| # | 断言 | 证据 |
| --- | --- | --- |
| 1 | xtervo 向 dsh-tui 宣告 Sixel：DA1 被改写为 `ESC[?62;4;9;22c` | `pty/host-to-app.raw` 字节 grep |
| 2 | xtervo 不冒充 Kitty：Kitty 探测无 `Gi=31;OK` 回复 | `pty/host-to-app.raw` |
| 3 | dsh-tui 开启 alt-screen / bracketed paste / SGR mouse | `pty/app-to-host.raw` |
| 4 | 复制会话的 transcript 缩略图以 Sixel 渲染 | `shots/02-transcript-images.png` + xtervo `[sixel] images=[(id,row,col)...]` 日志 + `app-to-host.raw` 中 `ESC P...q` |
| 5 | dsh-tui 打开自己的大图预览（标题 `Image #1 — PNG · 640×400 · …`） | `shots/03-transcript-preview-open.png`；原始字节中 `Open original` |
| 6 | 缩放 `+` → 200% | `shots/04-preview-zoom.png` + 原始字节 `200%` |
| 7 | 平移/滚动改变画面 | `shots/05-preview-pan.png` 与 04 像素 diff > 500 |
| 8 | 关闭后无图片残留 | `shots/06-preview-closed.png` 与 02 的像素 diff ≤ 3000 |
| 9 | 真实剪贴板链路：X PRIMARY（真实 PNG）→ xtervo 中键 → bracketed paste → `[Image #1]` | `host-to-app.raw` 中 `ESC[200~`；`app-to-host.raw` 中 `Pasted image` / `[Image #1]`；`shots/07-clipboard-token.png` |
| 10 | 输入框图片 peek 预览（真实 X 点击 token）打开并显示 fixture | `shots/08-composer-peek.png` 洋红像素 > 8000 |
| 11 | peek 关闭无残留 | `shots/09-composer-peek-closed.png` 洋红 ≤ 500 且与 07 diff ≤ 3000 |
| 12 | 文本帧快照（不依赖 X 截图） | `frame-snapshot.json`（`xtervo diag replay --input pty/app-to-host.raw`） |

`scripts/e2e_dsh_image_checks.py` 负责洋红像素计数、截图 diff、暗色缩略图
定位；`scripts/e2e_dsh_pty_proxy.py` 记录双向原始 PTY 字节，并提供
`inject.sock` 以便在 PTY 边界注入等价 SGR 鼠标事件。

## 4. 已发现的真实交互问题

### 4.1 xtervo 灯箱吞掉 transcript 图片点击（确定性入口的 blocker）

- 现象：鼠标点击 Sixel 缩略图时，xtervo 自己的 `[image] preview open id=N`
  先打开（底部 “Image preview / Fit / 100% / Close”），dsh-tui 收不到这次
  点击，其大图预览不会出现。
- 证据（未提交，`.logs/proto2/`）：`q_xtervo_lightbox.png`（xtervo 灯箱）、
  `q_after_close_btn.png`（关闭灯箱后 transcript，无 dsh 预览）。
- 影响：`scripts/e2e_image_display.sh` 里“点图片打开灯箱”的断言与
  dsh-tui 的“点缩略图打开预览”在同一坐标上互斥。
- harness 处理：transcript 缩略图点击改为经 `inject.sock` 向 dsh-tui stdin
  注入等价 SGR 鼠标报告（`ESC[<0;col;rowM` + release）；这验证 dsh-tui 的
  预览逻辑，但**不验证 xtervo 的点击路由**。输入框 `[Image #N]` 的点击是
  纯文本区域，仍然走真实 X 点击。

### 4.2 关闭预览后 Sixel 残留（复现中）

- 现象（`.logs/proto2/z_closed.png`）：Esc 关闭 composer peek 后，粉色
  fixture 图像仍覆盖在 transcript 上，文本已重绘、图片 overlay 未清除。
- 这是本 harness 第 8/11 条断言要抓的回归；如果复现，harness 必须返回
  非零，而不是 PASS。
- 目前该证据来自手工原型运行，尚未由 harness 本轮正式复现。

### 4.3 Kitty 参考未做

- 本机 `kitty` 实际安装在 `/usr/bin/kitty`，但 harness 尚未驱动它作为
  对照终端；当前对照是 xtervo 自己的灯箱截图（同一 Sixel 字节、不同协议
  消费者）。这是明确的覆盖缺口，不能声称“与 kitty 对比通过”。

## 5. 当前验证结果（诚实记录）

最近一次 harness 全量运行（2026-11-10，`E2E_BIN` 指向主仓
`target/debug/xtervo`，其版本串为 `0.3.1 commit efdfd54d9a82 (dirty)`，
与 worktree HEAD `c2d2f19` 不是同一构建）：

```text
PASS: xtervo DA1 advertises Sixel (ESC[?62;4;9;22c)
PASS: xtervo does not claim Kitty graphics (no Gi=31;OK reply)
PASS: dsh-tui enabled SGR mouse reporting
PASS: dsh-tui enabled bracketed paste
PASS: xtervo forwarded the clipboard as a bracketed paste (ESC[200~)
PASS: dsh-tui staged the real PNG as [Image #1]
PASS: closing the composer peek left no residue (magenta=0 diff=673)
PASS: saved textual frame snapshot (frame-snapshot.json)
FAIL: no Sixel transcript thumbnail appeared after scrolling   # 解析 bug：已修
FAIL: dsh-tui preview did not open from transcript thumbnail click  # 同上
FAIL: composer image peek did not open after two Left presses   # 入口改为点击 token，待复跑
FAIL: composer peek does not show the fixture (magenta=0 px)    # 同上
```

这次运行的失败来自 harness 自身的两处缺陷，已修复但尚未复跑：
1. `[sixel] offset=... images=[...]` 行被错误地按 `[sixel] images=` 匹配，
   导致缩略图锚点解析为空、点击坐标落到图片左侧；
2. peek 只用键盘 `Left` 触发，未使用已证可行的 token 点击入口。

**在修复后复跑通过之前，本 harness 不能标 PASS。** 手工原型已经证明
dsh-tui 预览可打开（`.logs/proto2/y_click_token.png`：标题
`Image #1 — PNG · 640×400 · 2.3 KB · fixture-magenta.png`，fixture 与
`Open original` 路径可见），但证据来自原型而非 harness。

## 6. 构建来源与限制

- 本轮验证用的 xtervo 二进制来自主仓 `target/debug/xtervo`
  （`efdfd54d9a82 (dirty)`，2026-11-10 构建），不是 worktree HEAD
  `c2d2f19` 的构建；正式跑 gate 应先在本 worktree `cargo build`。
- 截图坐标依赖固定字体格 `9x22`；cell 尺寸从 xtervo `READY TIMING` 日志解析，
  窗口尺寸从 `xwininfo` 解析，但字体变化会影响像素断言阈值。
- 会话是从真实 `$HOME/.dsh` 复制的；fixture PNG 是生成的测试图，不涉及
  真实用户图片内容外泄（截图只写 `.logs/`）。
- 该 harness 不证明 Kitty 渲染、也不证明 xtervo 灯箱与 dsh 预览能同时存在。
