# Tabby 插件生态参考笔记（不追求兼容）

> 状态：参考笔记（2026-11-09 调研；2026-11-24 调整定位：参考，不是兼容路线）
> 目的：了解 Tabby 热门插件解决什么问题、哪些用户价值值得借鉴。
> **不加载 Tabby 的 npm 插件，也不以兼容或替代 Tabby 为目标。**
> 相关：`docs/PLUGIN_API.md`、`docs/PLUGIN_SYSTEM.md`、`docs/TABBY_UPSTREAM_AUDIT.md`。

## 0. 结论

### 不能做的

**不能在 xtervo 里直接加载 Tabby 的 npm 插件。**

Tabby 插件是：

```text
npm 包
→ default export NgModule
→ 依赖 tabby-core/tabby-terminal 的 Angular DI、DOM、services
→ 运行在 Electron renderer
```

xtervo 是 Rust + Slint + 原生终端，没有 Angular/Electron/DOM，也没有 Tabby 的 service API。
直接兼容等于把整个 Tabby renderer 运行时塞进来，体积、性能、安全都不划算。

### 可借鉴的

**按用户价值借鉴，用 xtervo 自己的机制实现。**

按插件实际做的事，而不是按它的 JS 代码：

| 借鉴方向 | xtervo 的做法 |
|---|---|
| 原生功能 | SSH/SFTP、分屏、搜索、主题、关闭确认等直接做进 xtervo |
| Process Plugin | 外部进程 JSON Lines：命令、状态栏、主题、事件 |
| ACP/MCP bridge | 让外部 AI/工具通过协议控制 xtervo 或读取上下文 |
| AI panel | 内置 OpenAI-compatible 聊天/命令面板，覆盖 `tabby-ai-chat` 这类插件的使用场景 |
| Declarative theme | `theme.toml` 主题包，可直接复用颜色 |

如果只挑用户价值（不是兼容清单）：

> **参考前 10 个热门插件里真正有用的用法，在 xtervo 里给出自己的入口；不加载它们的 JS，也不承诺行为一致。**

---

## 1. 可见度较高的 Tabby 插件/类别

> 说明：npm/GitHub 没有稳定公开的“Tabby 插件下载量 Top 10”榜单；以下是按公开包名、社区文章和功能热度归类的**可见度较高类别**，不冒充精确排名。

| # | 插件/类别 | 类型 | AI | 代表包/链接 | xtervo 参考判断 |
|---:|---|---|---|---|---|
| 1 | AI Chat（终端旁 LLM 对话） | UI panel | ✅ | [`tabby-ai-chat`](https://github.com/ephb/tabby-ai-chat) | 需内置 AI panel；用法可借鉴 |
| 2 | AI Assistant / 命令提示 | UI + command | ✅ | [`tabby-ai-assistant`](https://www.npmjs.com/package/tabby-ai-assistant) | 需 AI command provider + 输入框 |
| 3 | AI Agent（agent 会话） | UI/agent | ✅ | [`tabby-ai-agent`](https://www.npmjs.com/package/tabby-ai-agent) | 先接 CLI agent/MCP，不重做 agent |
| 4 | MCP 控制桥 | server/bridge | ✅ | [`Tabby-MCP`](https://github.com/GentlemanHu/Tabby-MCP)、[`tabby-mcp`](https://www.npmjs.com/package/tabby-mcp) | 做 xtervo MCP server/client，高价值 |
| 5 | 命令提示/补全 | command | ⚠️ | [`tabby-command-tips`](https://www.npmjs.com/package/tabby-command-tips) | 可先做命令 provider |
| 6 | Quick Commands 提示 | command/UI | ⚠️ | [`tabby-quick-cmds-hint`](https://www.npmjs.com/package/tabby-quick-cmds-hint) | 可做命令面板/状态栏入口 |
| 7 | Clipboard 历史/增强 | UI/service | ❌ | [`tabby-clippy`](https://www.npmjs.com/package/tabby-clippy) | 需 clipboard history；后续 |
| 8 | 主题包 | declarative | ❌ | 社区主题包（Catppuccin/Tokyo Night 等） | 已支持主题目录/Theme Pack |
| 9 | Zmodem 文件传输 | terminal protocol | ❌ | `tabby-plugin-zmodem` 类 | 需 zmodem 协议；后续 |
| 10 | 保存输出/会话记录 | command/service | ❌ | save-output/workspace 类 | 已有 Save Contents/会话恢复，部分等价 |
| 附 | SSH/SFTP/连接管理 | native | ❌ | Tabby 内置/插件 | xtervo 已原生支持 |
| 附 | Quake mode / 全局热键 | native | ❌ | Tabby 内置 | xtervo 已支持 F12 quake |
| 附 | Settings/配置同步 | service | ❌ | Tabby Sync/社区 sync | 未做云端同步；可做本地导出 |

### 1.1 AI 插件拆解

AI 相关插件大致是三件事：

```text
1. Chat UI（侧栏/底部面板）
2. 命令建议/解释（inline）
3. 工具调用/agent（读写终端、文件、MCP）
```

xtervo 不应该直接复刻三个不同插件的 JS，而应该提供：

```text
AI panel（OpenAI-compatible endpoint）
+ command provider（可输入 prompt）
+ MCP client/server（工具调用）
```

这样 `tabby-ai-chat` / `tabby-ai-assistant` 的核心用法可以覆盖，且不依赖 Electron。

---

## 2. 实现策略（自有机制）

### 2.1 原生优先

以下已经在 xtervo 核心里，优先级高于做插件兼容：

```text
SSH/SFTP
分屏/Tab
搜索/复制/粘贴
主题/背景/透明
Quake mode
保存输出
```

### 2.2 Process Plugin V1

已实现：

```text
plugin.toml + runtime = "process"
Command Provider（静态 args，文本结果）
Status Provider（2s 轮询/推送）
Theme Pack
startup/tab-open/tab-close 事件
```

限制：

```text
无动态输入/交互
无 UI 扩展
无沙箱/权限强制
无热重载/安装 UI
```

可立即支持的场景：

- Quick Commands
- Git/Docker/构建状态
- 主题包
- 简单命令集成

### 2.3 AI Panel（今晚重点）

新增内置 AI Chat 面板（不是 Tabby 插件运行时）：

```text
右侧/底部面板
输入 prompt → OpenAI-compatible /v1/chat/completions
流式输出
复制/插入命令
可选模型/base_url/api_key（Vault）
```

先做 MVP，不承诺：

```text
多模态
工具调用/自动执行
会话持久化
多 provider 管理
```

### 2.4 MCP bridge（AI 工具接入）

优先级高于“复刻更多 UI 插件”：

- 让外部 agent/CLI 通过 MCP 读取 xtervo 当前 Tab/屏幕/选中文本；
- 让 MCP 工具打开 Tab、输入命令、读取输出；
- 权限默认只读，写操作需显式开启；
- 参考 `Tabby-MCP` 的“控制终端”用法，但不复用其 Electron API。

### 2.5 不做的兼容

```text
Tabby JS/Angular 插件直接加载
Electron renderer 嵌入
Tabby service API 全量模拟
任意 npm 插件安装即执行
```

---

## 3. 借鉴清单（不是兼容目标）

| 借鉴项 | 当时（11-09） | 后续 |
|---|---|---|
| AI Chat panel | MVP | 流式/历史/错误恢复 |
| AI command suggestions | 先命令入口 | inline 提示/解释 |
| MCP | 只读桥 | 写操作/工具权限 |
| Quick commands | 已有 | 动态参数/输入 |
| Theme packs | 已有 | 主题市场格式 |
| Clipboard history | 不做 | 独立功能 |
| Zmodem | 不做 | 协议适配 |
| Sync | 不做 | 本地/Gist |
| Save output | 已有部分 | 完整会话导出 |
| SSH/SFTP | 已有 | 继续打磨 |

结论：

```text
用户价值层面能覆盖 6–7/10；
直接加载插件 0/10（也不打算做）。
```

---

## 4. 当时的执行顺序（2026-11-09 历史记录）

### 18:00–18:30 冻结

- 确认 AI panel MVP 范围
- 确认 endpoint/config/Vault 读取方式
- 确认 MCP 只读工具清单

### 18:30–21:00 AI panel MVP

- Slint 面板 + 输入框 + 流式文本
- Rust HTTP client（复用现有依赖，不引入重型框架）
- 配置：base_url/model/api_key（key 不写明文）
- 错误/超时/取消
- 截图 + 手动测试

### 21:00–22:30 MCP 只读桥

- JSON-RPC over stdio
- 工具：list tabs、active tab、visible screen、selection
- 默认只读；写操作留开关
- 文档 + e2e

### 22:30–23:30 插件样例/兼容验证

- quick-commands/git-status 进程插件样例
- 对照 `tabby-command-tips` / `tabby-quick-cmds-hint` 的用户路径
- 内存 2m 门禁
- 更新 README/PLUGIN_API/兼容矩阵

---

## 5. 风险

- AI panel 涉及网络/密钥/流式 UI，不能把 key 写日志/明文配置。
- MCP 写操作风险高，默认只读。
- Tabby AI 插件各自 endpoint/协议不同，只能做 OpenAI-compatible 主路径，不保证每家一致。
- 不用“兼容 Tabby 插件”做宣传；准确说法是“参考其用户价值，提供自己的迁移/互操作说明”。

---

## 6. 验收

- [ ] AI panel 可配置 OpenAI-compatible endpoint
- [ ] 流式输出/取消/超时/错误可见
- [ ] API key 不落明文日志
- [ ] MCP list/read 工具可用，默认只读
- [ ] 命令插件样例可运行
- [ ] 2m 内存门禁 PASS
- [ ] 文档写明“不加载 Tabby JS 插件”

## 8. 当前落地状态（2026-11-09 0.2.0）

- 已落地：Plugin API v2（v1 兼容）、`[contributes.ai]`、`ai.chat` process plugin、Settings AI Chat 面板、离线 echo 样例。
- 边界：不加载 Tabby Electron/Angular JS 插件；AI 能力由插件自己持有凭据/网络，宿主不存 API Key。
- 未完成：插件安装市场/签名/权限沙箱/热重载；MCP 只读桥仍在设计，不在 0.2.0。
- 验收：`scripts/e2e_ai_panel.sh`、`scripts/e2e_plugin_v1.sh`、app 393 tests。
