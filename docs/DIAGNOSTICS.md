# Tervo 诊断 CLI（harness-neutral）

状态：**阶段 A 已落地（2026-11-10）**。

排障前先读事故复盘：[`POSTMORTEM_2026-11-11_RENDER_RESIDUE.md`](POSTMORTEM_2026-11-11_RENDER_RESIDUE.md)。

```text
tervo diag env --json
tervo diag snapshot --json
tervo diag replay --input trace-raw.bin --json
tervo diag trace --input trace-raw.bin --out trace.jsonl
tervo diag metrics
tervo diag observability
tervo diag scene-diff --left before.json --right after.json --json
tervo diag serve            # MCP stdio server
```

不启动 Slint 窗口、不需要 DISPLAY；任何 harness（Codex CLI、Claude Code、
DSH、普通 shell）都可以直接调用。

## 命令

### env

```bash
tervo diag --json
```

输出：

```json
{
  "schema": "tervo.diag.v1",
  "kind": "env",
  "version": "0.2.0",
  "commit": "...",
  "display": ":0",
  "backend": null,
  "dri": true,
  "config": "/home/me/.config/tervo/config.toml",
  "profiles": "/home/me/.config/tervo/profiles.toml",
  "log": "..."
}
```

不输出环境变量值、Vault 内容或 API key。

### snapshot

启动一次性 PTY，等 `--wait-ms` 后抓当前 frame：

```bash
tervo diag snapshot --shell /bin/sh --wait-ms 300 -- -c 'printf "hello\n"'
```

常用参数：

```text
--cols N       默认 120
--rows N       默认 32
--wait-ms N    默认 250
--shell PATH   默认 /bin/bash
-- <args...>   传给 shell 的参数
```

### replay

把录制下来的原始字节通过真实 PTY/core 回放，抓 frame：

```bash
tervo diag replay --input bytes.bin --cols 120 --rows 32 --wait-ms 400
```

路径会转成绝对路径；frame 包含：

```text
text            当前视口文本（trim 尾部空白）
cursor          row/col/visible/shape
mode            TermMode 原始位
display_offset  滚动 offset
images[]        id/row/col/width/height（不含 RGBA）
```

### trace

```bash
tervo diag trace --input bytes.bin --out trace.jsonl --duration-ms 1000
```

回放输入字节并按 frame generation 变化写 JSONL：

```text
{"schema":"tervo.diag.v1","kind":"trace","seq":1,"ts_ms":25,
 "generation":1,"input":"/abs/bytes.bin", ...frame 字段}
```

当前 trace 是 **frame 变化流**，不是完整 PTY 事件流；输入文件本身可保留原始
字节，后续再增加 pty/damage 事件。

### metrics / observability（P0 additive）

```bash
tervo diag metrics
tervo diag observability
```

输出 `tervo.diag.v1` 的 additive 字段：

```text
metrics       atomic frame/damage/image/texture/presenter/stale 计数器
resources     image resources/placements、glyph atlas、GPU textures 的 count+bytes
trace         有界 TraceBuffer（capacity/dropped/events）
integration   仅 observability：下一步接线点（machine-readable）
```

CLI 进程没有 app 渲染循环，所以当前计数是**真实的空 registry 默认值**：
`initialized=false`、`source=cli`，不伪造 live app 指标。schema/字段说明见
[`OBSERVABILITY.md`](OBSERVABILITY.md)。app 接入后仍走同一 shape，kind 可保持
`metrics`/`observability` 并由 live snapshot 合并。

### scene-diff

```bash
tervo diag scene-diff --left before.json --right after.json
tervo diag scene-diff --left before.json --right after.json --json
```

比较两个 frame JSON（`tervo.diag.v1` snapshot/trace）或轻量 SceneSummary：
文本 changed rows/cells、图片 added/removed/moved/resized/source_changed、
damage region 增删和 coverage ppm。默认输出有界文本；`--json` 输出
`kind=scene_diff`。frame JSON 新增 additive 的
`damage.full/region_count/rects` 字段。

## live control socket（已实现，显式 opt-in）

启动 Tervo 时：

```bash
TERVO_DIAG_SOCKET=/run/user/$(id -u)/tervo-diag.sock TERVO_DIAG_TOKEN=$(head -c 24 /dev/urandom | base64) tervo
```

- Unix stream，权限 0600
- 默认不启动；未设置 env 就没有监听
- JSON Lines 请求：`{"id":1,"cmd":"capabilities","token":"..."}` / `{"cmd":"snapshot"}`
- 只读命令：`capabilities`、`snapshot`
- `snapshot` 经 `invoke_from_event_loop` 回 UI 线程取当前所有 pane 的 frame/cursor/images
- 2s 超时；不阻塞渲染/输入

MCP 读取 live：

```bash
tervo diag serve --socket /run/user/$(id -u)/tervo-diag.sock --token "$TERVO_DIAG_TOKEN"
```

额外工具：

```text
tervo.live_capabilities
tervo.live_snapshot
```

没有 socket 时 MCP 仍提供静态 env/snapshot/replay。

自动验收：

```bash
./scripts/e2e_diag_live.sh
# PASS: live socket capabilities/snapshot
# PASS: MCP tools/list + live_snapshot
```

## 排障环境变量（显式 opt-in）

```bash
TERVO_LINE_NUMBERS=1
# 终端行号 overlay；每行显示 1..N，会盖住最左约 3 个字符，仅调试。

TERVO_FRAME_DUMP=/tmp/tervo-frame.txt
# 每次重绘前把当前 Frame 文本写入该文件；用于区分 Term/Frame 与 Slint 层。

TERVO_SIXEL_DEBUG=1
# 打印图片 placement 的 id/row/col；图片滚动/删除问题使用。

TERVO_RENDER_PROFILE=1
# 打印 full/partial、rows/copy/damage/draw/image 耗时。
```

### 分层排障顺序

```text
raw PTY bytes
  -> IIP/Sixel/Kitty filter
  -> alacritty Term
  -> partial Frame + damage
  -> force_full Frame
  -> 增量 framebuffer
  -> fresh full framebuffer
  -> xwd/take_snapshot
```

只修第一处出现分歧的层。不要用默认整屏重绘掩盖上游字节/Frame 错误。
详见事故复盘第 5 节。

## schema v1

所有输出：

```json
{"schema":"tervo.diag.v1","kind":"env|snapshot|replay", ...}
```

字段只做加法；未知字段读取方应忽略。图片只给 rect，避免大 payload/敏感像素。

## harness 用法

### Codex CLI / Claude Code / DSH

这些 harness 在 shell 内执行即可：

```bash
tervo diag snapshot --shell /bin/sh --wait-ms 300 -- -c '<command>'
```

它们不需要 Tervo 插件；诊断由宿主自带。

### MCP stdio（已实现，阶段 A）

```bash
tervo diag serve
```

这是 **内置 CLI 子命令**，不是插件；MCP client 直接 spawn 它，stdin/stdout
JSON-RPC，日志只走 stderr。

已实现工具：

```text
tervo.diag_env       版本/display/config/log 路径
tervo.diag_snapshot  一次性 PTY 运行 command，返回 frame/cursor/images
tervo.diag_replay    回放原始字节文件，返回 frame
```

Claude Code：

```bash
claude mcp add tervo-diag -- tervo diag serve
```

Codex CLI（`~/.codex/config.toml`）：

```toml
[mcp_servers.tervo_diag]
command = "tervo"
args = ["diag", "serve"]
```

DSH / 其他 MCP harness：同样 spawn `tervo diag serve`，按标准
`initialize` → `tools/list` → `tools/call` 调用。没有 MCP 的 harness 用
`tervo diag snapshot --json` 即可。

安全边界：

```text
- 只读 Tervo 状态；snapshot 会执行调用者给的 shell command（本地诊断进程）
- 不读 Vault/API key/环境变量值
- 不写日志/stdout 正文以外的终端内容
- 仅本地 stdio，无网络监听
```

尚未实现：

```text
- 读正在运行的 Tervo 实例（app control socket）
- screenshot / image RGBA / PTY 事件级 trace
- 权限 token / per-harness 鉴权
```

## 限制

```text
- 当前只能抓一次性 pane，不能读正在运行的 Tervo 实例
- metrics/observability CLI 是未接线的 schema；app render loop 尚未喂 registry
- 没有 trace（PTY/frame/image 事件流），replay 输入是原始字节文件
- 没有 screenshot/PNG
- 没有 MCP / Unix socket / token
- raw bytes 可能含终端内容，录制/分享前自行脱敏
```

observability 设计/集成点见 [`OBSERVABILITY.md`](OBSERVABILITY.md)。

## 验证

```bash
./target/debug/tervo diag env --json
./target/debug/tervo diag snapshot --shell /bin/sh --wait-ms 300 -- -c 'printf hello'
./target/debug/tervo diag replay --input .logs/soak/final-2m/sixel-a.six --wait-ms 800
```

最后一条应输出 `images:[{row:0,col:0,width:120,height:72,...}]`。
