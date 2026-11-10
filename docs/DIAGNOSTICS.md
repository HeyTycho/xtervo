# Tervo 诊断 CLI（harness-neutral）

状态：**阶段 A 已落地（2026-11-10）**。

```text
tervo diag env --json
tervo diag snapshot --json
tervo diag replay --input trace-raw.bin --json
tervo diag trace --input trace-raw.bin --out trace.jsonl
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
- 没有 trace（PTY/frame/image 事件流），replay 输入是原始字节文件
- 没有 screenshot/PNG
- 没有 MCP / Unix socket / token
- raw bytes 可能含终端内容，录制/分享前自行脱敏
```

## 验证

```bash
./target/debug/tervo diag env --json
./target/debug/tervo diag snapshot --shell /bin/sh --wait-ms 300 -- -c 'printf hello'
./target/debug/tervo diag replay --input .logs/soak/final-2m/sixel-a.six --wait-ms 800
```

最后一条应输出 `images:[{row:0,col:0,width:120,height:72,...}]`。
