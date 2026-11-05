# Tervo 插件 API（外部进程 MVP，api = 1）

状态：**MVP / 实验性**。当前闭环：

- v1 目录清单 `<plugins>/<name>/plugin.toml`：
  - `[[commands]]` 注册进命令表，从 Command Palette 或插件默认快捷键触发；
  - `events = ["startup", "tab-open", "tab-close"]` 声明生命周期事件，宿主在对应时机
    异步执行 `entry`；
  - 命令与事件都在**后台线程** spawn `entry` 子进程，通过 stdin 发一行 JSON，
    stdout/stderr 写日志，超时 kill；
  - 最近 8 条执行摘要进入设置页「最近插件活动」（只读展示）。
- 保留旧扁平清单 `<plugins>/*.toml`：继续发现、校验、展示、统计启用状态，
  **不执行任何代码**（旧版只有元数据，没有 entry/commands/events）。
- 明确**不做** Tabby/JS/TS 插件兼容，不做 WASM/WIT，不做 Node sidecar。

实现：`crates/app/src/plugin.rs`；命令表扩展点：`plugin::register_plugins(&mut Registry, &PluginStore)`；
事件扩展点：`PluginStore::spawn_event(PluginEvent, payload)`。

## 1. 目录与启用规则

插件根目录默认 `~/.config/tervo/plugins`（遵循 `XDG_CONFIG_HOME`）；
环境变量 `TERVO_PLUGINS` 可覆盖（测试/多实例）。

```text
~/.config/tervo/plugins/
├── legacy.toml              # 旧扁平清单：只展示，不执行
└── hello/                   # v1 目录插件；目录名建议与 name 一致
    ├── plugin.toml
    └── run.sh               # entry；必须可执行（Unix 权限 + shebang/ELF）
```

启用规则：

- v1 清单的 `enabled` 缺省为 `true`；`enabled = false` 时**不注册命令、不执行事件**。
- 启动时读取一次清单；修改 `enabled`、命令、事件或 entry 后需要**重启 Tervo**。
- 同名（`name`）的 v1 插件只加载第一个，后续跳过并在日志/设置页计数。
- 旧扁平清单按 `id` 去重，行为与旧版一致；旧清单**不能**通过加 `events`/`entry`
  字段获得执行能力。
- 目录下没有 `plugin.toml` 的子目录会被忽略；坏清单写入 `skipped`（设置页显示数量，
  `[plugin] 跳过 <path>: <原因>` 写日志）。

## 2. 清单格式 v1

`~/.config/tervo/plugins/hello/plugin.toml`：

```toml
name = "hello"                 # 必填；命令命名空间，[A-Za-z0-9_-]，≤64 字节
version = "1.0.0"              # 必填，仅展示
api = 1                        # 必填；宿主只支持 1
entry = "run.sh"               # 必填；插件目录内的相对路径
enabled = true                 # 可选，默认 true
description = "示例插件"        # 可选
events = ["startup", "tab-open"]   # 可选；缺省 = 不订阅任何事件

[[commands]]
id = "greet"                   # 必填；插件内唯一，小写字母/数字/-/_/.
title = "Hello: Greet"         # 必填；Palette 显示为 "Hello: Greet (plugin: hello)"
key = "ctrl+alt+g"             # 可选；单个可绑定组合键

[[commands]]
id = "status"
title = "Hello: Status"
```

字段校验（失败 = 整个插件跳过，原因写日志）：

| 字段 | 规则 |
|---|---|
| `name` | 非空，仅 ASCII 字母/数字/`-`/`_`，≤64 字节；同时用于 `plugin.<name>.<id>` 命名空间 |
| `version` | 非空字符串 |
| `api` | 必须精确等于 `1`；其它值报“不支持的 api” |
| `entry` | 非空相对路径；拒绝绝对路径、任何 `..`；canonical 后必须仍在插件目录内（拒绝符号链接逃逸）；必须是已存在的普通文件 |
| `enabled` | 缺省 true；false 时命令/事件都不执行 |
| `commands[].id` | 非空、小写字母/数字开头，仅小写字母/数字/`-`/`_`/`.`，≤64 字节，插件内唯一 |
| `commands[].title` | 非空 |
| `commands[].key` | 可选；必须是**单个**可绑定组合（Ctrl/Alt/Meta + 键，或 F1–F24）；多段键/普通字符拒绝 |
| `events[]` | 可选；只接受 `startup` / `tab-open` / `tab-close`，不允许重复；未知值整份清单跳过 |

其它行为：

- 注册后的完整 ID 为 `plugin.<name>.<command-id>`，标题追加来源 `(plugin: <name>)`；
  分类为 `Plugins`，Palette 可搜索/执行，Hotkeys 设置页可看到/改键。
- 插件默认快捷键只在**内置硬编码快捷键都未命中**时触发，不能覆盖 `Ctrl+T` 等内置键；
  用户可在 Hotkeys 页给插件命令绑定 override。
- 插件 ID 与内置/其它命令冲突时跳过该命令并写 `[plugin] 命令注册冲突` 日志。
- 清单中的未知字段被忽略（便于前向兼容）；旧扁平清单的未知字段仍原样保留。
- 设置页插件行显示：名称/版本/状态/API/命令数/事件数/说明，例如
  `hello v1.0.0 — enabled · api 1 · 1 command(s) · 2 event(s) — 示例插件`。

## 3. 执行协议（命令 + 事件）

### 3.1 命令

命令被触发（Palette 或插件默认快捷键 / 用户 override）时：

1. UI 线程只做查找并 spawn 一个后台线程，**不等待子进程**。
2. 后台线程启动 `entry`：
   - `cwd` = 插件目录（canonical）；
   - 环境变量 `TERVO_PLUGIN_NAME=<name>`、`TERVO_PLUGIN_API=1`；
   - stdin/stdout/stderr 均为管道。
3. 向 stdin 写入**一行** JSON，然后关闭 stdin（插件读到 EOF）：

   ```json
   {"event":"command","id":"greet","args":[]}
   ```

   `id` 是清单里声明的命令 id（**不含** `plugin.<name>.` 命名空间）；
   `args` 当前恒为空数组，保留给后续版本。
4. 并发读取 stdout/stderr（各最多保留 64 KiB，超出继续 drain 但日志标记截断），
   全部写入 stderr 日志，带 `[plugin]` 前缀。
5. 等待进程退出；超过 **10 秒** 则 `kill()` + `wait()`，记录超时。
6. 退出码/超时/输出**仍只写日志**；额外把一条摘要放进「最近插件活动」。
   不向插件回传结构化结果。

### 3.2 事件钩子

清单里声明 `events` 后，宿主在对应时机执行 `entry`。**未声明该事件、或插件
`enabled = false`，都不会触发。**

| 事件 | 触发时机 | payload |
|---|---|---|
| `startup` | 应用状态/设置页就绪后、首个 Tab 创建前（随后才会发首 Tab 的 `tab-open`） | `{}` |
| `tab-open` | 本地 Tab、Profile Tab、恢复的 Tab、复制 Tab 打开后 | `{"title":"Build: dev","kind":"local"}` |
| `tab-close` | 关闭 Tab、关闭最后一个 Pane、Close Others 关闭 Tab 后 | `{"title":"Build: dev"}` |

事件协议（stdin 单行 JSON，写完关闭 stdin）：

```json
{"event":"startup","payload":{}}
{"event":"tab-open","payload":{"title":"Build: dev","kind":"ssh"}}
{"event":"tab-close","payload":{"title":"Build: dev"}}
```

- `title` 已去除控制字符、合并空白、按字符截断到 200；`kind` 只允许短 ASCII
  标识（`local` / `ssh` / `telnet` / `serial` 等），异常值回退 `local`。
- 事件 payload **不包含** cwd、命令行、环境变量、Profile 密码、终端内容或路径；
  但插件进程本身仍拥有当前用户全部权限（见 §4）。
- 每个声明该事件的已启用插件各起一个后台线程；`spawn_event` 立即返回，UI 不等待。
- 事件执行失败、spawn 失败、超时（同为 10 秒）都只写 `[plugin]` 日志与活动列表，
  不影响窗口/命令主流程，也不会自动重试。
- 每次事件都是**新进程**；没有常驻插件进程，也没有事件去重/合并/取消。
- 关闭最后一个 Tab 会立刻退出事件循环，因此该次 `tab-close` 是**尽力而为**
  （后台插件可能来不及跑完）；窗口正常退出同理。

### 3.3 最近插件活动（设置页，只读）

Plugins 设置页保留最近 **8** 条命令/事件执行摘要，新记录在前：

```text
14:03:22 · hello · command greet · exit=0 · plugin=hello api=1
14:03:20 · hello · event tab-open · exit=0 · opened Build: dev
14:03:10 · slow · command slow · timeout
14:02:55 · broken · command boom · spawn-error · 无法启动 /path/to/run.sh: ...
```

- 每行固定包含：时间（本地 `HH:MM:SS`）/ 插件 / 命令或事件 / 结果
  （`exit=<code>`、`timeout`、`signal`、`spawn-error`）/ 输出首行摘要。
- 摘要优先取 stdout 首个非空行，其次 stderr；去控制字符、合并空白、按字符截断到 80。
- 完整 stdout/stderr 仍只写日志（`~/.local/state/tervo/logs`）；活动列表不持久化，
  重启清空，也不能点击/复制。
- 不收集密码、环境变量、stdin 或完整终端内容。

日志示例：

```text
[plugin] 发现 2 个插件（1 个可执行包），注册 2 条命令，跳过 0
[plugin] plugin.hello.greet exit=Some(0) elapsed=12ms stdout=34B stderr=0B
[plugin] plugin.hello.greet stdout:
plugin=hello api=1
[plugin] hello:event tab-open exit=Some(0) elapsed=5ms stdout=0B stderr=0B
[plugin] plugin.hello.slow 超时，已 kill elapsed=10002ms stdout=0B stderr=0B
[plugin] plugin.hello.slow 执行失败（超时，已 kill）
```

约定：退出码 0 = 成功；非 0 / 被信号杀死 / 超时都只记日志与活动列表，不重试、不弹窗。
每次触发都是**新进程**；不维护常驻插件进程，也不阻止同一插件并发执行多次。

## 4. 权限与安全（必读）

**插件是普通子进程，拥有当前用户的全部权限，没有沙箱。**

- 没有 capability/权限声明，没有 seccomp、namespace、chroot、Landlock、WASM 隔离；
  也无法撤销文件/网络/进程权限。插件可以读写当前用户可访问的任何文件、
  访问网络、启动其它进程、读取环境变量。
- 插件可以读取 `~/.ssh` 私钥、`config.toml`、`profiles.toml`、日志等；
  Vault 的 `vault.enc` 虽然是密文，同用户进程仍可读取/复制，安全边界是主密码而非插件隔离。
- `entry` 的“必须在插件目录内”只防止清单用绝对路径/`..`/符号链接把宿主指向目录外，
  **不是沙箱**：entry 本身仍可访问整台机器。
- 事件 payload 只包含 Tab 标题/类型，不含凭据；但这只是减少宿主主动外泄，
  **不代表**插件读不到本机敏感数据。
- 插件写到 stdout/stderr 的内容会进入 Tervo 日志（`~/.local/state/tervo/logs`，
  默认保留 8 个）；输出首行还会进入设置页活动列表。不要把 secret 打到输出里。
- 只安装/启用你信任的插件。当前没有签名、来源校验、安装前权限展示。
- 需求中的插件分权（SEC-005/PLUG-004）**未实现**；在引入沙箱或 capability 前，
  不应把插件 API 当成安全边界。

## 5. 限制与非目标

- 不兼容 Tabby 的 npm/JS/TS 插件，不识别 `package.json` / `tabby-plugin` keyword，
  无 Angular/WebView UI，无 Node sidecar/RPC。
- 扩展点：Commands（+ 默认快捷键/override）与三个固定生命周期事件
  （`startup` / `tab-open` / `tab-close`）。Toolbar、Settings、Tab/终端菜单、
  Profile Provider、Middleware、Decorator、Linkifier、Color Scheme、任意事件订阅
  等未实现。
- 事件不可按插件参数过滤、不可取消/去重/合并，也没有“pane 输出/命令完成/退出”等事件；
  事件无返回值，不能修改 Tab/终端状态。
- 命令结果不回传给插件；宿主只把退出码/超时/输出首行的摘要展示在设置页，
  没有结构化返回值或回调通道。
- 无安装/卸载/更新/市场/签名校验；Plugins 设置页只列出已安装清单和最近活动。
- 无热重载：`enabled`/entry/命令/事件变更需重启；无单插件超时/并发上限/崩溃重启配置。
- 活动列表仅保留最近 8 条、内存态、只读、不持久化，也不展示 stderr 完整内容；
  关闭最后一个 Tab/窗口时的 `tab-close` 是尽力而为。
- 无 Windows 支持验证；entry 需是 Unix 可执行文件（shebang 脚本或 ELF）。
- 旧扁平清单仍只展示、不执行；不能通过给旧清单加 `entry`/`events` 字段获得执行能力，
  需要迁移为 `<name>/plugin.toml`。

## 6. 手工验证步骤

1. 准备命令 + 事件插件：

   ```sh
   export TERVO_PLUGINS="$HOME/.config/tervo/plugins"
   mkdir -p "$TERVO_PLUGINS/hello"
   cat > "$TERVO_PLUGINS/hello/plugin.toml" <<'EOF'
   name = "hello"
   version = "1.0.0"
   api = 1
   entry = "run.sh"
   description = "MVP demo"
   events = ["startup", "tab-open", "tab-close"]

   [[commands]]
   id = "greet"
   title = "Hello: Greet"
   key = "ctrl+alt+g"
   EOF
   cat > "$TERVO_PLUGINS/hello/run.sh" <<'EOF'
   #!/bin/sh
   IFS= read -r line
   printf 'plugin=%s api=%s stdin=%s\n' "$TERVO_PLUGIN_NAME" "$TERVO_PLUGIN_API" "$line" >> events.log
   printf 'stderr-from-plugin\n' >&2
   exit 0
   EOF
   chmod +x "$TERVO_PLUGINS/hello/run.sh"
   ```

2. 启动 Tervo，日志应出现 `[plugin] 发现 1 个插件（1 个可执行包），注册 1 条命令`；
   设置 → Plugins 显示 `hello v1.0.0 — enabled · api 1 · 1 command(s) · 3 event(s)`。
   启动后 `"$TERVO_PLUGINS/hello/events.log"` 应新增一行 `"event":"startup"`。
3. 新建一个 Tab：`events.log` 新增 `"event":"tab-open"`，payload 含 `title`/`kind`；
   关闭该 Tab：新增 `"event":"tab-close"`，payload 只含 `title`。
4. `Ctrl+Shift+P` 搜 `Hello: Greet` 并回车（或按 `Ctrl+Alt+G`），日志应出现
   `[plugin] plugin.hello.greet exit=Some(0)`；设置页「Recent plugin activity」
   出现 `时间 · hello · command greet · exit=0 · ...`，终端输入不卡顿。
5. 把 entry 改成 `exec sleep 30`，触发后约 10 秒日志出现 `超时，已 kill`，
   活动列表对应行显示 `timeout`。
6. 在清单加 `enabled = false`，重启：Palette 不再出现该命令，设置页显示 `disabled`，
   新建/关闭 Tab 不再产生事件日志；把 entry 指向不存在文件（或删除）时命令活动行
   显示 `spawn-error`，主窗口与其它命令不受影响。
7. 把 `entry = "../x"`、`api = 2` 或 `events = ["tab-explode"]` 写入清单，重启：
   设置页 skipped +1，日志出现对应 `[plugin] 跳过 ...` 原因。
8. 无 GUI 回归：`cargo test -p tervo-app --bin tervo`（清单/事件过滤/payload/执行/
   禁用/超时/活动截断均有单测）。
