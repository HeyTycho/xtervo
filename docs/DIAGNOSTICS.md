# Tervo 诊断 CLI（harness-neutral）

状态：**阶段 A 已落地（2026-11-10）**。

排障前先读事故复盘：[`POSTMORTEM_2026-11-11_RENDER_RESIDUE.md`](archive/POSTMORTEM_2026-11-11_RENDER_RESIDUE.md)。

```text
tervo diag env --json
tervo diag snapshot --json
tervo diag replay --input trace-raw.bin --json
tervo diag trace --input trace-raw.bin --out trace.jsonl
tervo diag trace-layer --input trace-raw.bin --options frame-snapshot.json --json
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

### trace-layer（P0：第一处分歧）

```bash
# 合成夹具（无需外部文件）
tervo diag trace-layer --fixture clean
tervo diag trace-layer --fixture images --json

# 真实 dsh raw：自动读取同目录/上一级的 frame-snapshot.json 里的 cols/rows
tervo diag trace-layer \
  --input .logs/dsh-dump-run/pty/app-to-host.raw \
  --chunk 4096 --json
```

按事故复盘 §5.1 的阶梯逐层比较，**只在第一处分歧停下**：

```text
filter       raw bytes 与独立协议账本（Sixel DCS / OSC 1337;File / Kitty APC）
             比较；非图片协议字节必须逐字节保留；再比较分块 vs 一次性
             filter 输出是否一致
term         filtered bytes 分块喂入 vs 一次性喂入后的 Term cell/cursor/mode
frame        生产 pull 路径的增量 Frame vs 全新 Term 的 force-full Frame
             （逐 cell/行、cursor/mode/display_offset；RowId 不跨 Term 实例比较）
framebuffer  renderer 的 cell 级模型：按 damage 增量更新上一帧 screen，
             再与 Term 真相逐 cell 比较（旧字符/残影）
image        解码出的图片 placement 锚点与 row 裁剪几何（source_y0 等）
```

输出同时支持人类可读（默认）与 `--json`。`status` 只有 `PASS` / `FAIL`；
找不到分歧时 `first_divergence=null`，不会只给统计不给位置：

```text
tervo diag trace-layer: FAIL
input: fixture:clean (215 bytes)
options: 120x32 chunk=16384 cell=10x20 (defaults)
layers:
  filter      FAIL   raw_bytes=215  filtered_bytes=172 ...
  term        SKIPPED
  ...
first divergence:
  layer: filter
  check: byte_conservation
  step : 1
  chunk: raw bytes [0,215)
  byte : 6
  left  (ledger_expected): 0x65 'e'
  right (pipeline): <missing; next byte 0x6c 'l'>
  context: ascii=".[32mhe <<<llo.[0m world..line2 [s" hex=...
  explain: 字节守恒失败：协议账本认为 raw byte 6 不属于任何图片序列，
           但 filter 链没有输出它（疑似 filter 吞字节）
```

JSON（`--json`）是 `tervo.diag.v1`、`kind=trace_layer`；`first_divergence`
含 `layer/check/at{byte,row,col,chunk,step}`、`left`/`right`（byte/hex/
cell/missing）与 `context/explain`。脚本读 `status`，**退出码始终为 0**。

参数：

```text
--input FILE          raw PTY 字节；`-` 读 stdin
--fixture NAME        clean | images | filter-edge
--options FILE        读取 cols/rows/cell_width/cell_height/chunk/scrollback
--cols/--rows         覆盖尺寸（缺省 120x32；有 frame-snapshot.json 时自动读取）
--chunk N             filter/Term 分块（默认 16384；真实 dsh 可用 4096 加压）
--cell-width/height   图片裁剪审计用 cell 尺寸（默认 10x20）
--no-images           只过滤图片协议、不解码（图片层 SKIPPED）
--fault SPEC          仅自测定位器，见下
```

`--fault` 用来证明“定位器真的能定位”，生产诊断不要使用：

```text
filter-drop=N       从 filtered 输出删掉第 N 个字节（模拟 filter 吞字节）
frame-residue=R:C[@STEP]  第 STEP 次 pull 后把 (R,C) 改成旧内容（模拟残影）
damage-drop=R       删掉第 R 行的 damage（模拟 renderer 不重绘）
```

自测示例：

```bash
tervo diag trace-layer --fixture clean --fault filter-drop=6 --json
tervo diag trace-layer --fixture clean --fault frame-residue=1:2 --json
tervo diag trace-layer --fixture clean --fault damage-drop=0 --chunk 7 --json
```

测试覆盖：filter 吞字节（含 UTF-8 0x9D 续字节）、C1 文本后接 Sixel 的
chunk 依赖、Frame 残影、damage 残影、图片 `row=-1` 裁剪（纯几何 + 真实
PaneActor frame）、真实 dsh raw 全链路 PASS（存在 fixture 时）。

限制（也在 JSON `limitations` 里显式列出）：

```text
- CLI 不建窗口：只比较 cell 级 framebuffer，不比较完整像素 framebuffer/Slint
- Term 层是“分块 vs 一次性”，不是第二个终端模拟器对照
- 图片层只审计解码 placement 的裁剪几何；SixelFilter 同 chunk 事件顺序
  （同 chunk 文本先于图片时 anchor 可能滞后）是 core 已知边界，不在本切片修
- 协议账本是独立整段扫描器，不跑生产 filter；它只回答“哪些字节属于图片协议”
```

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

## live control socket（已实现，默认开启只读）

Tervo 正常启动即可（`mcp_enabled=true`，默认）：

```bash
tervo
# socket: ${XDG_RUNTIME_DIR:-$HOME/.local/state}/tervo/mcp.sock  (0600)
# token:  ${XDG_STATE_HOME:-$HOME/.local/state}/tervo/mcp.token (0600)
```

- Unix stream，权限 0600；总开关 `mcp_enabled=false` 时不创建 socket
- 默认只读：`capabilities`、`snapshot`、`list`、`output`
- 写/破坏性命令按 `mcp_input` / `mcp_tabs` / `mcp_destructive` 分组授权
  （Settings → Privacy），app 端对每个请求二次校验
- JSON Lines 请求：`{"id":1,"cmd":"capabilities","token":"..."}` / `{"cmd":"snapshot"}`
- `snapshot` 经 `invoke_from_event_loop` 回 UI 线程取当前所有 pane 的 frame/cursor/images
- 2s 超时；不阻塞渲染/输入

MCP bridge 无参数自动发现 socket/token：

```bash
tervo diag serve
```

`--socket` / `--token` 与 `TERVO_DIAG_SOCKET` / `TERVO_DIAG_TOKEN` 仍是
测试/CI override。完整权限与工具列表见
[`MCP_TERMINAL_CONTROL.md`](MCP_TERMINAL_CONTROL.md)。没有 socket 时 MCP 仍提供
静态 env/snapshot/replay。

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
JSON-RPC 2.0（每行一个 JSON 对象），日志只走 stderr。

已实现工具：

```text
tervo.diag_env       版本/display/config/log 路径
tervo.diag_snapshot  一次性 PTY 运行 command，返回 frame/cursor/images
tervo.diag_replay    回放原始字节文件，返回 frame
tervo.doctor_check   确定性 doctor；输出 tervo.diagnostic_case.v1（只读、无 shell）
tervo.issue_draft    对完整 doctor case 渲染 markdown issue 草稿（只读）
tervo.metrics_read   Tervo 日志目录的有界统计（只读、拒绝任意路径）
```

资源：

```text
tervo://version      版本/commit/build/target/display 元数据（application/json）
```

`tervo.doctor_check` 的输入可以是内联 JSON object，也可以是 `$HOME` 或 `/tmp`
下的 JSON 文件路径（单个文件上限 1 MiB）：

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "tools/call",
  "params": {
    "name": "tervo.doctor_check",
    "arguments": {
      "environment": {"schema": "tervo.diag.v1", "kind": "env", "version": "0.3.1"},
      "frame": "/tmp/tervo-frame.json",
      "metrics": "/tmp/tervo-metrics.json",
      "symptom": "滚动后残影",
      "repro_steps": ["打开 Tervo", "滚动输出"]
    }
  }
}
```

输出就是 `tervo.diagnostic_case.v1` JSON（含 `issue_draft`）。把该对象原样传给
`tervo.issue_draft` 即可得到 markdown：

```json
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
  "name":"tervo.issue_draft","arguments":{"case": <tervo.doctor_check 的输出>}
}}
```

`tervo.metrics_read` 默认读取 `TERVO_LOG_DIR` / XDG state 下的 Tervo 日志目录，
也可显式传 `log_dir`。路径必须位于 `$HOME` 或 `/tmp`，且只统计普通文件
`tervo-*.log`：最多 8 个文件、每个只扫描尾部 256 KiB；返回文件名、字节数、
扫描行数和最近 error/panic/fatal 行数，**不返回原始行**。

MCP 资源读取：

```json
{"jsonrpc":"2.0","id":3,"method":"resources/list","params":{}}
{"jsonrpc":"2.0","id":4,"method":"resources/read","params":{"uri":"tervo://version"}}
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

协议错误码：

```text
-32700  坏 JSON / parse error
-32600  非法请求（缺 method、params 不是 object、超长请求）
-32601  未知 method
-32602  未知工具、缺 tool name、arguments 不是 object
-32002  未知 resource URI
```

工具自身失败时优先返回 MCP 语义的
`{"content":[{"type":"text","text":"..."}],"isError":true}`。

安全边界：

```text
- doctor/issue/metrics 只读：不启动进程、不写文件、不访问网络
- 文件路径只允许 $HOME 或 /tmp 下（canonicalize 后校验，拒绝 symlink 逃逸）
- doctor 只读有界配置/日志路径；配置里的 secret-like 值只输出计数，不输出正文
- metrics 只统计 tervo-*.log 的尾部摘要，不返回日志原文
- 不读 Vault/API key/环境变量值；不整段读取 scrollback
- resource 只暴露版本/构建元数据
- 仅本地 stdio，无网络监听
- snapshot 仍会执行调用者显式给出的 shell command（原有诊断行为，保持
  “本地诊断进程”说明；新工具不使用这个能力）
```

尚未实现：

```text
- screenshot / image RGBA / PTY 事件级 trace
- MCP 鉴权 token / per-harness 授权
- doctor/metrics 的 live app 数据（当前只读进程内 env 与显式输入）
```

## 限制

```text
- 默认只能抓一次性 pane；读运行中的 Tervo 实例需显式启用 live control socket
- metrics/observability CLI 是未接线的 schema；app render loop 尚未喂 registry
- 没有 trace（PTY/frame/image 事件流），replay 输入是原始字节文件
- 没有 screenshot/PNG
- MCP stdio 已实现，但无鉴权 token / per-harness 授权；仅限本地显式 spawn
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
