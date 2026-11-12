> **已废弃 / Unsupported**：本文档描述 Plugin API v1/v2。Tervo 自 Plugin API v3 起
> **只加载 `api = 3` 且 `host_abi = 3`** 的清单；v1/v2 清单不会再执行，设置页只显示
> `unsupported` 与迁移提示，代码中没有双协议兼容层。请使用
> [`docs/PLUGIN_V3.md`](PLUGIN_V3.md)。以下内容仅作历史参考。

# Tervo 外部进程插件 API（V1 + V2 AI 扩展，api = 1/2）

状态：**V1 稳定子集（2026-11-06）+ V2 AI Chat 扩展（2026-11-09）**。本轮在已有
外部进程 MVP 上增加 API v2 与内置 AI 面板，v1 清单/行为完全保留：

- 常驻进程运行时 `runtime = "process"`：启动/握手（2s）/JSON Lines 双向协议/
  崩溃自动重启（上限 2 次）/超时 kill/坏 JSON 禁用；
- Command Provider：命令进 Palette/快捷键，支持清单静态参数与结构化返回文本，
  结果进底部浮层、活动列表与日志；
- Status Provider：宿主每 2s 轮询或插件主动推送 `text/tooltip/command`，
  主窗口底部显示一行最小状态栏，点击执行声明的插件命令；
- Theme Pack：`[contributes] theme = "theme.toml"` 声明式主题（可选），
  插件禁用/主题坏了不影响 Tervo 启动；
- **API v2 / AI Chat（新增）**：`api = 2` 清单可声明 `[contributes.ai]`，内置
  AI 面板经同一 supervisor 发送 `ai.chat` 请求（messages + 可选 model），插件返回
  `text`；宿主不保存/不转发 API Key，凭据由插件自己持有；
- 权限声明 `[permissions]`（filesystem/network/process/clipboard/credentials）：
  **仅声明与展示，V1/V2 都不授予、不沙箱**；
- 设置页 Plugins 显示 running/restarting/disabled + 错误原因；AI 面板为宿主渲染的
  最小面板（插件列表、prompt、Send/Cancel/Close、输出/错误），不开放任意 Slint UI。

旧行为完整保留：没有 `runtime`（或 `runtime = "oneshot"`）的目录插件仍是“每次命令/事件
启动一个新进程、发一行 JSON 后退出”；旧扁平清单 `<plugins>/*.toml` 仍只展示、不执行。
明确**不做** Tabby/JS/TS/Electron 插件兼容、WASM/WIT、Node sidecar、安装/卸载/市场/签名。

实现：`crates/app/src/plugin.rs`；命令注册：`plugin::register_plugins(&mut Registry, &PluginStore)`；
常驻进程启动：`PluginStore::start_process_plugins()`；主题加载：`themes::load_plugin_theme()`；
AI 请求：`PluginStore::spawn_ai_chat()` / `cancel_ai_chat()`；参考插件：
`docs/examples/ai-echo/`。

## 0. 兼容承诺（0.2.x）

“API 固定”指**可以按本文写插件**，不是永远不扩展：

```text
api = 1   已固定的稳定子集；清单字段、oneshot/process 行为、JSON Lines V1 保持
api = 2   0.2.0 固定为“v1 + 可选 [contributes.ai] + ai.chat”基线
```

0.2.x 的保证：

- 宿主同时接受 `api = 1` 和 `api = 2`；清单 `api` 必须精确等于声明版本。
- v1 清单字段、`init/ready/command/result/event/status/log/shutdown` 行格式不变；
  v2 只增加可选的 AI 贡献，不改变已有 JSON Lines 行。
- 清单未知字段被忽略；AI 请求/结果新增字段只做加法，插件应忽略未知字段。
- 破坏已有字段/语义的变更必须提升到 `api = 3`；不会在 0.2.x 静默破坏 v1/v2。
- `ai.chat` 固定请求形状：`messages: [{role, content}]`，可选 `model`；返回 `text`
  （兼容旧 `result.text` 形状）。凭据/网络由插件负责，宿主不存 Key。

不承诺固定：

- 安装/卸载/市场/签名/权限沙箱/热重载（当前没有）。
- 超时、并发上限、日志/活动截断、UI 坐标/布局等宿主实现细节。
- `[permissions]` 仅展示，不是安全边界；插件以当前用户权限运行。
- 未来大版本可能停止支持 v1；会先写 CHANGELOG/迁移说明。

## 1. 目录与启用规则

插件根目录默认 `~/.config/tervo/plugins`（遵循 `XDG_CONFIG_HOME`）；
环境变量 `TERVO_PLUGINS` 可覆盖（测试/多实例）。

```text
~/.config/tervo/plugins/
├── legacy.toml                  # 旧扁平清单：只展示，不执行
├── status/                      # v1 目录插件；推荐目录名 = 插件 id/name
│   ├── plugin.toml
│   ├── plugin.py                # entry；必须是可执行文件（Unix 权限 + shebang/ELF）
│   └── theme.toml               # 可选，[contributes] theme 声明
└── crash/
    └── plugin.toml
```

启用规则：

- 清单 `enabled` 缺省 `true`；`enabled = false` 时**不注册命令、不启动进程、不执行事件**。
- 启动时读取一次清单并启动已启用的 `runtime = "process"` 插件；修改清单后需重启 Tervo。
- 同名（`name`）或同 `id` 的插件只加载第一个，后续跳过并写日志。
- 目录下没有 `plugin.toml` 的子目录会被忽略；坏清单写 `skipped`，设置页显示数量，
  `[plugin] 跳过 <path>: <原因>` 写日志。
- 目录名不强制等于 id（兼容旧插件）；推荐 `<id>/plugin.toml`，便于人工定位。

## 2. 清单格式 v1/v2

### 2.1 最小 oneshot 插件（旧格式，保持兼容）

```toml
name = "hello"                 # 必填；命令命名空间，[A-Za-z0-9_-]，≤64 字节
version = "1.0.0"              # 必填，仅展示
api = 1                        # 必填；1 或 2（v2 可声明 AI）
entry = "run.sh"               # 必填；插件目录内的可执行相对路径
enabled = true                 # 可选，默认 true
description = "示例插件"        # 可选
events = ["startup", "tab-open"]   # 可选；缺省 = 不订阅

[[commands]]
id = "greet"                   # 必填；插件内唯一
title = "Hello: Greet"         # 必填；Palette 显示 "Hello: Greet (plugin: hello)"
key = "ctrl+alt+g"             # 可选；单个可绑定组合
```

### 2.2 常驻进程插件（V1/V2 格式）

```toml
id = "dev.tervo.status"        # 可选；稳定 ID/展示/去重，缺省 = name；允许反向域名点号
name = "status"                # 必填；命令命名空间仍是 plugin.<name>.<id>
version = "1.0.0"
api = 1                        # 必填；1（v1）或 2（v2，可声明 AI）
runtime = "process"            # 缺省/oneshot = 旧行为；process = 常驻 JSON Lines
entry = "plugin.py"            # 必填；可执行相对路径
description = "构建状态"
events = ["startup", "tab-open", "tab-close"]   # 可选；以通知行投递给常驻进程

[permissions]                  # 可选；仅声明/展示，V1/V2 都不授予
filesystem = "none"            # none | read-plugin | read-home | custom
network = false
process = false
clipboard = false
credentials = "none"           # none | profile-ref

[contributes]                  # 可选
commands = ["greet"]           # 可选交叉校验：必须已在 [[commands]] 声明
status = true                  # 仅 runtime=process 允许；提供状态
theme = "theme.toml"           # 可选；插件目录内 TOML 主题
# ai = { ... }                 # 仅 api=2 允许；见 2.3

[[commands]]
id = "greet"
title = "Status: Greet"
key = "ctrl+alt+g"
args = ["world"]               # 可选；静态参数，最多 16 个、每个 ≤256 字符
```

### 2.3 AI Chat 插件（API v2 新增）

```toml
id = "dev.tervo.ai-echo"
name = "ai-echo"               # 仍用作 supervisor 路由键；AI 面板按 name 定位插件
version = "1.0.0"
api = 2                        # 必须 ≥2 才能声明 contributes.ai
runtime = "process"            # AI 仅支持常驻进程（oneshot 会整份清单拒绝）
entry = "plugin.py"
description = "offline echo"

[permissions]
filesystem = "none"
network = false                # 真实 AI 插件通常改为 true；仍只是声明
process = false
clipboard = false
credentials = "none"           # 凭据由插件自己管理，宿主不下发

[contributes.ai]
id = "dev.tervo.ai-echo"       # 可选；AI 面板展示/去重，缺省回退插件 id/name
name = "AI Echo"               # 可选；面板展示名，缺省回退插件 name
model = "echo-1"               # 可选；作为 ai.chat payload.model 原样下发
description = "deterministic echo; no API key"
```

规则：

| 字段 | 规则 |
|---|---|
| `api` | 必须为 `1` 或 `2`；其它值整份清单跳过 |
| `[contributes.ai]` | 需要 `api = 2` 且 `runtime = "process"`；否则跳过并写原因 |
| `[contributes.ai].id` | 可选；非空时与插件 id 同规则（≤128 字节，字母/数字/`-`/`_`/`.`） |
| `[contributes.ai].name` | 可选；≤128 字符；空 = 使用插件 `name` |
| `[contributes.ai].model` | 可选；≤128 字符；空 = 请求不带 `model` |
| `[contributes.ai].description` | 可选；≤512 字符；仅面板展示 |

AI 插件也需被 `enabled = true` 且完成 `ready` 握手才会出现在面板；宿主不读插件目录内
的任何密钥文件。参考实现：`docs/examples/ai-echo/`（无网络、无 API Key、确定性回复）。

字段校验（失败 = 整份清单跳过，原因写日志/设置页 skipped）：

| 字段 | 规则 |
|---|---|
| `id` | 可选；非空、≤128 字节、仅 ASCII 字母/数字/`-`/`_`/`.`；全局唯一 |
| `name` | 非空，仅 ASCII 字母/数字/`-`/`_`，≤64 字节；命令命名空间 |
| `version` | 非空字符串 |
| `api` | 必须为 `1` 或 `2`（v1 兼容；v2 仅增加可选 AI 贡献） |
| `runtime` | 缺省/`oneshot`/`process`；其它值跳过 |
| `entry` | 非空相对路径；拒绝绝对路径、任何 `..`；canonical 后必须仍在插件目录内（拒绝符号链接逃逸）；必须是已存在的普通文件 |
| `enabled` | 缺省 true；false 时不注册/不启动 |
| `commands[].id` | 非空、小写字母/数字开头，仅小写字母/数字/`-`/`_`/`.`，≤64 字节，插件内唯一 |
| `commands[].title` | 非空 |
| `commands[].key` | 可选；必须是**单个**可绑定组合（Ctrl/Alt/Meta + 键，或 F1–F24） |
| `commands[].args` | 可选；最多 16 个、每个 ≤256 字符；原样进命令协议 |
| `events[]` | 只接受 `startup` / `tab-open` / `tab-close`，不允许重复 |
| `permissions.filesystem` | `none` / `read-plugin` / `read-home` / `custom` |
| `permissions.credentials` | `none` / `profile-ref` |
| `[contributes] commands` | 列出的 id 必须已在 `[[commands]]` 中声明 |
| `[contributes] status` | 仅 `runtime = "process"` 允许 |
| `[contributes] theme` | 相对路径，禁止绝对路径/`..`；文件缺失只记 theme-error，插件仍加载 |
| `[contributes.ai]` | 仅 `api = 2` + `runtime = "process"`；字段默认空，超长/非法 id 跳过 |

其它行为：

- 注册后的完整命令 ID 为 `plugin.<name>.<command-id>`，标题追加来源 `(plugin: <name>)`；
  分类为 `Plugins`，Palette 可搜索/执行，Hotkeys 页可看到/改键。
- 插件默认快捷键只在**内置硬编码快捷键都未命中**时触发，不能覆盖内置键。
- 插件 ID 与内置/其它命令冲突时跳过该命令并写 `[plugin] 命令注册冲突` 日志。
- 清单未知字段被忽略（前向兼容）；旧扁平清单未知字段原样保留。
- 设置页插件行示例：
  `status v1.0.0 — enabled · id=dev.tervo.status · api 1 · process · 1 command(s) · status · theme · running — 构建状态`；
  崩溃后为 `... · disabled: 达到重启上限（2 次）：...`。

## 3. 生命周期（runtime = "process"）

```text
启动 Tervo
  → 扫描清单（只读）
  → start_process_plugins(): 每个启用的 process 插件一个 supervisor 线程
  → spawn entry（cwd=插件目录，环境变量见 §4）
  → 写 init 行（api=清单声明版本；v1 插件仍收到 1，v2 收到 2）
  → 2s 内收到 {"type":"ready","api":2} = running
  → 周期状态轮询 + 异步命令/事件
  → 进程退出/握手失败/命令超时 = crash
       → 自动重启（间隔 300ms），最多 2 次
       → 超过上限 = disabled（设置页保留原因，进程不再启动）
  → 坏 JSON/超长行累计 ≥5 = 直接 disabled（不重启）
  → 宿主退出 = 发 shutdown → 最多等 300ms → kill；Unix 另设 PDEATHSIG
```

- 所有启动/握手/重启/命令等待都在 supervisor/后台线程，UI 线程只提交请求，绝不等插件。
- `status` 轮询每 2s 一次；单次 2s 不返回只丢弃该次请求，不 kill 进程。
- 命令超时 10s；超时会 kill 进程并记 `timeout`，再按崩溃重启/禁用。
- AI `ai.chat` 超时 120s；超时只回复错误并丢弃该次请求，**不 kill 进程**；用户也可在
  面板点 Cancel，效果相同。
- 每次新进程都重新握手；未投递的事件/命令在 supervisor 队列中等待 ready 后执行。
- 无手动重启/禁用按钮；`enabled` 修改需重启 Tervo。

## 4. JSON Lines 协议（stdin/stdout，每行一条 JSON）

编码 UTF-8，一行一个 JSON 对象，单行上限 **64 KiB**（超长行截断并计坏行）。
stderr 不参与协议，逐行写 `[plugin] <name> stderr:` 日志（单行截断 8 KiB）。

启动环境变量：

```text
TERVO_PLUGIN_NAME=<name>
TERVO_PLUGIN_ID=<id>
TERVO_PLUGIN_API=<清单声明版本：1 或 2>
TERVO_PLUGIN_HOST_API=2
TERVO_PLUGIN_PROTOCOL=1
```

### 4.1 宿主 → 插件

```json
{"type":"init","api":2,"host_api":2,"protocol":1,"plugin":"ai-echo","host":"tervo"}
{"type":"command","id":7,"command":"greet","args":["world"]}
{"type":"command","id":8,"command":"ai.chat","args":[],"payload":{"messages":[{"role":"user","content":"hello"}],"model":"echo-1"}}
{"type":"event","event":"tab-open","payload":{"title":"Build: dev","kind":"local"}}
{"type":"status","id":9}
{"type":"shutdown"}
```

- `id` 是宿主生成的递增请求号（不回绕到 0），插件必须原样带回。
- `command.args` 来自清单 `[[commands]].args`。V1 命令面板**不弹参数输入**；
  参数由清单静态声明，保留后续版本扩展。
- `ai.chat` 是 API v2 的 AI 请求约定：`command = "ai.chat"`，结构化数据放在
  `payload`，`args` 固定为空。`payload.messages` 每项为 `{role, content}`，
  role 只允许 `system`/`user`/`assistant`；`payload.model` 可选（来自清单
  `[contributes.ai].model`）。请求最多 32 条消息、每条 ≤16000 字符、模型 ≤128 字符。
  宿主不附带任何 API Key/凭据字段。
- `event.payload` 只含 Tab 标题/类型（见 §7），不会传 cwd/命令行/环境变量/凭据。

### 4.2 插件 → 宿主

```json
{"type":"ready","api":2}
{"type":"result","id":7,"ok":true,"text":"hello world"}
{"type":"result","id":8,"ok":true,"result":{"text":"AI reply"}}
{"type":"status","text":"build ok","tooltip":"main: green","command":"plugin.status.greet"}
{"type":"log","text":"optional diagnostic"}
```

- `ready`：握手响应；`api` 必须与清单声明版本一致（v1=1，v2=2），否则禁用并显示原因；
  旧插件省略 `api` 时按清单版本兼容接受。`init` 里另带 `host_api=2` 表示宿主最高版本。
- `result`：命令结果；`ok=false` 表示命令失败但进程可继续使用。`text` 按字符截断到
  2000（AI `ai.chat` 为 16000），进入底部浮层（普通命令最多 300 字符）、活动列表（80 字符）
  与日志。缺失 `ok` 视为 true。AI 回复也接受 `{"result":{"text":"..."}}`、
  `{"payload":{"text":"..."}}`、`result` 字符串或无 `type` 的 `{"id":N,"text":"..."}`。
- `status`：可响应 `status` 请求，也可主动推送；`text` ≤200 字符、`tooltip` ≤512、
  `command` ≤128。`command` 必须是本插件声明的命令 id 或完整 ID，宿主归一化为完整 ID；
  非法/别的插件命令忽略并写日志。空 `text` 会隐藏状态项。
- `log`：写 `[plugin] <name>:` 日志，不进入 UI。
- 未知 `type`、坏 JSON、缺字段都计坏行；累计 5 次禁用插件。握手期坏行同样计数。

### 4.3 命令结果与 UI

1. Palette/快捷键触发 → `PluginStore::spawn_command()` 查找命令 → 发 `command` 行。
2. supervisor 记录 pending + 10s deadline；stdout 收到 `result` 后按 id 回传等待线程。
3. 等待线程写「最近插件活动」（`ok` / `error` / `timeout` / `unavailable`）并推通知；
   UI 在 event-loop 唤醒/400ms 兜底时取走通知，底部浮层显示
   `plugin: text`（失败为 `plugin: error: text`）。
4. 完整结果也写日志；活动列表只保留摘要。UI 线程不在命令路径上等待。

### 4.4 AI Chat 面板与请求路由（API v2）

```text
Settings → Plugins → Open AI Chat
  → 宿主读取 PluginStore::ai_plugins()（enabled + api=2 + process + [contributes.ai]）
  → 用户输入 prompt；Rust 把最近对话组装成 messages
  → PluginStore::spawn_ai_chat(plugin, messages, model)
       → 同一 supervisor 写 {"type":"command","command":"ai.chat","payload":{...}}
       → pending kind=AiChat，deadline=120s
  → 后台等待线程收 result，写活动 "ai.chat / ok|error|cancelled"（只记状态/字符数，不记正文）
  → WakeSignal 叫醒 UI；Slint 只在 UI 线程追加一次回复
  → Cancel/Close：SupervisorMsg::CancelAi，只丢弃该次请求，不 kill 进程
```

- 无 AI 插件时面板显示空状态：示例插件 `docs/examples/ai-echo/` 与真实插件目录
  `~/.config/tervo/plugins/ai-echo/`，并说明需要 `api = 2`、`runtime = "process"`、
  `[contributes.ai]`；不联网、不下载。
- 面板固定由宿主渲染：插件列表、prompt、Send/Cancel/Close、输出、错误与安全声明；
  插件不能提供任意 UI。
- 宿主在 `ai.chat` 请求里只放 `messages` + 可选 `model`，不读取插件目录外的密钥，
  也不把 Vault/Profile/环境变量放进 payload。
- API v2 插件仍使用协议 V1；`api=2` 只表示“可声明新贡献点”，不改变 JSON Lines 行格式。
- 参考实现与手工验证：`docs/examples/ai-echo/`（无网络、无 API Key、确定性回复）。

## 5. Status Provider

- 清单 `[contributes] status = true` 且 `runtime = "process"`。
- 宿主每 2s 发 `{"type":"status","id":N}`；插件也可随时主动推 `status`。
- 主窗口底部一行（`plugin: text · ...`，最多 560px，elide）；多个插件用 ` · ` 合并。
- 状态项带 `tooltip`（当前保存在 Rust/UI 属性，不弹自定义控件）与 `command`；
  点击状态栏执行第一条带 command 的命令（仍走正常命令注册/分发）。
- 状态更新通过 `WakeSignal` 唤醒 UI，只在 revision 变化时更新 Slint 属性。
- 进程崩溃/禁用/退出时清空该插件状态，避免显示过期信息。

## 6. Theme Pack（声明式 TOML，可选）

- 清单 `[contributes] theme = "theme.toml"`，文件在插件目录内；路径校验同 entry
  （拒绝绝对路径/`..`/逃逸），但**不要求可执行**。
- 支持 Tervo 完整 TOML 主题或 Alacritty TOML（与 `~/.config/tervo/themes/` 相同格式；
  也可用现有导入格式的扩展名，如 `.itermcolors`/`.json`/Xresources）。
- 主题 id 统一加前缀 `custom:plugin-<slug>-<stem>`，显示名追加 `(plugin: <name>)`，
  避免与用户主题撞 id；重复 id 跳过并记录。
- 主题坏了/文件缺失只在插件行显示 `theme-error` 并写日志，**插件仍加载**；
  主题是可选贡献，不阻塞 Tervo 启动。
- 主题列表由设置页现有下拉框展示；选择后写 `config.toml` 的 `theme`，无需额外 UI。

## 7. 事件与 payload（两种 runtime 共用）

| 事件 | 触发时机 | payload |
|---|---|---|
| `startup` | 应用状态/设置页就绪后、首个 Tab 创建前 | `{}` |
| `tab-open` | 本地 Tab、Profile Tab、恢复的 Tab、复制 Tab 打开后 | `{"title":"Build: dev","kind":"local"}` |
| `tab-close` | 关闭 Tab、关闭最后一个 Pane、Close Others 关闭 Tab 后 | `{"title":"Build: dev"}` |

- `title` 已去控制字符、合并空白、按字符截断到 200；`kind` 只允许短 ASCII 标识，
  异常值回退 `local`。
- 事件 payload **不包含** cwd、命令行、环境变量、Profile 密码、终端内容或路径。
- oneshot runtime：每个声明事件的插件起一个后台线程，spawn entry、发一行
  `{"event":"<name>","payload":{...}}` 后关闭 stdin，10s 超时 kill；每次都是新进程。
- process runtime：宿主向常驻进程发 `{"type":"event",...}` 通知，无返回值、不重启、
  不影响命令/状态；插件未 ready 时事件在队列中等待。
- 关闭最后一个 Tab/窗口时的 `tab-close` 仍是尽力而为。

## 8. 设置页、AI 面板与诊断

- Settings → Plugins 列表逐行显示：名称/版本、enabled/disabled、id、api、runtime、
  命令数、事件数、status/theme/ai 贡献、running/restarting/disabled 原因、权限声明、说明。
- Settings → Plugins 顶部有 **AI Chat** 卡片和「Open AI Chat…」按钮；无插件时显示
  空状态与示例路径。AI 面板列出所有已启用的 api=2 AI 插件，提供 prompt 输入、
  Send/Cancel/Close、只追加一次的回复、错误状态和“插件以当前用户权限运行、宿主不保存
  API Key”的安全声明；不阻塞终端渲染，请求/等待都在后台线程。
- summary：`N plugins · M enabled · K commands · S skipped`。
- 「Recent plugin activity」保留最近 8 条命令/事件/AI/生命周期摘要（时间/插件/动作/结果/
  首行），后台写、唤醒 UI 刷新；不持久化，重启清空。AI 只记状态与字符数，不记 prompt/回复正文。
- 所有完整 stdout/stderr/协议错误写 `~/.local/state/tervo/logs`（`[plugin]` 前缀）；
  坏清单在启动日志逐条打印跳过原因。
- 无 UI 开关/安装/卸载/搜索/详情页；`enabled` 仍用手写 TOML + 重启。

日志示例：

```text
[plugin] 已启动 2 个常驻进程插件（握手超时 2000ms，命令超时 10000ms，重启上限 2）
[plugin] 发现 3 个插件（3 个可执行包），注册 2 条命令，跳过 0
[plugin] status: 常驻进程就绪 pid=362
[plugin] status: 状态更新 text="build ok" tooltip="fake build status" command="plugin.status.greet"
[plugin] plugin.status.greet ok: hello world
[plugin] crash: 进程异常（握手前进程退出），准备重启 1/2
[plugin] crash: 已禁用：达到重启上限（2 次）：握手前进程退出
[plugin] badjson: 坏 JSON/超长行（5/5）: ...
```

## 9. 权限与安全（必读）

**插件是普通子进程，拥有当前用户的全部权限，没有沙箱。**

- `[permissions]` 只是声明：V1 不实现 filesystem/network/process/clipboard/credentials
  的能力授予或撤销，没有 seccomp/namespace/chroot/Landlock/WASM 隔离。
- 插件可以读写当前用户可访问的任何文件、访问网络、启动其它进程、读取环境变量；
  可以读取 `~/.ssh` 私钥、`config.toml`、`profiles.toml`、日志等。
- **不向插件传 Vault 明文**：协议里没有密码/凭据字段，事件 payload 只带 Tab 标题/类型；
  `credentials = "profile-ref"` 仅声明，V1/V2 不会解析/下发任何 Vault 引用。
- **AI 凭据不经过宿主**：`ai.chat` payload 只有 `messages` 与可选 `model`；宿主不读
  API Key、不写 Key、不转发 Key。真实 AI 插件必须在自己的进程内管理凭据与网络访问；
  宿主的活动列表/日志只记录状态与字符数，不记录 prompt/回复正文（完整错误文本写日志）。
- 宿主不做 shell 拼接：entry 直接 `exec`，命令/事件/参数都以 JSON 传 stdin；没有 shell
  解释器参与。`entry` 的目录内校验只防清单指向目录外，**不是沙箱**。
- 输出限制：协议单行 64 KiB、普通结果文本 2000 字符、AI 结果 16000 字符、状态 200、
  tooltip 512、活动摘要 80、日志单行 8 KiB；stderr 逐行截断；坏 JSON 累计 5 次禁用。
- 超时：握手 2s、普通命令 10s、AI 请求 120s、状态单次 2s；普通命令超时/进程退出 →
  kill + 有上限重启；AI 超时/取消只丢弃该请求，不 kill 进程。Unix 子进程设置 PDEATHSIG，
  宿主退出不留下插件孤儿。
- 只安装/启用你信任的插件；当前没有签名、来源校验、安装前权限展示。
- 需求中的插件分权（SEC-005/PLUG-004）**未实现**；在引入沙箱或 capability 前，
  不应把插件 API 当成安全边界。

## 10. 测试与手工验证

### 10.1 自动测试

```bash
CARGO_HOME=$PWD/.cargo CARGO_TARGET_DIR=$PWD/target \
  cargo test -p tervo-app --bin tervo
```

插件相关单测（`plugin.rs`）：

- 清单：旧/新共存、新字段解析、api 1/2 兼容、runtime/permissions/contributes/args 校验、
  坏 theme 路径、主题文件缺失、重复 id/name、非法 entry/符号链接、禁用；
- AI：`[contributes.ai]` 解析/默认值、api=2 + process 校验、`ai.chat` payload 路由、
  嵌套/最小 result text 解析、取消回传 `cancelled` 且不 kill 进程、活动不记录 prompt 正文；
- 进程：Python 命令（静态参数 + 返回文本）、Node 命令、状态轮询与 command 归一化、
  startup 事件投递、崩溃重启到上限禁用、命令超时 kill、坏 JSON 禁用、握手超时、
  api 不匹配、禁用插件不启动、协议行长度上限；
- 主题：`themes::tests::plugin_theme_is_namespaced_and_duplicate_is_rejected`。

GUI 集成/截图（Xvfb :142）：

```bash
CARGO_HOME=$PWD/.cargo CARGO_TARGET_DIR=$PWD/target \
  cargo build -p tervo-app --bin tervo --bin xwin --bin xclick --bin xkey
CARGO_TARGET_DIR=$PWD/target ./scripts/e2e_plugin_v1.sh
```

脚本断言：常驻握手、状态轮询、Theme Pack 加载、Palette 命令结果、状态栏点击执行、
崩溃重启上限禁用、disabled 插件不启动；输出
`.logs/plugin-v1/{command_result,status,plugins}.png`。

AI 面板手工/自动验证（无需网络或密钥）：

```bash
cp -r docs/examples/ai-echo ~/.config/tervo/plugins/ai-echo
chmod +x ~/.config/tervo/plugins/ai-echo/plugin.py
# 重启 Tervo → Settings → Plugins → Open AI Chat → 选 AI Echo → 输入 hello → Send
```

Headless Xvfb 冒烟（与 `e2e_plugin_v1.sh` 共用 xwin/xclick/xkey）：

```bash
CARGO_TARGET_DIR=$PWD/target ./scripts/e2e_ai_panel.sh
```

脚本断言：Settings 搜索跳转、AI 卡片/面板打开、`ai.chat` 经同一 supervisor 提交、
AI Echo 返回、UI 收到结果、prompt/回复正文不写应用日志、Close 返回设置页；输出
`.logs/ai-panel/{settings-ai-card,panel-empty,panel-result,panel-closed}.png`。

预期日志：`[plugin] ai-echo: 常驻进程就绪`、`[plugin] ai-echo: ai.chat ok (N chars)`；
面板只追加一次 `AI: ai-echo[echo-1] 1 message(s): hello`，不显示假 token 流。
`cargo test -p tervo-app --bin tervo` 还覆盖 `ai_chat_routes_payload_and_returns_text` 与
`ai_chat_cancel_returns_cancelled_without_killing_plugin`（Python 常驻进程，headless）。

### 10.2 手工最小插件

```sh
export TERVO_PLUGINS="$HOME/.config/tervo/plugins"
mkdir -p "$TERVO_PLUGINS/demo"
cat > "$TERVO_PLUGINS/demo/plugin.toml" <<'EOF'
name = "demo"
version = "1.0.0"
api = 1
runtime = "process"
entry = "plugin.py"
description = "V1 demo"

[contributes]
status = true
commands = ["greet"]

[[commands]]
id = "greet"
title = "Demo: Greet"
key = "ctrl+alt+g"
args = ["world"]
EOF
cat > "$TERVO_PLUGINS/demo/plugin.py" <<'EOF'
#!/usr/bin/env python3
import json, sys
for line in sys.stdin:
    msg = json.loads(line)
    t = msg.get("type")
    if t == "init":
        print(json.dumps({"type": "ready", "api": 1}), flush=True)
    elif t == "command":
        print(json.dumps({"type": "result", "id": msg["id"], "ok": True,
                          "text": "hello " + " ".join(msg.get("args", []))}), flush=True)
    elif t == "status":
        print(json.dumps({"type": "status", "text": "demo ok",
                          "tooltip": "demo", "command": "greet"}), flush=True)
    elif t == "shutdown":
        break
EOF
chmod +x "$TERVO_PLUGINS/demo/plugin.py"
```

启动后确认：日志 `[plugin] demo: 常驻进程就绪`；底部状态栏 `demo: demo ok`；
`Ctrl+Shift+P` 搜 `Demo: Greet` 回车（或 Ctrl+Alt+G）→ 底部浮层
`demo: hello world`、活动列表新增一行；点击状态栏会再次执行 greet。
把 `plugin.py` 改成握手前退出 → 约 1s 内重启两次后设置页显示 disabled 原因；
改成输出 `not json` → 5 行后禁用；把 `enabled = false` → 不启动、Palette 无命令。

## 11. 限制与非目标

- 不兼容 Tabby npm/JS/TS、Electron/Angular/WebView；无 Node sidecar/RPC。
- 扩展点仅 Commands、三个生命周期事件、Status、Theme；Connection/Middleware/
  Observer/Linkifier/Workspace/Profile Provider/设置 schema/任意事件订阅等未实现。
- 命令参数只有清单静态 `args`，无运行时输入框/补全/取消；返回值只有文本，无二进制/
  回调通道；命令结果前 UI 不阻塞，但插件仍在独立进程内。
- 无安装/卸载/更新/市场/签名/权限授予；权限仅声明；无沙箱。
- AI 仅支持 `runtime = "process"`、一次完整回复、单轮请求内发送最近对话（无 token
  streaming/工具调用/多模态/取消后的服务端中断）；插件列表启动时快照，新装插件需重启；
  真实模型、密钥和网络完全由插件自己实现；宿主不内置任何 AI provider。
- 无常驻插件热重载/手动重启/单插件并发上限；普通命令可并发（同一插件多条同时飞）；
  AI 面板同一时间只允许一个进行中的请求（Cancel 后才可再发）。
- 活动列表仅 8 条、内存、只读；状态栏只有一行文本，无图标/多行/Tab badge/右键项。
- 主题包只进主题下拉列表，不自动切换/不覆盖 UI 布局；无字体/快捷键/Profile 包。
- 无 Windows 验证；entry 需 Unix 可执行文件（shebang 脚本或 ELF）。
- 旧扁平清单仍只展示、不执行；需迁移为 `<name>/plugin.toml` 才能获得执行能力。
