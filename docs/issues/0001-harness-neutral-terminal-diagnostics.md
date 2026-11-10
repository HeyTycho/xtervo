# feat: harness-neutral terminal diagnostics (MCP + CLI) for Codex / Claude Code / DSH

## 背景

Tervo 的终端问题经常只在特定 TUI/harness 下出现：

- Sixel/IIP 图片位置、裁剪、ECH 擦除；
- cell 像素尺寸查询（`CSI 16t/14t`）导致的低清/错位；
- 滚动 offset 下的 cell/image 对齐；
- 字体 fallback、块元素、宽字符、IME、resize。

这些不能只靠“截一张图”定位：需要同时看到 **PTY 原始序列、文本 frame、cell 属性、光标/模式、图片 rect、像素尺寸、日志**。
不同 harness（DSH / Codex CLI / Claude Code / 其他）各有自己的 MCP/工具机制，为每家写适配器不可维护。

## 提案

提供一个 **harness-neutral、只读优先的终端诊断面**：

1. **标准 MCP stdio server**（任何支持 MCP 的 harness 都能启动/连接）
2. **CLI 兜底**（不支持 MCP 的 harness 也能在 shell 里调用）
3. **统一 trace schema v1**（JSONL；可回放）

### 1. MCP tools

```text
tervo.capabilities     版本、只读、限制、截图/trace 能力
tervo.list_panes       pane id/title/kind/active/尺寸
tervo.read_pane        文本、光标、模式、scroll offset（限长）
tervo.snapshot         文本 + cell 属性摘要 + cursor/modes/scroll
tervo.screenshot       PNG（MCP image content；仅显式请求）
tervo.image_info       image id/row/col/width/height/协议
tervo.get_trace        JSONL 事件（PTY/escape/frame/image/damage）
tervo.search_pane      只读搜索
tervo.replay_trace     无 UI 回放 trace，输出 frame/图片断言
```

第一版 **不做** input/paste/send-key/执行命令，避免把“诊断”变成“远程控制”。

### 2. CLI 兜底

```bash
tervo diag --json                     # 环境、版本、配置、日志路径、pane 列表
tervo diag snapshot --pane 0 --json   # frame/cursor/modes/scroll
tervo diag screenshot --out shot.png
tervo diag trace --out trace.jsonl    # 限时/限大小
tervo diag replay trace.jsonl --json  # 确定性回放
```

现有 `tervo --diagnose` 只打印环境/最近日志，扩展它而不是再造一套。

### 3. Trace schema v1

```json
{"v":1,"seq":42,"ts_ms":...,"pane":0,"kind":"pty","bytes_b64":"..."}
{"v":1,"seq":43,"ts_ms":...,"pane":0,"kind":"frame","cols":120,"rows":32,
 "cursor":{"row":10,"col":4,"visible":true,"shape":"block"},
 "modes":{"alt":false,"mouse":"1002","bracketed_paste":true},
 "scroll":{"offset":3,"history":72},
 "images":[{"id":1,"row":16,"col":0,"w":300,"h":96,"protocol":"sixel"}]}
{"v":1,"seq":44,"kind":"damage","rows":[16,17]}
{"v":1,"seq":45,"kind":"screenshot","path":"...","sha256":"..."}
```

要求：

- 默认 **不记录** Vault/环境变量/命令参数；raw PTY 必须显式 `--raw`。
- 每行限长；trace 总大小/时长有上限；敏感字段可脱敏。
- 回放用同一 Rust core，输出 frame/图片 rect 供断言。

## 兼容策略

不做 per-harness 插件，做“标准 + 文档 + 配置片段”：

| Harness | 接入 | 说明 |
|---|---|---|
| Claude Code | MCP stdio | `.mcp.json` / `claude mcp add` 配置片段 |
| Codex CLI | MCP stdio | `~/.codex/config.toml` MCP 配置片段 |
| DSH / dsh-tui | MCP stdio | 配置片段；不依赖 dsh 私有插件 |
| 无 MCP 的 harness | CLI | `tervo diag ... --json` 由 harness 调 shell |

每个 harness 只提供“如何启动/授权 Tervo MCP”的文档与 e2e 配置，不复制终端实现。

## 安全

- 默认关闭；Settings 显式启用“诊断接口（只读）”。
- 本地 Unix socket（`$XDG_RUNTIME_DIR/tervo/diag.sock`，0600），一次性 token，可撤销。
- MCP server 只读；无 shell/命令/任意文件访问。
- 截图/trace 可能包含终端内容，UI 明确提示；不自动上传。
- 输出限长、限速率；日志不写 prompt/凭据/终端正文。

## 验收

- [ ] `tervo diag --json` 可在任意 harness 的 shell 内运行
- [ ] MCP `initialize` / `tools/list` / `tools/call` 通过
- [ ] `snapshot` 与当前 frame/cursor/modes/scroll 一致
- [ ] `image_info` 与 Sixel/IIP overlay rect 一致（含滚动 offset）
- [ ] `screenshot` 为真实 pane 像素
- [ ] `trace` 可回放，输出 frame/图片断言
- [ ] Claude Code / Codex / DSH 配置片段可复制
- [ ] 无 token/错 token/socket 权限/关闭后不可连接测试
- [ ] 2m soak；线程/FD 无增长；无 UI 阻塞
- [ ] 文档明确：只读、不是沙箱、可能含敏感终端内容

## 非目标

- 不做远程控制/输入注入/命令执行（后续若做必须单独设计+显式授权）
- 不加载 Tabby Electron/Angular 插件
- 不实现各 harness 的私有插件格式
- 不把诊断接口当安全边界

## 实现阶段

1. `tervo diag` CLI + trace schema + snapshot/replay（无 GUI 可测）
2. app 内只读控制 socket + MCP stdio server
3. harness 配置文档 + e2e（至少 CLI + 一个真实 MCP client）
4. 可选：截图/trace UI 导出

## 相关

- `docs/MCP.md`（只读桥设计）
- `docs/PLUGIN_API.md`（process plugin，不是本 issue 的实现路径）
- 0.2.0 image v2 / XTWINOPS / ECH 行级清理
