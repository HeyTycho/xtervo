# MCP 只读桥（设计，未在 0.2.0 提供）

> 状态：设计冻结；不随 0.2.0 发布。
> 原则：默认只读、显式授权、绝不把终端内容/凭据写成日志。

## 目标

让外部 MCP client（Claude Desktop、Codex、其他 agent）读取 Tervo 的终端状态，
而不是让 Tervo 自己实现一套 AI 网络栈。

```text
MCP client
  ⇄ stdio JSON-RPC
tervo-mcp（独立小进程）
  ⇄ 本地 Unix socket / JSON Lines
Tervo（显式启用 read-only bridge）
```

## 工具（v0）

```text
tervo.list_panes   → id/title/kind/cwd(可选)/是否活跃
tervo.read_pane    → pane_id + 最近 N 行文本（去 ANSI，限长）
tervo.search_pane  → pane_id + query（只读，复用现有搜索）
tervo.capabilities → 版本/只读/限制
```

不做：

```text
write/input/paste
send keys
执行命令
读取 Vault/API key/环境变量
任意文件读写
```

## 安全

- 默认关闭；必须用户在 Settings 显式 enable。
- socket 权限 0600，路径在 `$XDG_RUNTIME_DIR/tervo/mcp.sock`。
- 每次连接需一次性 token（显示在 Settings，可撤销）。
- 输出截断/限速；不记录内容，只记录字节数/错误码。
- 明确标注“读取终端内容可能包含敏感信息”。

## 为什么不放入 0.2.0

- 需要跨进程控制通道，现有插件 API 不暴露 UI/会话状态。
- 写入/命令能力风险高，必须先做只读和权限确认。
- 当前优先级是图片合成与 process plugin AI Chat 稳定。

## 后续验收

```text
MCP initialize/tools/list/tools/call
只读工具真实读取 Xvfb 会话
无 token/错误 token 拒绝
socket 权限/关闭后不可连接
2m soak；无新增线程/FD 泄漏
```
