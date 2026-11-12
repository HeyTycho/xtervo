# Tervo Plugin API v3

状态：**v3 是 Tervo 唯一受支持的插件协议**。宿主只加载 `api = 3` 且
`host_abi = 3` 的清单；v1/v2 清单**不兼容、不会执行**，设置页只显示
`unsupported` 和迁移提示。代码里没有双协议兼容层。

实现位置：`crates/app/src/plugin_v3/`（`manifest` / `protocol` / `state` /
`supervisor` / `tests`）。旧 `crates/app/src/plugin.rs` 保留为不再编译的
legacy 文件，不注册、不启动。

- 协议：JSON-RPC 2.0，line-delimited stdio（一行一个 UTF-8 JSON）。
- 进程：每个启用的插件一个独立子进程 + 独立 supervisor 线程。
- UI：全部由宿主渲染；插件只能给声明式文档。
- 权限：capability gate + 用户确认，**不是 OS 沙箱**。
- MCP：Tervo 内嵌功能，**不是插件能力**；本协议没有插件 MCP server/代理。

---

## 1. 安全模型（必须如实理解）

插件是普通子进程，**以当前用户身份运行，拥有该用户的全部系统权限**。

宿主能做的是：

1. **首次启用确认**：展示 manifest 声明的权限，用户点 “Allow and enable”
   之前不启动进程；权限集合变化后旧确认失效，必须重新确认。
2. **capability gate**：只对宿主代理的 API 做运行时检查：
   - `terminal.read` → `permissions.terminal_context`，并且**每次**请求都要
     用户 Allow/Deny；
   - `terminal.write` → `permissions.terminal_write`，每次 ask；
   - `clipboard.read` / `clipboard.write` → `permissions.clipboard`，每次 ask。
3. **不提供秘密**：宿主永不返回 Vault 内容、环境变量明文、API Key 或其他
   凭据。`credentials.get` 永远返回 `-32002 unsupported`。插件要用凭据必须
   自己管理（`credentials = "own"` 只是声明）。
4. **环境变量白名单**：spawn 插件时 `env_clear()`，只透传
   `PATH` / `HOME` / `LANG` / `LC_ALL` / `TMPDIR`，再设置
   `TERVO_PLUGIN_ID` / `TERVO_PLUGIN_API` / `TERVO_HOST_VERSION`。
5. **输入/输出上限**：单行 256 KiB；stdout 总量 16 MiB；stderr 尾部 8 KiB；
   超限按协议错误重启/停止。
6. **生命周期隔离**：每个插件独立 supervisor；崩溃、坏 JSON、超时只影响
   该插件。Linux 上子进程设置 `PR_SET_PDEATHSIG=SIGKILL` 且独立 process
   group，Tervo 被 SIGKILL 时不会留下孤儿插件。

**宿主不做的事**：不阻止插件直接读写文件、访问网络、启动子进程、读取它
能访问的环境/配置。`filesystem` / `network` / `process` / `credentials` 权限
只是“声明 + 确认时展示 + 诊断记录”，不是内核级强制。需要强隔离请用容器/
专用用户；Tervo 不提供 OS 沙箱。

---

## 2. 目录与发现

插件目录默认：

```text
$XDG_CONFIG_HOME/tervo/plugins/          # 或 ~/.config/tervo/plugins/
```

可用 `TERVO_PLUGINS=/path/to/plugins` 覆盖。每个 v3 插件必须是子目录：

```text
<plugins>/<dir>/
  plugin.toml      # 必需
  main.py          # entry（可执行文件/脚本；必须在插件目录内）
```

发现规则：

- 子目录里有 `plugin.toml`：读取并严格解析。
- 目录顶层 `*.toml` 旧扁平清单：显示为 `unsupported`，永不执行。
- `api = 1/2` 或缺少 `api`：显示 `unsupported`，原因包含迁移提示。
- `api = 3` 但字段/`host_abi`/`entry` 非法：显示 `invalid` + 原因；永不执行。
- 重复 `id`：第一个保留，其余标记 invalid。
- `entry` / `theme` 只允许插件目录内相对路径；拒绝绝对路径、`..`、反斜杠；
  canonicalize 后必须仍在插件目录内（防符号链接逃逸）。`entry` 额外要求
  普通文件 + Unix 执行位。

启用状态与权限确认持久化在：

```text
<config dir>/plugins-v3-state.toml
```

只保存 `id` / `enabled` / 权限指纹（`permissions_approved` +
`permissions = [...]`），不保存凭据、路径或环境变量。状态文件损坏时记录
日志并按空状态启动，不删除原文件。

---

## 3. Manifest

完整示例（`docs/examples/plugin-v3-echo/plugin.toml`）：

```toml
api = 3
host_abi = 3
id = "dev.tervo.echo"
name = "Echo (v3 example)"
version = "1.0.0"
runtime = "process"          # v3 只支持 process；oneshot 属于已移除的 v1
entry = "main.py"
description = "Reference plugin"

[permissions]
filesystem = "plugin"        # none | plugin | home（声明；不是强制）
network = false              # 声明
process = false              # 声明
clipboard = false            # gate clipboard.read/write
terminal_context = true      # gate terminal.read（每次还要 ask）
terminal_write = false       # gate terminal.write（每次还要 ask）
credentials = "own"          # none | own（宿主永不提供凭据明文）

[contributes]
commands = true              # 有 [[commands]] 时必须 true
status = true                # 允许 status.get + ui.status
events = ["startup", "tab-open", "tab-close", "pane-focus"]
theme = "theme.toml"         # 可选；启动时加载，运行中不热换
panels = ["echo"]            # 声明式面板 ID
settings = true              # 允许 surface.render(settings)
assistant = false            # bool 或 [contributes.assistant] 表；仅占位

[[commands]]
id = "greet"
title = "Echo: Greet"
description = "Return a hello message"

[[commands]]
id = "panel"
title = "Echo: Render panel"
```

### 3.1 顶层字段

| 字段 | 必需 | 规则 |
|---|---|---|
| `api` | 是 | 必须 `3`；1/2 直接 unsupported |
| `host_abi` | 是 | 必须 `3` |
| `id` | 是 | `[A-Za-z0-9]` 开头，只允许 `A-Za-z0-9._-`，≤64 字符，全局唯一 |
| `name` | 是 | 非空，≤128 字符 |
| `version` | 是 | 非空，≤64 字符 |
| `runtime` | 是 | 必须 `"process"` |
| `entry` | 是 | 插件目录内相对路径；可执行 |
| `description` | 否 | ≤512 字符 |
| `permissions` | 否 | 见上 |
| `contributes` | 否 | 见上 |
| `commands` | 否 | 每个 `id`/`title` 必需，`description` 可选；id 唯一 |

未知字段会导致整份清单 `invalid`（严格模式，防止拼写错误静默生效）。
命令完整 ID 是 `plugin.<plugin id>.<command id>`，例如
`plugin.dev.tervo.echo.greet`。

### 3.2 `assistant` 占位

`assistant` 支持两种写法：

```toml
assistant = false
assistant = true
# 或
[contributes.assistant]
name = "Optional display name"
description = "Placeholder metadata only"
```

宿主只做校验/展示。**v3 MVP 不实现 AI provider、不启动 AI 请求、不转发
API Key**。声明 assistant 不会让插件获得任何 AI 能力。

### 3.3 主题

`contributes.theme` 是插件目录内相对 TOML 主题。启用且权限已确认的插件
主题会在 Tervo 启动时并入主题列表；enable/reload 后不会在运行中热换主题
（重启 Tervo 才生效）。主题缺失只记录 `theme` 错误，不阻止命令运行。

---

## 4. JSON-RPC 2.0 生命周期

### 4.1 传输

- stdin/stdout 各一行一个 JSON 对象；必须以 `\n` 结束。
- 空行忽略。
- 单条消息上限 **256 KiB**；超长行计坏行并截断。
- `jsonrpc` 必须是 `"2.0"`。
- Request 必须有 `method` + `id`（数字或字符串）；
  notification 只有 `method`；response 必须回显宿主分配的**数字 id**。
- Request/notification 不得包含 `result`/`error`。
- 坏 JSON / 坏结构 / 超长行累计到 **5** 次（可测试配置）后 supervisor 重启
  插件；超过自动重启上限后 `Failed`。

### 4.2 启动握手

宿主 spawn 后立即发送：

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{
  "protocol_version":3,
  "host":{"name":"tervo","version":"0.3.1","abi":3},
  "plugin":{"id":"dev.tervo.echo","name":"Echo (v3 example)"},
  "permissions":{
    "granted":["filesystem=plugin","terminal_context","credentials=own"],
    "terminal_read":"ask",
    "terminal_write":"ask",
    "clipboard":"denied",
    "credentials":"never-host-provided"
  },
  "contributes":{
    "commands":["greet","panel"],
    "status":true,
    "events":["startup","tab-open"],
    "panels":["echo"],
    "settings":true,
    "assistant":false
  }
}}
```

插件必须返回：

```json
{"jsonrpc":"2.0","id":1,"result":{"protocol_version":3}}
```

`protocol_version` 必须是 `3`；否则握手失败。宿主随后发：

```json
{"jsonrpc":"2.0","method":"initialized","params":{}}
```

握手超时默认 **5s**。超时/错误按崩溃处理：退避重启，最多 3 次。

### 4.3 宿主 → 插件方法

| method | 类型 | 参数 | 结果 |
|---|---|---|---|
| `initialize` | request | 见上 | `{protocol_version:3}` |
| `initialized` | notification | `{}` | 无 |
| `command.invoke` | request | `{command, args:[], context:{host_version,api}}` | 任意 JSON；推荐 `{ok, message}` |
| `event.deliver` | notification | `{event, payload}` | 无 |
| `status.get` | request | `{}` | `{text, tooltip?, command?}` |
| `ping` | request | `{seq}` | 任意 JSON |
| `surface.render` | request | `{surface:"panel"\|"settings", panel?}` | 声明式 UI 文档 |
| `ui.action` | notification | `{surface, panel?, element, action, value?}` | 无 |
| `shutdown` | request | `{}` | 任意 JSON；随后宿主关闭进程 |

`command.invoke` 超时默认 **10s**，超时上报 `timeout` 并重启插件进程。
`status.get` 每 **2s** 一次，超时 **2s**；状态超时只丢弃本次结果，不杀进程。
`ping` 每 **30s** 一次，超时 **5s**；连续 2 次 ping 超时会重启。
`surface.render` 超时 **5s**。

`status.get` / `ui.status` 结果：

```json
{"text":"Echo v3 ready","tooltip":"optional","command":"greet"}
```

`text` ≤200 字符，`tooltip` ≤512，`command` 必须是 manifest 中声明的命令
短 ID；否则拒绝。状态栏最多显示每插件一行，多个插件用 ` · ` 合并。

`shutdown`：宿主最多等 **1s**，不退出则 SIGKILL 整个 process group。

### 4.4 插件 → 宿主方法

插件可以发 request（带 id）或 notification。宿主当前要求插件 → 宿主 request
使用**数字 id**（字符串 id 会收到 `-32600`）；notification 没有 id。需要异步
ask 的方法只能是 request（不能是 notification）。

| method | 类型 | 能力/确认 | 返回 |
|---|---|---|---|
| `ui.notify` | request/notification | 无（限速 20 次/10s，≤500 字符） | `{}` |
| `ui.status` | request/notification | `contributes.status=true` | `{}` |
| `ui.render` | request | 对应 `panels`/`settings` 声明 | `{accepted:true}` |
| `context.version` | request | 无 | `{api,host_abi,protocol_version,jsonrpc,host_version,transport}` |
| `context.config_metadata` | request | 无（只读非敏感字段） | 见下 |
| `terminal.read` | request | `terminal_context` + 每次 ask | `{text,truncated,max_lines,max_bytes}` |
| `terminal.write` | request | `terminal_write` + 每次 ask | `{}` |
| `clipboard.read` | request | `clipboard` + 每次 ask | `{text}` |
| `clipboard.write` | request | `clipboard` + 每次 ask | `{}` |
| `credentials.get` | request | 永不 | 总是 `-32002 unsupported` |

`context.config_metadata` 只返回：

```json
{"theme":"tokyo-night","language":"zh","font_size":13.0,"tabbar_position":"top","plugin_api":3}
```

不包含环境变量、Vault、Profile 密码、cwd、命令行或文件内容。

`terminal.read` 参数会被 clamp：

```json
{"max_lines":80,"max_bytes":32768}
```

`max_lines` 1..500，`max_bytes` 1..65536；宿主只返回**焦点终端**的尾部文本，
不含 cwd/env/Vault。每次请求都会在 UI 显示 Allow once / Deny 卡片；默认
**60s** 未响应则超时拒绝。

`terminal.write` 的 `text` ≤4096 字节；确认卡片显示转义后的预览。Allow 后
文本被送入焦点终端输入（等同于用户键入，可能包含换行）。

`clipboard.write` 的 `text` ≤64K 字符；`clipboard.read` 只返回文本，不返回
图片。

错误响应示例：

```json
{"jsonrpc":"2.0","id":42,"error":{
  "code":-32001,
  "message":"缺少 permissions.terminal_context（capability gate）"
}}
```

### 4.5 错误码

| code | 含义 |
|---|---|
| `-32700` | JSON parse error（宿主主要计坏行） |
| `-32600` | invalid request |
| `-32601` | method not found |
| `-32602` | invalid params |
| `-32603` | internal error |
| `-32001` | permission denied（capability gate） |
| `-32002` | unsupported |
| `-32003` | denied by user |
| `-32004` | unavailable（无焦点终端/剪贴板/进程已退出） |
| `-32005` | rate limited |
| `-32006` | permission not approved |
| `-32007` | request timeout（ask 超时等） |

---

## 5. 权限、确认与运行时 gate

规范化权限集合（顺序固定）由 manifest 生成：

```text
filesystem=none|plugin|home
network
process
clipboard
terminal_context
terminal_write
credentials=none|own
```

首次 Enable：

1. 设置页显示插件声明的权限 + “不是 OS 沙箱”警告；
2. 用户点 **Allow and enable** 后写入权限指纹并启动；
3. 点 Cancel 不改变状态。

运行时：

- `terminal.read` 无 `terminal_context` → `-32001`，活动日志记录 `denied`。
- `terminal.write` 无 `terminal_write` → `-32001`。
- clipboard 请求无 `clipboard` → `-32001`。
- 有权限但每次 terminal/clipboard 请求仍需用户 Allow；Deny →
  `-32003 denied by user`。
- manifest 权限变化 → reload 停止旧进程，旧确认失效，状态显示
  `permission confirmation required`，重新 Enable/确认后才运行。
- 所有拒绝写日志（`[plugin-v3] ... 拒绝 <method>`）和活动列表；不写敏感
  参数正文。

---

## 6. 事件

清单在 `contributes.events` 声明；宿主只向已启用 + 权限已确认 + 运行中的
插件投递。`startup` 在启动早期可能排队到握手完成后投递（避免丢失）。

| event | payload |
|---|---|
| `startup` | `{}` |
| `tab-open` | `{"title":"...","kind":"local"}` |
| `tab-close` | `{"title":"..."}` |
| `pane-focus` | `{"pane":<u64 opaque id>}` |

标题清洗到 200 字符；`kind` 只允许短 ASCII 标识（否则回退 `local`）；不含
cwd、命令行、环境变量、终端内容或凭据。事件是 notification，不等插件回复。

---

## 7. 声明式面板 / settings

插件**不能**提供 Slint/WASM/HTML/任意布局；只能返回固定 schema：

```json
{
  "version": 1,
  "title": "Echo settings",
  "elements": [
    {"type":"text","text":"..."},
    {"type":"markdown","text":"## Title\n**bold**"},
    {"type":"list","items":["a","b"]},
    {"type":"input","id":"name","label":"Name","value":"v","placeholder":"..."},
    {"type":"button","id":"save","label":"Save"},
    {"type":"select","id":"mode","label":"Mode","value":"a","options":["a","b"]},
    {"type":"progress","label":"Progress","value":0.5}
  ]
}
```

限制：

- 最多 100 个元素；
- 标题 ≤120，text/markdown ≤4000 字符；
- list ≤50 项、单项 ≤500；
- input value ≤1000，placeholder ≤120；
- button label ≤120；
- select 1..32 个选项，value 必须在 options 内；
- progress `0..=1`；
- 交互元素 id 全局唯一，只允许 `[A-Za-z0-9._-]`，≤64；
- 未知 type 或超限 → 整份文档拒绝，界面显示错误。

宿主渲染：

- `text`：纯文本换行；
- `markdown`：宿主做极简纯文本渲染（去标题/粗体/反引号，链接显示为
  `text (url)`），**不解析 HTML、不执行链接、不加载远程图片**；
- `list`：多行纯文本；
- `input`：宿主 TextInput；`edited` 发 `ui.action`（`action:"change"`）；
- `button`：发 `ui.action`（`action:"click"`）；
- `select`：宿主从 options 计算下一个值后发 `ui.action`（`action:"change"`）；
- `progress`：宿主进度条。

打开面板/设置页：

- 设置页每行有 `Panel…`（声明了 `panels` 时）和 `Settings…`（声明了
  `settings` 且已启用确认时）；
- 宿主发 `surface.render` 请求，结果进入当前打开的卡片；
- 插件可随时用 `ui.render` request 推送更新（只接受当前已打开的
  `(plugin, surface)`）；
- UI 动作走 `ui.action` notification，插件可回 `ui.render` 更新。

当前 MVP 限制：多个 panel 只显示第一个 panel 的入口；要打开其它 panel 需要
后续 UI 扩展。

---

## 8. 设置页 Plugins

每行显示：

- `name` / `version` / `api`
- `status`：`running` / `starting` / `restarting` / `stopped` / `failed` /
  `disabled` / `permission confirmation required` / `unsupported` / `invalid`
- `error`（崩溃原因、stderr 尾部、权限变化的提示）
- `permissions`（规范化摘要；明确是声明/capability，不是沙箱）
- `restart_count`
- `source` / `signature`：本地安装来源与实时 Ed25519 校验状态
- 操作：`[Enable]` `[Disable]` `[Reload]` `[Restart]` `[Update…]` /
  `[Rollback to <version>]` / `[Open folder]`（启用/权限确认后额外显示
  `[Panel…]` / `[Settings…]`）
- 顶部：本地安装路径 + `[Choose file…]` / `[Choose folder…]` / `[Install]`，
  以及 “Require Ed25519 signature” 开关

本地安装/更新/回滚、签名格式、安全限制见
[`docs/PLUGIN_INSTALL.md`](PLUGIN_INSTALL.md)。安装器不执行插件代码；权限确认
仍由本节描述的 v3 host 负责。

v1/v2 行：`unsupported`，`api` 显示原版本，`error` 说明
“Unsupported Plugin API vX; Tervo v3 only loads api = 3 / host_abi = 3 …”，
`Open folder` 仍可用，但不会执行任何旧插件。

所有操作**不重启 Tervo**：

- Enable：确认后启动 supervisor；
- Disable：关闭进程，保留权限确认；再次 Enable 不再询问（权限未变时）；
- Reload：重新读盘、校验；权限变化会停止并要求重新确认；
- Restart：手动重启 supervisor，重置自动重启计数；
- Open folder：`xdg-open` / `open` / `explorer`。

---

## 9. Supervisor 行为

生产默认值：

| 项 | 默认 |
|---|---|
| 握手超时 | 5s |
| 命令超时 | 10s（超时 kill/restart） |
| status 轮询/超时 | 2s / 2s（超时只丢弃） |
| ping 轮询/超时 | 30s / 5s（连续 2 次失败 restart） |
| surface 超时 | 5s |
| ask 超时 | 60s |
| 自动重启 | 3 次；backoff 200ms → 400ms → 800ms（上限 2s） |
| 坏行上限 | 5 |
| 单行上限 | 256 KiB |
| stdout 总量 | 16 MiB |
| stderr 尾部 | 8 KiB |

失败处理：

- spawn 失败 → `Failed`，不自动重试（可 Restart/Reload）；
- 握手失败 / 进程退出 / 坏 JSON 超限 / 命令超时 / 输出超限 → 自动重启；
- 超过重启上限 → `Failed` + 原因，命令立即 `unavailable`；
- 任一插件故障不影响终端或其它插件；
- reload/restart/disable 由 supervisor 消息驱动，不 restart Tervo。

---

## 10. 示例插件

`docs/examples/plugin-v3-echo/`：

```bash
mkdir -p ~/.config/tervo/plugins
cp -r docs/examples/plugin-v3-echo ~/.config/tervo/plugins/echo
chmod +x ~/.config/tervo/plugins/echo/main.py
# 启动 Tervo → Settings → Plugins → Enable → Allow and enable
```

可用命令（Palette）：

- `Echo: Greet`
- `Echo: Host context`（`context.version`）
- `Echo: Read terminal tail`（`terminal.read` ask/allow）
- `Echo: Render panel`（声明式 panel）
- `Echo: Notify`（`ui.notify`）

例子还实现 `status.get`、`ui.status`、`surface.render(settings)`、
`ui.action` 和事件日志（`events.txt`）。它只用 Python 标准库。

也可以把 `TERVO_PLUGINS` 指向包含该示例的目录：

```bash
TERVO_PLUGINS=$PWD/docs/examples TERVO_CONFIG=/tmp/tervo-config.toml cargo run -p tervo-app --bin tervo
```

（示例目录名是 `plugin-v3-echo`；插件 id 是 `dev.tervo.echo`。）

---

## 11. 测试

```bash
cargo test -p tervo-app --bin tervo plugin_v3
cargo test -p tervo-app --bin tervo
cargo check --workspace
```

v3 测试使用真实 Python 子进程覆盖：

- manifest 严格校验 / v1/v2 unsupported / 无效 host_abi / entry 逃逸；
- 首次启用确认、状态持久化、重启后自动启动；
- initialize/initialized、command.invoke、status.get、ping、context.version；
- 事件 startup/tab-open/pane-focus 实际投递；
- 坏 JSON 重启、命令超时、status 超时不影响进程、超长行、stdout 总量上限；
- terminal.read capability gate + 每次 Allow/Deny；terminal.write ask；
- clipboard gate（代码路径）、credentials.get 永远拒绝；
- 声明式 UI 七种元素、ui.notify、ui.render；
- reload 更新 manifest 不重启宿主；权限变化要求重新确认；
- disable/enable/restart 不重启宿主；
- 示例 manifest 可解析且 entry 可执行。

---

## 12. 已知限制（MVP）

- **没有 OS 沙箱**；权限不是强制隔离。见第 1 节。
- 插件命令没有独立快捷键绑定，也不支持命令参数（`args` 恒为空）。
  命令只从 Palette 与状态栏点击触发。
- 插件主题只在 Tervo 启动时加载；运行中 enable/reload 不热换主题。
- 多个 panel 只显示第一个 panel 入口；`contributes.panels` 其余 ID 会校验
  但当前 UI 无选择器。
- markdown 是极简纯文本渲染，不执行链接/HTML/图片。
- 状态栏每插件只显示一行；`status.get` 结果超长会被拒绝。
- `assistant` 只是占位，**没有 AI provider**；Tervo 不保存/转发 API Key。
- 没有插件市场、签名、任意 Slint UI、WASM。
- **没有插件 MCP**：MCP 是 Tervo 内嵌、由宿主拥有的功能；插件不能提供/
  代理 MCP server，也不能通过本协议拿到 MCP 工具。
- terminal.read 只读焦点终端尾部；不提供 cwd/env/Vault/历史全文。
- clipboard 只代理文本（不代理图片）。
- 自动重启上限后需要用户 Restart/Reload；不会无限重启。
- 子进程环境变量被清空为白名单；依赖其它环境变量的插件需要自行配置
  启动包装脚本（注意白名单仍不改变“非沙箱”事实）。
- 没有跨 Tervo 实例的插件锁：同时运行多个 Tervo 实例时，每个实例都可能启动
  同一个插件进程并写同一份状态文件（last writer wins）。生产使用建议单实例。
- 非 Linux 平台的进程组/PDEATHSIG 清理未做同等验收；当前主要验证平台是 Linux。

---

## 13. 从 v1/v2 迁移

v1/v2 不会加载；迁移步骤：

1. 清单顶层加 `api = 3`、`host_abi = 3`，`runtime = "process"`。
2. 把 `events` / `commands` / `status` / `theme` 移到 `[contributes]`；
   `[[commands]]` 只保留 `id` / `title` / `description`。
3. 新增 `[permissions]`（字段见第 3 节）；旧的 `fs=... net=...` 声明需要
   改写。首次启用会要求用户重新确认。
4. 协议从旧 Tervo JSON Lines 改成 **JSON-RPC 2.0**：
   - 旧 `ready` 握手 → `initialize` request/response + `initialized`；
   - 旧 `{"event":"command",...}` → `command.invoke` request；
   - 旧 `{"event":"...","payload":...}` → `event.deliver` notification；
   - 旧 `status` → `status.get` / `ui.status`；
   - 旧 stdout 单行结果 → JSON-RPC response/error。
5. 旧 `oneshot` 形态不再支持；必须改成常驻进程。
6. UI 只能通过声明式文档提供，不能自带 Slint/WASM/HTML。

迁移后 v1/v2 目录可以保留，Tervo 会显示 `unsupported`，不会执行；确认不再
需要后手动删除。

---

## 14. 非目标（本版本明确不做）

- 插件 MCP server / MCP 代理 / 插件工具市场；
- 托管插件市场、远程自动更新、在线 key 分发/信任链；
- 任意 Slint UI、WASM 运行时；
- 内置 AI provider 或插件 AI provider 路由；
- 双协议 v1/v2 兼容层；
- OS 级沙箱 / 文件系统/网络强制隔离。

MCP 是宿主内嵌功能（见 `docs/MCP.md` / `docs/MCP_TERMINAL_CONTROL.md`），
与插件协议完全无关。
