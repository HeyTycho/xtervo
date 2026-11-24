# MCP 桥：离线诊断 + live terminal control

> Live socket tools 的权限/协议细节见
> [`MCP_TERMINAL_CONTROL.md`](MCP_TERMINAL_CONTROL.md)。
>
> **状态（2026-11-13）**：MCP 默认开启，但默认**只读**。xtervo 启动时自动创建
> `$XDG_RUNTIME_DIR/xtervo/mcp.sock`（0600）和 state token（0600），不再需要
> `XTERVO_DIAG_SOCKET`。写能力必须在 Settings → Privacy 里按分组授权，或使用
> 旧的 env override（测试/CI）。MCP 是宿主内嵌功能，不是插件。

## 默认行为与配置

配置是 flat TOML（`~/.config/xtervo/config.toml`，可用 `XTERVO_CONFIG` 覆盖）：

```toml
# MCP 总开关；false = 不创建 live socket、不暴露 live 工具。
mcp_enabled = true
# 四个权限分组；默认只开 read。
mcp_read = true
mcp_input = false
mcp_tabs = false
mcp_destructive = false
```

- 旧配置没有这些字段时，serde 默认补齐（`enabled/read = true`，其余 `false`），
  不需要迁移；字段是 additive，不 bump `version`。
- 设置页在 **Settings → Privacy → MCP live control**：总开关、四个分组开关、
  socket/token 路径、是否就绪。修改立即生效，不需要重启 xtervo。
- `mcp_enabled=false` 是硬关闭：即使设置了 `XTERVO_DIAG_SOCKET` 也不会创建
  socket；MCP 只剩离线诊断工具。

### 请求格式与窗口路由

每个连接一行 JSON；**命令参数必须放在 `args` 里**，平铺字段会被忽略：

```json
{"id": 3, "cmd": "split", "token": "<mcp.token>", "args": {"pane_id": 1, "direction": "right"}}
```

- `token` 读 `~/.local/state/xtervo/mcp.token`（0600）；socket 在
  `$XDG_RUNTIME_DIR/xtervo/mcp.sock`（0600）。
- 可选 `args.window_id`：不传 = 当前活动窗口；显式值必须命中 WindowRegistry，否则报
  `unknown window_id`（不会悄悄改到别的窗口）。
- pane 不存在于目标窗口时，写命令必须返回 `unknown pane N in window W`，不能回
  `ok: true`（`split` 曾有此问题，2026-11-16 修复）。
- 响应固定是 `{"id", "ok", "data"|"error"}`；`error` 不含 token/命令文本。

权限分组：

| 分组 | 配置键 | socket commands |
| --- | --- | --- |
| read | `mcp_read` | `capabilities`, `snapshot`, `list`, `output` |
| input | `mcp_input` | `send_text`, `send_key`, `run` |
| tabs | `mcp_tabs` | `new_tab`, `new_window`, `split`, `focus`, `scroll` |
| destructive | `mcp_destructive` | `close_pane`, `close_tab`, `close_window`, `kill` |

`destructive` 是附加开关：`close_pane/close_tab/close_window` 还需要 `tabs`，`kill` 还需要
`input`。单独打开 destructive 不会授予任何破坏性工具。

旧的 `XTERVO_DIAG_ALLOW_WRITE=1` / `XTERVO_DIAG_ALLOW_DESTRUCTIVE=1` 仍是
override：前者放宽 input+tabs，后者放宽 destructive；但都不能绕过
`mcp_enabled=false`。这些 env 只建议测试/CI 使用。

## 目标

让外部 MCP client（Claude Desktop/Code、Codex、其他 agent）在本地显式
spawn 一个诊断/控制进程，读取 xtervo 的版本/构建、确定性 doctor 结果和日志
摘要，并在用户授权后操作 live pane，而不是让 xtervo 自己实现一套 AI 网络栈。

```text
MCP client
  ⇄ stdio JSON-RPC 2.0（每行一个 JSON）
xtervo diag serve
  ⇄ 本地 live Unix socket（0600 + token），或只读文件/路径检查
xtervo app（内嵌 socket listener）
```

## 启动

xtervo 正常启动即可：

```bash
xtervo
# [mcp] live socket ready path=/run/user/1000/xtervo/mcp.sock token=required
# token: ~/.local/state/xtervo/mcp.token (0600)
```

MCP client 不需要再传 socket/token：

```bash
xtervo diag serve
```

无参数时 bridge 会：

1. 读配置判断 MCP 是否启用；
2. 使用默认 socket 路径；
3. 读 state token 文件；
4. 在每次 `tools/list` 里向 app 查询 `capabilities`，只列出 app 实际允许的
   live 工具。

`--socket PATH` / `--token T` 以及 `XTERVO_DIAG_SOCKET` / `XTERVO_DIAG_TOKEN`
仍是 override，供测试/CI 或非默认目录使用。

### Claude Desktop / Claude Code

Claude Code：

```bash
claude mcp add xtervo-diag -- xtervo diag serve
```

Claude Desktop（`claude_desktop_config.json`）：

```json
{
  "mcpServers": {
    "xtervo": {
      "command": "/path/to/xtervo",
      "args": ["diag", "serve"]
    }
  }
}
```

### Codex CLI（`~/.codex/config.toml`）

```toml
[mcp_servers.xtervo_diag]
command = "xtervo"
args = ["diag", "serve"]
```

### 其他 harness

按 MCP stdio 标准发送 `initialize` → `notifications/initialized` →
`tools/list` → `tools/call`。没有 MCP 的 harness 继续用
`xtervo diag env/snapshot --json`。

## 工具

离线工具（始终可用，不需要 live socket）：

| 工具 | 输入 | 输出 | 说明 |
| --- | --- | --- | --- |
| `xtervo.diag_env` | 无 | 版本/display/config/log 路径 | 不返回环境变量值 |
| `xtervo.diag_snapshot` | `command` 等 | frame/cursor/images | **唯一**会启动一次性 PTY/shell 的离线工具 |
| `xtervo.diag_replay` | `input` 文件 | frame | 通过 PTY 回放原始字节 |
| `xtervo.doctor_check` | `environment`/`frame`/`metrics`（object 或 HOME//tmp 下路径）、`symptom` 等 | `xtervo.diagnostic_case.v1` JSON | 确定性、离线、只读、无 shell |
| `xtervo.issue_draft` | `case`（完整 DiagnosticCase） | `markdown` + title/labels | 固定 doctor issue 模板，无模型/网络 |
| `xtervo.metrics_read` | `log_dir?` | 文件/字节/行/错误计数 | 只统计 `xtervo-*.log` 尾部 256 KiB，不返回原始行 |

live 工具（仅当 `mcp_enabled=true` 且对应分组允许时出现在 `tools/list`）：

| 工具 | 分组 | 说明 |
| --- | --- | --- |
| `xtervo.live_capabilities` | read | 当前分组/命令（不含 token） |
| `xtervo.live_snapshot` | read | 所有 pane 的 frame/cursor/images 元数据 |
| `xtervo.live_list` | read | windows[]/active_window_id + pane id/title/grid/rect/focus |
| `xtervo.live_output` | read | 有界可见 pane 文本 |
| `xtervo.live_send_text` | input | 写文本，不自动 Enter |
| `xtervo.live_send_key` | input | Enter/Escape/Ctrl+C/Ctrl+D/方向键等 |
| `xtervo.live_run` | input | 写一行命令 + Enter；不额外套 shell |
| `xtervo.live_new_tab` | tabs | 新建 Tab |
| `xtervo.live_new_window` | tabs | 同进程新窗口（需 XTERVO_MULTI_WINDOW_INPROC=1） |
| `xtervo.live_split` | tabs | 向右/向下分屏 |
| `xtervo.live_focus` | tabs | 聚焦 pane |
| `xtervo.live_scroll` | tabs | 滚动 scrollback |
| `xtervo.live_close_pane` | tabs + destructive | 关闭 pane |
| `xtervo.live_close_tab` | tabs + destructive | 关闭 pane 所在 Tab |
| `xtervo.live_close_window` | tabs + destructive | 按 window_id 关窗口（拒绝关最后一个） |
| `xtervo.live_kill` | input + destructive | 向 pane 发 Ctrl+C |

**未实现/不暴露**：`xtervo.live_paste`、`xtervo.live_resize`、
`xtervo.live_metrics`、`xtervo.live_rename`。不要依赖这些工具。

### 资源

```text
xtervo://version  版本/commit/build/target/display 元数据（application/json）
```

`resources/list` 返回静态描述，`resources/read` 的 `contents[].text` 是 JSON
字符串。

### 示例

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{
  "protocolVersion":"2024-11-05","capabilities":{},
  "clientInfo":{"name":"example","version":"0"}
}}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
  "name":"xtervo.doctor_check","arguments":{
    "environment":{"schema":"xtervo.diag.v1","kind":"env","version":"0.4.0"},
    "symptom":"滚动后残影"
  }
}}
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{
  "name":"xtervo.issue_draft","arguments":{"case": <上一步输出的 case>}
}}
```

## 安全边界

```text
- doctor/issue/metrics 只读：不启动进程、不写文件、不访问网络
- 文件路径必须在 $HOME 或 /tmp 下；canonicalize 后校验，拒绝 symlink 逃逸
- 单个 JSON 输入文件上限 1 MiB；MCP 单行请求上限 1 MiB
- metrics 只枚举普通文件 xtervo-*.log，最多 8 个、每个尾部 256 KiB；
  只输出计数和文件名，不输出日志正文
- 不读 Vault/API key/环境变量值；不整段读取 scrollback
- doctor 对配置中的 secret-like 内容只输出规则计数，不输出值
- Unix socket 0600、state token 文件 0600；token 值不写入 config/log/
  feedback bundle
- tools/list 以 app 的 capabilities.commands 为准；app 对 tools/call 二次校验
- 仅本地 stdio/socket，无网络监听
- snapshot/replay 保留原有“本地诊断进程”行为；新工具不调用它们
```

## 模式与限制

```text
默认（mcp_enabled=true, mcp_read=true, 其他 false）
  socket 自动创建；只列出 live_capabilities/snapshot/list/output
  input/tabs/destructive 的 tools/call 由 app 拒绝

分组授权后
  新工具在下一次 tools/list 出现（server 重新探测 capabilities）
  收紧后立即拒绝；tools/call 不依赖 server 缓存

mcp_enabled=false
  不创建 socket；bridge 只提供离线工具

token
  app 自动生成/读取 state token；错误 token -> unauthorized
  也可以设置 XTERVO_DIAG_TOKEN override（不写入 state 文件）

MCP 本身无网络监听；单线程同步处理
resources 只有 xtervo://version，不支持 subscribe
无 per-harness token scope、无内置 rate limit；只有参数长度上限
```

## 关掉 MCP

- Settings → Privacy → **Enable MCP** 关闭；或
- `config.toml` 写 `mcp_enabled = false`。

两者都会让 xtervo 删除 live socket；已有 `xtervo diag serve` 进程的
`tools/list` 只剩离线工具，live `tools/call` 返回明确错误。`xtervo diag`
的离线诊断命令不受影响。
