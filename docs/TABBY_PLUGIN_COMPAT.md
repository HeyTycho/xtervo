# Tabby 插件生态与 Tervo 兼容路线

> 状态：研究/计划（2026-11-09）
> 目标：回答“Tabby 热门插件有哪些、Tervo 能兼容前 10 个吗、AI 相关怎么接”。
> 相关：`docs/PLUGIN_API.md`、`docs/PLUGIN_SYSTEM.md`、`docs/TABBY_UPSTREAM_AUDIT.md`。

## 0. 结论

### 不能做的

**不能在 Tervo 里直接加载 Tabby 的 npm 插件。**

Tabby 插件是：

```text
npm 包
→ default export NgModule
→ 依赖 tabby-core/tabby-terminal 的 Angular DI、DOM、services
→ 运行在 Electron renderer
```

Tervo 是 Rust + Slint + 原生终端，没有 Angular/Electron/DOM，也没有 Tabby 的 service API。
直接兼容等于把整个 Tabby renderer 运行时塞进来，体积、性能、安全都不划算。

### 能做的

**行为兼容 + 进程/MCP 适配。**

按插件实际做的事，而不是按它的 JS 代码：

| 兼容层 | 做法 |
|---|---|
| 原生功能 | SSH/SFTP、分屏、搜索、主题、关闭确认等直接做进 Tervo |
| Process Plugin | 外部进程 JSON Lines：命令、状态栏、主题、事件 |
| ACP/MCP bridge | 让外部 AI/工具通过协议控制 Tervo 或读取上下文 |
| AI panel | 内置 OpenAI-compatible 聊天/命令面板，替代 `tabby-ai-chat` 这类 UI 插件 |
| Declarative theme | `theme.toml` 主题包，可直接复用颜色 |

因此“兼容前 10”应定义为：

> **前 10 个热门插件的核心用户价值，在 Tervo 里有等价入口/可迁移配置，而不是二进制加载。**

---

## 1. 可见度较高的 Tabby 插件/类别

> 说明：npm/GitHub 没有稳定公开的“Tabby 插件下载量 Top 10”榜单；以下是按公开包名、社区文章和功能热度归类的**可见度较高类别**，不冒充精确排名。

| # | 插件/类别 | 类型 | AI | 代表包/链接 | Tervo 兼容判断 |
|---:|---|---|---|---|---|
| 1 | AI Chat（终端旁 LLM 对话） | UI panel | ✅ | [`tabby-ai-chat`](https://github.com/ephb/tabby-ai-chat) | 需内置 AI panel；行为可兼容 |
| 2 | AI Assistant / 命令提示 | UI + command | ✅ | [`tabby-ai-assistant`](https://www.npmjs.com/package/tabby-ai-assistant) | 需 AI command provider + 输入框 |
| 3 | AI Agent（agent 会话） | UI/agent | ✅ | [`tabby-ai-agent`](https://www.npmjs.com/package/tabby-ai-agent) | 先接 CLI agent/MCP，不重做 agent |
| 4 | MCP 控制桥 | server/bridge | ✅ | [`Tabby-MCP`](https://github.com/GentlemanHu/Tabby-MCP)、[`tabby-mcp`](https://www.npmjs.com/package/tabby-mcp) | 做 Tervo MCP server/client，高价值 |
| 5 | 命令提示/补全 | command | ⚠️ | [`tabby-command-tips`](https://www.npmjs.com/package/tabby-command-tips) | 可先做命令 provider |
| 6 | Quick Commands 提示 | command/UI | ⚠️ | [`tabby-quick-cmds-hint`](https://www.npmjs.com/package/tabby-quick-cmds-hint) | 可做命令面板/状态栏入口 |
| 7 | Clipboard 历史/增强 | UI/service | ❌ | [`tabby-clippy`](https://www.npmjs.com/package/tabby-clippy) | 需 clipboard history；后续 |
| 8 | 主题包 | declarative | ❌ | 社区主题包（Catppuccin/Tokyo Night 等） | 已支持主题目录/Theme Pack |
| 9 | Zmodem 文件传输 | terminal protocol | ❌ | `tabby-plugin-zmodem` 类 | 需 zmodem 协议；后续 |
| 10 | 保存输出/会话记录 | command/service | ❌ | save-output/workspace 类 | 已有 Save Contents/会话恢复，部分等价 |
| 附 | SSH/SFTP/连接管理 | native | ❌ | Tabby 内置/插件 | Tervo 已原生支持 |
| 附 | Quake mode / 全局热键 | native | ❌ | Tabby 内置 | Tervo 已支持 F12 quake |
| 附 | Settings/配置同步 | service | ❌ | Tabby Sync/社区 sync | 未做云端同步；可做本地导出 |

### 1.1 AI 插件拆解

AI 相关插件大致是三件事：

```text
1. Chat UI（侧栏/底部面板）
2. 命令建议/解释（inline）
3. 工具调用/agent（读写终端、文件、MCP）
```

Tervo 不应该直接复刻三个不同插件的 JS，而应该提供：

```text
AI panel（OpenAI-compatible endpoint）
+ command provider（可输入 prompt）
+ MCP client/server（工具调用）
```

这样 `tabby-ai-chat` / `tabby-ai-assistant` 的核心用法可以覆盖，且不依赖 Electron。

---

## 2. 兼容策略

### 2.1 原生优先

以下已经在 Tervo 核心里，优先级高于插件兼容：

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

可立即兼容：

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

### 2.4 MCP bridge（AI 工具兼容）

优先级高于“兼容更多 UI 插件”：

- 让外部 agent/CLI 通过 MCP 读取 Tervo 当前 Tab/屏幕/选中文本；
- 让 MCP 工具打开 Tab、输入命令、读取输出；
- 权限默认只读，写操作需显式开启；
- 与 `Tabby-MCP` 的“控制终端”价值对齐，但不复用其 Electron API。

### 2.5 不做的兼容

```text
Tabby JS/Angular 插件直接加载
Electron renderer 嵌入
Tabby service API 全量模拟
任意 npm 插件安装即执行
```

---

## 3. “前 10”兼容目标

| 目标 | 今晚 | 本月 |
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
行为层面可以覆盖 6–7/10 的核心价值；
直接插件加载 0/10；
AI 相关是今晚最高优先级。
```

---

## 4. 今晚 18:00 后的执行顺序

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
- 不用“兼容 Tabby 插件”做营销；准确说法是“行为兼容/迁移指南”。

---

## 6. 验收

- [ ] AI panel 可配置 OpenAI-compatible endpoint
- [ ] 流式输出/取消/超时/错误可见
- [ ] API key 不落明文日志
- [ ] MCP list/read 工具可用，默认只读
- [ ] 命令插件样例可运行
- [ ] 2m 内存门禁 PASS
- [ ] 文档写明“不加载 Tabby JS 插件”
