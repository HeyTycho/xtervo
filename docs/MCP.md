# MCP 桥：离线诊断 + live terminal control

> Live socket tools are documented in [`MCP_TERMINAL_CONTROL.md`](MCP_TERMINAL_CONTROL.md).
> Read tools are default; write tools require explicit environment switches and a token.

> 状态：`tervo diag serve` 是本地 MCP stdio bridge，不新增独立 daemon、不监听网络。
> 未配置 live socket 时提供离线诊断；配置 `TERVO_DIAG_SOCKET` 后可读取/操作运行中的
> Tervo。MCP 是宿主内嵌功能，不是插件。完整 CLI/协议细节见
> [`DIAGNOSTICS.md`](DIAGNOSTICS.md#mcp-stdio已实现阶段-a)。

## 目标

让外部 MCP client（Claude Desktop/Code、Codex、其他 agent）在本地显式
spawn 一个只读诊断进程，读取 Tervo 的版本/构建、确定性 doctor 结果和日志
摘要，而不是让 Tervo 自己实现一套 AI 网络栈。

```text
MCP client
  ⇄ stdio JSON-RPC 2.0（每行一个 JSON）
tervo diag serve
  ⇄ 只读文件/路径检查（HOME 或 /tmp）
Tervo 配置、日志、显式提供的 frame/metrics JSON
```

## 启动

```bash
tervo diag serve
```

这是内置 CLI 子命令，不是 Tervo 插件；stdin/stdout 走协议，日志只走
stderr。协议版本为 `2024-11-05`，`serverInfo.name=tervo-diag`。

### Claude Desktop / Claude Code

Claude Code：

```bash
claude mcp add tervo-diag -- tervo diag serve
```

Claude Desktop（`claude_desktop_config.json`）：

```json
{
  "mcpServers": {
    "tervo": {
      "command": "/path/to/tervo",
      "args": ["diag", "serve"]
    }
  }
}
```

### Codex CLI（`~/.codex/config.toml`）

```toml
[mcp_servers.tervo_diag]
command = "tervo"
args = ["diag", "serve"]
```

### 其他 harness

按 MCP stdio 标准发送 `initialize` → `notifications/initialized` →
`tools/list` → `tools/call`。没有 MCP 的 harness 继续用
`tervo diag env/snapshot --json`。

## 工具

| 工具 | 输入 | 输出 | 说明 |
| --- | --- | --- | --- |
| `tervo.diag_env` | 无 | 版本/display/config/log 路径 | 不返回环境变量值 |
| `tervo.diag_snapshot` | `command` 等 | frame/cursor/images | **唯一**会启动一次性 PTY/shell 的工具 |
| `tervo.diag_replay` | `input` 文件 | frame | 通过 PTY 回放原始字节 |
| `tervo.doctor_check` | `environment`/`frame`/`metrics`（object 或 HOME//tmp 下路径）、`symptom` 等 | `tervo.diagnostic_case.v1` JSON | 确定性、离线、只读、无 shell |
| `tervo.issue_draft` | `case`（完整 DiagnosticCase） | `markdown` + title/labels | 固定 doctor issue 模板，无模型/网络 |
| `tervo.metrics_read` | `log_dir?` | 文件/字节/行/错误计数 | 只统计 `tervo-*.log` 尾部 256 KiB，不返回原始行 |

已有 live control socket 时，`tools/list` 还可返回
`tervo.live_capabilities` / `tervo.live_snapshot`；它们仍只读。

### 资源

```text
tervo://version  版本/commit/build/target/display 元数据（application/json）
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
  "name":"tervo.doctor_check","arguments":{
    "environment":{"schema":"tervo.diag.v1","kind":"env","version":"0.3.1"},
    "symptom":"滚动后残影"
  }
}}
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{
  "name":"tervo.issue_draft","arguments":{"case": <上一步输出的 case>}
}}
```

`tervo.doctor_check` 输出可直接原样作为 `tervo.issue_draft` 的 `case`。
`frame` 只接收显式提供的视口摘要（doctor 内部上限 40×200），不读取
scrollback。

## 安全边界

```text
- doctor/issue/metrics 只读：不启动进程、不写文件、不访问网络
- 文件路径必须在 $HOME 或 /tmp 下；canonicalize 后校验，拒绝 symlink 逃逸
- 单个 JSON 输入文件上限 1 MiB；MCP 单行请求上限 1 MiB
- metrics 只枚举普通文件 tervo-*.log，最多 8 个、每个尾部 256 KiB；
  只输出计数和文件名，不输出日志正文
- 不读 Vault/API key/环境变量值；不整段读取 scrollback
- doctor 对配置中的 secret-like 内容只输出规则计数，不输出值
- 仅本地 stdio，无网络监听
- snapshot/replay 保留原有“本地诊断进程”行为；新工具不调用它们
```

## 模式与限制

```text
离线模式（无 --socket）
  只读 diag_env/diag_snapshot/diag_replay/doctor_check/issue_draft/metrics_read
  diag_snapshot 会启动一次性 PTY 执行调用者给的 command

live 模式（--socket）
  live_capabilities/snapshot/list/output 默认可用
  live_send_text/send_key/run/focus/new_tab/split/scroll 需要写开关
  close_pane/close_tab/kill 需要破坏性开关
  live_metrics/paste/resize/rename 当前未暴露

MCP 本身无网络监听
token 在配置 live socket 时用于鉴权；不配置 token 只能依赖 Unix socket 0600
单线程同步处理
resources 只有 tervo://version，不支持 subscribe
```

后续若要读取运行中的 pane，需要显式启用 `TERVO_DIAG_SOCKET` 并补权限/限速
设计；写能力和任意文件访问不在 M0 范围内。
