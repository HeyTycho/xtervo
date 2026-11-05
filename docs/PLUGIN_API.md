# Tervo 插件 API（外部进程 MVP，api = 1）

状态：**MVP / 实验性**。本轮只实现“外部可执行文件 + 清单声明命令”的闭环：

- 支持 v1 目录清单 `<plugins>/<name>/plugin.toml`：把 `[[commands]]` 注册进命令表，
  从 Command Palette 或插件默认快捷键触发，在后台线程 spawn `entry` 子进程。
- 保留旧扁平清单 `<plugins>/*.toml`：继续发现、校验、展示、统计启用状态，
  **不执行任何代码**（旧版只有元数据，没有 entry/commands）。
- 明确**不做** Tabby/JS/TS 插件兼容，不做 WASM/WIT，不做 Node sidecar。

实现：`crates/app/src/plugin.rs`；命令表扩展点：`plugin::register_plugins(&mut Registry, &PluginStore)`。

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

- v1 清单的 `enabled` 缺省为 `true`；`enabled = false` 时不注册命令、不执行。
- 启动时读取一次清单；修改 `enabled`、命令或 entry 后需要**重启 Tervo**。
- 同名（`name`）的 v1 插件只加载第一个，后续跳过并在日志/设置页计数。
- 旧扁平清单按 `id` 去重，行为与旧版一致。
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
| `enabled` | 缺省 true |
| `commands[].id` | 非空、小写字母/数字开头，仅小写字母/数字/`-`/`_`/`.`，≤64 字节，插件内唯一 |
| `commands[].title` | 非空 |
| `commands[].key` | 可选；必须是**单个**可绑定组合（Ctrl/Alt/Meta + 键，或 F1–F24）；多段键/普通字符拒绝 |

其它行为：

- 注册后的完整 ID 为 `plugin.<name>.<command-id>`，标题追加来源 `(plugin: <name>)`；
  分类为 `Plugins`，Palette 可搜索/执行，Hotkeys 设置页可看到/改键。
- 插件默认快捷键只在**内置硬编码快捷键都未命中**时触发，不能覆盖 `Ctrl+T` 等内置键；
  用户可在 Hotkeys 页给插件命令绑定 override。
- 插件 ID 与内置/其它命令冲突时跳过该命令并写 `[plugin] 命令注册冲突` 日志。
- 清单中的未知字段被忽略（便于前向兼容）；旧扁平清单的未知字段仍原样保留。

## 3. 执行协议

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
6. 退出码/超时/输出只写日志，MVP **不回传 UI**，也没有结构化返回值。

日志示例：

```text
[plugin] 发现 2 个插件（1 个可执行包），注册 2 条命令，跳过 0
[plugin] plugin.hello.greet exit=Some(0) elapsed=12ms stdout=34B stderr=0B
[plugin] plugin.hello.greet stdout:
plugin=hello api=1
[plugin] plugin.hello.slow 超时，已 kill elapsed=10002ms stdout=0B stderr=0B
[plugin] plugin.hello.slow 执行失败（超时，已 kill）
```

约定：退出码 0 = 成功；非 0 / 被信号杀死 / 超时都只记日志，不重试、不弹窗。
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
- 插件写到 stdout/stderr 的内容会进入 Tervo 日志（`~/.local/state/tervo/logs`，
  默认保留 8 个）；不要把 secret 打到输出里。
- 只安装/启用你信任的插件。当前没有签名、来源校验、安装前权限展示。
- 需求中的插件分权（SEC-005/PLUG-004）**未实现**；在引入沙箱或 capability 前，
  不应把插件 API 当成安全边界。

## 5. 限制与非目标

- 不兼容 Tabby 的 npm/JS/TS 插件，不识别 `package.json` / `tabby-plugin` keyword，
  无 Angular/WebView UI，无 Node sidecar/RPC。
- 扩展点只有 Commands（+ 默认快捷键/override）。Toolbar、Settings、Tab/终端菜单、
  Profile Provider、Middleware、Decorator、Linkifier、Color Scheme 等未实现。
- 不向插件回传执行结果，不提供 TUI/UI 回调、通知、事件订阅或常驻生命周期。
- 无安装/卸载/更新/市场/签名校验；Plugins 设置页只列出已安装清单。
- 无热重载：`enabled`/entry/命令变更需重启；无单插件超时/并发上限/崩溃重启配置。
- 无 Windows 支持验证；entry 需是 Unix 可执行文件（shebang 脚本或 ELF）。
- 旧扁平清单仍只展示、不执行；不能通过给旧清单加 `entry` 字段来获得执行能力，
  需要迁移为 `<name>/plugin.toml`。

## 6. 手工验证步骤

1. 准备插件：

   ```sh
   export TERVO_PLUGINS="$HOME/.config/tervo/plugins"
   mkdir -p "$TERVO_PLUGINS/hello"
   cat > "$TERVO_PLUGINS/hello/plugin.toml" <<'EOF'
   name = "hello"
   version = "1.0.0"
   api = 1
   entry = "run.sh"
   description = "MVP demo"

   [[commands]]
   id = "greet"
   title = "Hello: Greet"
   key = "ctrl+alt+g"
   EOF
   cat > "$TERVO_PLUGINS/hello/run.sh" <<'EOF'
   #!/bin/sh
   printf 'plugin=%s api=%s\n' "$TERVO_PLUGIN_NAME" "$TERVO_PLUGIN_API"
   IFS= read -r line
   printf 'stdin=%s\n' "$line"
   printf 'stderr-from-plugin\n' >&2
   exit 0
   EOF
   chmod +x "$TERVO_PLUGINS/hello/run.sh"
   ```

2. 启动 Tervo，日志应出现 `[plugin] 发现 1 个插件（1 个可执行包），注册 1 条命令`；
   设置 → Plugins 显示 `hello v1.0.0 — enabled · api 1 · 1 command(s)`。
3. `Ctrl+Shift+P` 搜 `Hello: Greet` 并回车（或按 `Ctrl+Alt+G`），日志应出现
   `[plugin] plugin.hello.greet exit=Some(0)`、`plugin=hello api=1`、`stdin={"event":...}`
   和 `stderr-from-plugin`；终端输入不卡顿（执行在后台线程）。
4. 把 entry 改成 `exec sleep 30`，触发后约 10 秒日志出现 `超时，已 kill`。
5. 在清单加 `enabled = false`，重启：Palette 不再出现该命令，设置页显示 `disabled`。
6. 把 `entry = "../x"` 或 `api = 2` 写入清单，重启：设置页 skipped +1，
   日志出现对应 `[plugin] 跳过 ...` 原因。
7. 无 GUI 回归：`cargo test -p tervo-app --bin tervo`（manifest/注册/执行/超时/禁用均有单测）。
