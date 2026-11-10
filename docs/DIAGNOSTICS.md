# Tervo 诊断 CLI（harness-neutral）

状态：**阶段 A 已落地（2026-11-10）**。

```text
tervo diag env --json
tervo diag snapshot --json
tervo diag replay --input trace-raw.bin --json
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

### MCP

尚未实现。计划是 `tervo-mcp` 只读 server，见
`docs/issues/0001-harness-neutral-terminal-diagnostics.md` 和 `docs/MCP.md`。

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
