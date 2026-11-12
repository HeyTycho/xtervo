> **状态更新**：Tervo Plugin API 已升级到 **v3**（只支持 `api = 3` / `host_abi = 3`）。
> v1/v2 不再兼容、不会执行；本文中的 V1/V2 AI/MCP/JSON Lines 设计仅为历史记录。
> 现行实现见 [`docs/PLUGIN_V3.md`](PLUGIN_V3.md)。MCP 是 Tervo 内嵌功能，不是插件能力。

# Tervo 插件系统：目标、运行时、能力类型与市场调研

> 状态：设计稿（2026-11-06）  
> 范围：终端插件/扩展系统，不包含普通主题文件的实现细节。  
> 结论先行：**核心不做插件；插件重点不是 UI，而是连接、终端数据流、命令/工作区、外部集成。**  
> 高频用 Rust/WASM，冷路径用 sidecar；UI 只保留最小状态/命令入口，绝不引入原生 dylib 插件。

## 0. V1 + V2 AI 落地状态（2026-11-09，`feat/ai-panel`）

V1 本设计稿的 Phase A 已按“稳定子集”落地为**外部进程插件 V1**；V2 在 V1 上**只做加法**，
新增 `api = 2` 清单选项、`[contributes.ai]` 与内置 AI Chat 面板，不引入 WASM/wasmtime，
不改变 JSON Lines 协议 V1：

| 设计项 | V1 / V2 实现 |
|---|---|
| Manifest + 权限声明 | `plugin.toml` 新增 `id`/`runtime`/`[permissions]`/`[contributes]`；权限**仅声明/展示**，默认拒绝未实现能力（不授予） |
| Sidecar/外部进程 | `runtime = "process"` 常驻子进程 + JSON Lines；2s 握手、10s 命令超时、崩溃重启上限 2、坏 JSON 禁用；旧 oneshot 行为保留 |
| Command Provider | 命令进 Palette/快捷键；清单静态参数 + `result.text` 回 UI 浮层/活动/日志 |
| Status Provider | 2s 轮询 + 插件主动推送 `text/tooltip/command`；主窗口底部一行最小状态栏，可点击执行命令 |
| Theme Pack | `[contributes] theme` 声明式 TOML，并入现有主题下拉；坏主题不拖垮插件/启动 |
| **API v2 / AI Chat** | `api = 2` 可加 `[contributes.ai]`（id/name/model/description）；内置面板列出插件，经同一 supervisor 发 `ai.chat`（messages + 可选 model），结果一次性追加；120s 超时/取消不 kill 进程；宿主不存/不传 API Key |
| 安全 | 无 shell 拼接、不传 Vault 明文、输出/行长度上限、超时 kill、坏 manifest/JSON/崩溃隔离；插件仍是普通用户进程 |
| UI | 设置页 Plugins 显示 running/restarting/disabled + 错误原因；Settings → Plugins → AI Chat 打开宿主渲染的最小面板（不开放任意 Slint UI） |

未做（按设计延后）：WASM/Component Model、Connection/Middleware/Observer/Linkifier/
Workspace/Profile Provider、安装/市场/签名、权限强制、任意 UI 面板、真实模型 provider
（示例插件只做确定性 echo，真实凭据/网络由插件自己负责）。
协议、清单、AI 契约与测试详见 [`PLUGIN_API.md`](PLUGIN_API.md)。

## 1. 目标

Tervo 的插件系统要满足四件事：

1. **可扩展行为**：连接类型、终端输入/输出中间件、输出观察者、命令、工作区、外部集成，而不是只加按钮/面板。
2. **不拖垮终端**：插件不能出现在 PTY/解析/渲染/IME 的热路径上；不得阻塞 UI。
3. **可隔离**：插件崩溃、死循环、OOM 不能带崩终端；默认最小权限。
4. **可长期维护**：不引入 Rust ABI/dylib 地狱，不让插件和 Tervo 编译器版本死绑。

### 1.1 插件价值排序（UI 不是重点）

Tervo 的插件第一优先级是**改变终端能做什么**，不是**改变终端长什么样**：

| 优先级 | 扩展点 | 价值 |
|---|---|---|
| P0 | Connection Provider | SSH/Docker/K8s/串口/自定义传输 |
| P0 | Terminal Middleware | 输出过滤/编码转换/脱敏/协议扩展 |
| P0 | Terminal Observer | 日志、通知、命令检测、审计、触发器 |
| P0 | Command Provider | Quick Commands、命令面板、广播 |
| P1 | Workspace Provider | 工作区/会话模板/恢复 |
| P1 | Integration Provider | MCP、Git、CI/CD、AI/agent 状态 |
| P1 | Theme Provider | 主题/配色 |
| P2 | UI Extension | 状态栏/Tab 角标/右键项；只做宿主渲染的最小插槽 |

**UI 面板、侧栏、自定义 Slint 组件不是重点**，也不进入 V1。  
插件市场真正有粘性的是连接、数据流、命令工作流和外部工具集成。

### 非目标

- **不追求完整 Tabby/Electron npm 插件兼容**：Tabby 插件可以直接操作 Angular/Electron/xterm.js，Tervo 的 Rust/Slint 模型不同，只能做能力子集。
- **不允许插件写任意 Slint UI**：插件通过宿主定义的 schema/slot 扩展 UI，避免 UI 与插件 ABI 耦合。
- **不把终端核心插件化**：PTY、解析器、渲染、IME、键盘协议、选区、滚动继续内置 Rust。
- **不做原生 `cdylib`/`dlopen` 插件**：ABI 不稳定、崩溃会带崩主进程、无法安全沙箱。
- **不做每帧/每键跨进程调用**：高频能力必须在进程内或内置。

## 2. 运行时架构

```text
┌──────────────────────────────────────────────┐
│ Tervo Core（Rust）                            │
│  PTY / alacritty / render / input / config   │
│                                              │
│  ┌──────────────┐   ┌────────────────────┐  │
│  │ WASM Runtime │   │ Plugin Host API     │  │
│  │ wasmtime     │←──│ commands/hooks/UI   │  │
│  └──────┬───────┘   └─────────┬──────────┘  │
│         │                     │             │
└─────────┼─────────────────────┼─────────────┘
          │                     │
   ┌──────▼──────┐       ┌──────▼──────────┐
   │ WASM 插件    │       │ 外部 sidecar     │
   │ 高频/温路径  │       │ SSH/SFTP/迭代器  │
   └─────────────┘       └─────────────────┘
```

### 2.1 三种插件形态

| 形态 | 运行位置 | 适合 | 语言 | 性能 |
|---|---|---|---|---|
| **Declarative Pack** | 核心解析 | 主题、快捷键、Profile 片段、布局预设 | TOML/JSON | 无调用开销 |
| **WASM Plugin** | wasmtime 沙箱 | linkifier、图像解码、协议过滤、命令、设置 schema | Rust、AssemblyScript、C/Zig | 接近原生 |
| **Sidecar Plugin** | 独立进程 + JSON-RPC | SSH/SFTP、Docker/K8s、同步、自迭代 | Rust/Go/TS/Python | 冷路径，IPC 成本 |

**禁止**：原生动态库插件。

### 2.2 为什么 WASM 是主力

- 进程内调用，避免 IPC 每次几十微秒；
- 沙箱、fuel/内存上限、超时可配；
- AOT 预编译后启动快；
- 语言可扩展，Rust 优先，AssemblyScript 方便 JS 背景作者；
- 崩溃隔离优于 native dylib。

### 2.3 为什么 sidecar 只做冷路径

- 每次 Unix socket 往返 + 序列化，适合命令/连接/文件操作，不适合每帧；
- 独立进程崩溃不拖死终端；
- 可复用现有 SSH/SFTP/Go/Python 生态；
- 可用共享内存优化大块数据传输。

## 3. 支持的插件类型

### 3.1 V1：先做“行为/数据流”，不碰 UI 布局

| 类型 | 能力 | 形态 | 示例 |
|---|---|---|---|
| Connection Provider | 注册连接类型/传输 | sidecar | SSH、Docker、K8s、串口 |
| Terminal Middleware | 终端输出过滤/编码/脱敏 | WASM | 编码转换、日志脱敏、协议扩展 |
| Terminal Observer | 只读观察输出/命令/状态 | WASM | 日志、通知、命令检测、审计 |
| Command Provider | 命令面板/快捷键/广播 | WASM 或声明 | Quick Commands、发送到多 pane |
| Workspace Provider | 工作区/会话模板/恢复 | 声明 + sidecar | “打开前端环境” |
| Integration Provider | 外部工具/服务 | sidecar | MCP、Git、CI、AI/agent 状态 |
| Theme Pack | 颜色/字体预设 | TOML | 主题、Tabby 主题导入 |
| Profile/Keymap Preset | Profile/快捷键片段 | TOML | 开发环境/键位模板 |
| Minimal UI | 状态栏文本、Tab 角标、右键命令项 | 宿主 schema | 构建状态、主机名 |

V1 不开放：自定义 Slint 组件、任意侧栏/面板、直接 PTY 写、无限制文件/网络。

### 3.2 V2：协议、连接与自动化

当前 `api = 2` 只落地了最小的 **AI Chat 进程插件契约**（`[contributes.ai]` + `ai.chat`），
其余 V2 能力仍按下表规划，未宣称已实现：

| 类型 | 能力 | 约束 |
|---|---|---|
| AI Chat Provider（已落地最小版） | 进程插件声明 `[contributes.ai]`，响应 `ai.chat` 的 messages/model | 仅 `runtime="process"`；宿主不存/不传 API Key；120s 超时/可取消；不 fake token 流 |
| Image Decoder | Sixel/IIP/自定义图片格式解码 | 只输出 RGBA + 尺寸；placement 由核心做 |
| Protocol Filter | OSC/DCS/CSI 扩展解析 | 只读/受限写；不能绕过白名单 |
| Connection Transport | SSH/SFTP/串口/自定义 | sidecar + 权限声明 |
| Profile Provider | 动态生成 Profile/凭据引用 | 不直接读 Vault 明文 |
| Completion Provider | 命令补全 | 有超时/取消 |
| Trigger/Automation | 输出匹配 → 动作 | 规则声明 + 受限动作 |
| Settings Schema | 插件配置字段 | 只生成表单描述，不写 UI 代码 |
| Optional Panel | 数据型面板（列表/树/表单） | 宿主渲染；V2 末或 V3，不是重点 |

V2 仍不开放：任意 UI 代码、直接 PTY 写、无限制文件/网络。AI 面板是**宿主内置**的固定
UI，只从清单读取展示元数据，不接受插件提供的 Slint 组件/布局。

### 3.3 V3：市场与兼容层

- 签名插件市场、版本/平台分发、自动更新；
- Tabby 插件**能力子集**兼容层（命令/主题/Profile/连接），不承诺 npm UI 插件；
- 可选 WASM Component Model ABI；
- 可选 native sidecar 市场（仍独立进程）。

### 3.4 永远不做成插件

- 终端解析器；
- 渲染器/GPU/Sixel placement；
- PTY/进程生命周期；
- IME/键盘编码；
- 鼠标/选区/滚动核心；
- 安全边界（Vault 解锁、权限决定）。

## 4. 插件 API 草案

### 4.1 Manifest

```toml
[plugin]
id = "dev.tervo.linkifier"
name = "GitHub Linkifier"
version = "0.1.0"
api = 1
runtime = "wasm"          # wasm | process
entry = "plugin.wasm"
description = "识别 GitHub issue/PR 链接"

[permissions]
clipboard = false
network = false
filesystem = "none"       # none | read-plugin | read-home | custom
process = false
credentials = "none"      # none | profile-ref

[contributes]
commands = ["open-issue"]
linkifier = true
settings = true
status = false
```

### 4.2 Hooks

插件只收到自己声明的事件：

```text
on_start(ctx)
on_command(id, args)
on_linkify(line_view, output_spans)
on_output_filter(chunk) -> FilterResult    # Terminal Middleware，严格权限
on_output_observe(chunk) -> ()             # Terminal Observer，只读
on_status(ctx) -> StatusItem[]             # 最小 UI，可选
on_settings_schema() -> SettingsSchema     # 只生成表单描述
on_profile_provider(...)                   # sidecar
on_workspace_provider(...)                 # 声明/ sidecar
```

### 4.3 热路径数据布局

不要 JSON。WASM 线性内存传 offset/len：

```rust
#[repr(C)]
struct BufferView { ptr: u32, len: u32, generation: u64 }

#[repr(C)]
struct Span { start: u32, end: u32, kind: u8 }
```

宿主预分配 output buffer，插件返回数量；宿主不把 `Vec<String>` 跨边界。

### 4.4 UI 扩展：最小、宿主渲染

**UI 不是插件重点。** V1 只保留：

- 状态栏 item：text/icon/tooltip/command；
- Tab badge：color/text/tooltip；
- 右键菜单 item；
- 命令面板 item；
- 插件配置表单：宿主根据 schema 渲染。

V2 末/V3 才考虑数据型侧栏/面板；仍由宿主渲染，插件只返回列表/树/表单数据。  
插件不能写任意 Slint 组件，不能控制布局/主题/DPR；UI 扩展不进入终端热路径。

### 4.5 AI Chat 契约（api = 2，当前实现）

清单（只列增量字段）：

```toml
api = 2
runtime = "process"            # AI 仅支持常驻进程
entry = "plugin.py"

[contributes.ai]
id = "dev.tervo.ai-echo"       # 可选，缺省回退插件 id/name
name = "AI Echo"               # 可选，面板展示名
model = "echo-1"               # 可选，原样进 payload.model
description = "offline echo"   # 可选，面板说明
```

宿主 → 插件（JSON Lines，协议仍是 V1）：

```json
{"type":"command","id":8,"command":"ai.chat","args":[],
 "payload":{"messages":[{"role":"user","content":"hello"}],"model":"echo-1"}}
```

插件 → 宿主（兼容旧 result schema）：

```json
{"type":"result","id":8,"ok":true,"text":"hello"}
{"type":"result","id":8,"ok":true,"result":{"text":"hello"}}
```

- `messages` 最多 32 条，role 仅 `system`/`user`/`assistant`，每条 ≤16000 字符；
  `model` ≤128 字符；请求总行仍受 64 KiB 协议上限约束（宿主按长度拒绝）。
- 120s 超时/用户 Cancel 只取消本次请求并回错误，不 kill 常驻进程；普通命令仍 10s 超时。
- 面板只支持**一次完整回复**（append 一次），不做假 token 流；后续可扩展为协议事件，
  但 MVP 不宣称 streaming。
- **安全**：插件是普通用户进程，无沙箱；宿主不保存、不读取、不转发 API Key。
  真实插件应在自己的进程内管理凭据（环境变量/自有配置/系统 keyring）。
- 空状态：没有 AI 插件时面板显示 `docs/examples/ai-echo/` 与
  `~/.config/tervo/plugins/ai-echo/` 示例路径，不要求联网。
- 参考插件：`docs/examples/ai-echo/`（确定性 echo、无网络、无密钥）。

## 5. 性能契约

### 5.1 调用分级

| 路径 | 频率 | 允许的插件形态 | 预算 |
|---|---|---|---|
| 按键/输入 | 每键 | 不允许插件 | 0 |
| 终端解析/渲染 | 每帧 | 不允许插件 | 0 |
| Linkifier | 每 dirty line/帧 | WASM，批量 | p99 < 0.5ms/200 行 |
| 图片解码 | 每图 | WASM worker | 不阻塞 UI |
| 状态栏/Tab | 1–10Hz | WASM/声明 | < 0.1ms |
| 命令/设置 | 用户操作 | WASM/sidecar | < 50ms |
| SSH/SFTP/迭代 | 秒级 | sidecar | 不阻塞 UI |

### 5.2 规则

1. **UI 线程不等插件**：提交请求，应用上一帧结果；latest-wins。
2. **批量**：整行/整屏传入，不逐 cell 调用。
3. **零拷贝**：WASM 线性内存 offset/len；进程插件共享内存。
4. **缓存**：行 hash/generation、图片 hash、设置 schema；负缓存。
5. **AOT**：安装时预编译 `.cwasm`，启动直接 deserialize；缓存 key 含插件 hash、host API、wasmtime 版本、CPU/平台。
6. **超时/降级**：单次超预算取消；连续超时自动禁用插件。
7. **背压**：有界队列；只保留最新请求；丢弃过期 generation。
8. **兴趣过滤**：只调用声明了 hook 的插件；支持 early-exit。
9. **启动懒加载**：首屏不加载全部插件；空闲预热。
10. **基准门禁**：每次插件 API 变更跑 criterion + 端到端延迟。

### 5.3 不做的优化

- 不为插件调用做“每帧 JIT”；
- 不让插件直接操作 framebuffer；
- 不让 sidecar 每键往返；
- 不用 JSON 传每帧文本。

## 6. 安全与分发

| 风险 | 对策 |
|---|---|
| 恶意插件读凭据 | 权限声明 + Vault 引用，不传明文 |
| 插件崩溃 | WASM 沙箱/sidecar 隔离；超时/重启 |
| 插件死循环 | fuel/epoch 超时；强制取消 |
| 插件 OOM | 线性内存上限；宿主预算 |
| 插件篡改缓存 | hash + 签名；原子安装 |
| 安装时执行代码 | 只 AOT 编译 WASM，不跑 build.rs |
| 供应链 | 签名索引、版本锁定、可回滚 |

安装流程：

```text
下载 .wasm / sidecar
→ 校验 sha256 + 签名
→ 检查 host API/平台
→ wasmtime 预编译 .cwasm
→ 原子写入缓存
→ 注册 manifest
```

失败不能阻止 Tervo 启动；插件管理器显示禁用原因。

## 7. 市场调研：终端插件都在做什么

> 目的不是照抄，而是判断用户期待和 Tervo 的差异化。不同终端的插件模型差异很大。

### 7.1 Tabby（Electron + xterm.js，npm 插件）

Tabby 是最接近 Tervo 的对标：插件是 JS/TS 模块，可注册连接类型、命令、UI 组件、设置页、Profile 选项。

典型能力：

- **连接**：SSH、SFTP、串口、RDP、Docker、Kubernetes、端口转发；
- **工作流**：Quick Commands、命令面板、登录脚本、post-connect actions；
- **终端增强**：链接识别、命令跳转（jumper）、文本高亮、编码切换、剪贴板图片粘贴、Shift+Enter/反斜杠换行；
- **UI**：主题、Tab 颜色/进度、状态栏、可拖拽快捷命令按钮；
- **集成**：配置同步、Vault、AI/agent 状态、MCP 类连接。

代表插件（npm/社区，名称仅作市场例子）：

```text
tabby-k8s
tabby-clipboard-image
tabby-encoding
tabby-jumper
tabby-advanced-highlight
tabby-quick-cmds-buttons-modern
tabby-post-connect-actions
tabby-ssh-auth-socket
tabby-ssh-keymap
tabby-rdp（RDP helper）
tabby-fit-screen
```

Tabby 的优势是 Electron 里什么都能做；代价是包体、内存、启动、原生集成弱。Tervo 不应该复制其运行时，而应复制**能力分类**。

### 7.2 Hyper（Electron/JS）

插件主要是 UI 和终端外观：

```text
主题、透明度、边框、Tab、搜索、cwd 同步、窗口 summon、保存窗口
```

说明“主题/外观/快捷操作”是插件市场的基础盘，但技术门槛低、可替代性强。

### 7.3 iTerm2（macOS，Python API）

不是 npm 市场，而是 Python 脚本/触发器：

```text
状态栏组件、触发器（输出匹配→动作）、自动 Profile 切换、
shell integration、tmux 集成、图片协议、AppleScript 自动化
```

值得借鉴：**触发器/自动化**和 **shell integration** 是高频生产力功能，但需要稳定的输出事件 API。

### 7.4 kitty（kittens，Python）

kitty 的“插件”更像内置/外部 kitten：

```text
hints（URL/路径/哈希选择）
icat（图片）
clipboard
ssh
transfer（文件传输）
broadcast（多 pane 同步输入）
choose-files
hyperlinked_grep
themes
```

值得借鉴：**hints/链接选择、文件传输、广播输入** 是终端原生能力，不一定要做成第三方插件。

### 7.5 Zellij（WASM 插件）

Zellij 的 WASM 插件模型最接近 Tervo 想做的方向：

```text
status bar、tab bar、session manager、strider（文件浏览）、
compact bar、配置/布局插件
```

权限/沙箱/版本化 ABI 是重点；说明 **WASM 插件做 UI 数据源是可行的**，但需要宿主提供组件模型。

### 7.6 tmux / WezTerm（脚本生态）

- **tmux**：TPM 插件，resurrect（会话恢复）、continuum、fzf、logging、copycat、yank；
- **WezTerm**：Lua 配置/插件，resurrect、tabline、workspace、command palette、smart splits。

说明：**会话恢复、命令面板、workspace、状态栏**是终端用户长期高频需求。

### 7.7 没有插件市场的终端

- Alacritty：无插件 API；
- GNOME Terminal / Windows Terminal：无第三方代码插件；
- Warp：workflows/themes，不是开放插件。

这给 Tervo 的机会：**原生性能 + 受控插件 + 自迭代**，而不是再做一个 Electron 插件市场。

### 7.8 市场功能归类

| 类别 | 用户期待 | Tervo 策略 |
|---|---|---|
| 连接/传输 | SSH/SFTP/串口/RDP/Docker/K8s/端口转发 | sidecar 插件 + 核心连接 API |
| 会话/工作区 | 恢复 Tab/分屏/cwd、workspace、session manager | 核心功能，不做插件 |
| 生产力 | Quick Commands、命令面板、跳转、触发器、广播 | 核心 + 声明式/WASM |
| 终端增强 | 链接、图片、编码、高亮、剪贴板图片 | 核心 + WASM |
| 外观 | 主题、Tab 颜色、状态栏、透明度、背景 | 声明式 Pack |
| 集成 | Git、AI/agent 状态、同步、MCP | 状态 provider/sidecar |
| 自动化 | 登录脚本、post-connect、输出触发 | 声明式规则 + sidecar |
| 自迭代 | 轮次、测试、截图、批准 | 外部 `tervo-iterate` |

### 7.9 Tervo 应优先覆盖的 8 个扩展点

结合 Tabby/kitty/Zellij/tmux 的市场功能，Tervo 第一版 Plugin API 按以下顺序设计：

| API | 典型用途 | 首版 |
|---|---|---|
| Connection Provider | SSH、Docker、Kubernetes、串口 | ✅ |
| Terminal Middleware | 输出过滤、编码转换、脱敏、协议扩展 | ✅ |
| Terminal Observer | 日志、通知、命令检测、审计 | ✅ |
| Command Provider | Quick Commands、命令面板、广播 | ✅ |
| Workspace Provider | 工作区、会话模板、恢复 | ✅ |
| Integration Provider | MCP、Git、CI/CD、AI/agent | ✅ |
| Theme Provider | 主题、配色、字体预设 | ✅ |
| UI Extension | 状态栏、Tab 角标、右键项；面板延后 | 最小 |

用 4 个真实插件验证 API，而不是先做 UI 框架：

1. Quick Commands（多 pane 广播）
2. Highlight/Observer（输出高亮/日志）
3. Docker/K8s Connector（sidecar 连接）
4. MCP/Integration Provider（外部工具）

**市场判断：主题和 UI 插件数量多，但差异化低；连接管理、终端数据流、命令工作流和外部集成才是 Tervo 值得投入的部分。**

## 8. Tervo 分阶段路线

### Phase A：P0 稳定后（V1，行为优先）

```text
1. Plugin manifest + 权限声明
2. Connection Provider（sidecar 协议）
3. Terminal Middleware / Observer（WASM，批量/只读）
4. Command Provider（命令面板/广播）
5. Workspace Provider（声明式）
6. Integration Provider（MCP/Git/CI 的最小适配）
7. Theme/Profile/Keymap 声明包
8. AOT 预编译缓存
```

验收：插件崩溃/超时不影响输入；middleware p99 达标；命令/连接可禁用恢复；UI 只出现状态/命令入口。

### Phase B：V2

```text
1. Image decoder WASM 接口（Sixel/IIP）
2. Protocol Filter（严格权限）
3. Sidecar JSON-RPC + 共享内存
4. SSH/SFTP/连接传输插件 API
5. Trigger/automation 规则
6. 设置表单 schema
7. 数据型面板（最后，非重点）
```

验收：图片解码不阻塞 UI；sidecar 崩溃可重启；权限拒绝可解释；新增 UI 不阻塞终端热路径。

### Phase C：V3

```text
1. 签名市场/安装/更新/回滚
2. Tabby 能力子集兼容
3. 可选 WASM Component Model
4. 性能/安全审计工具
```

### 自迭代插件

`tervo-iterate` 是**外部 sidecar**，不是终端内插件：

```text
tervo-iterate
  ├─ planner（只读 Codex/DSH）
  ├─ worker（worktree）
  ├─ verifier（不可变测试）
  └─ release/rollback
```

Tervo 只提供可选 UI 插件显示轮次/测试/批准，不让 UI 进程改自己的二进制。

## 9. 开放问题

1. WASM Component Model 还是自定义 ABI？
2. 是否支持 AssemblyScript 作为一等语言？
3. 插件市场签名/托管由谁负责？
4. Tabby 兼容层做到命令/主题/Profile 还是也做 UI？
5. 是否允许插件声明自定义协议解析（安全边界）？
6. 多插件链式 linkifier 的优先级/冲突规则？
7. 插件 UI 插槽的版本化/废弃策略？

## 10. 相关文档

- [`FEATURE_MATRIX.md`](FEATURE_MATRIX.md)
- [`self-iteration.md`](self-iteration.md)
- [`STABILITY_REPORT.md`](STABILITY_REPORT.md)
- [`tabby-parity.md`](tabby-parity.md)
- [`themes.md`](themes.md)
- [`LOGGING.md`](LOGGING.md)

## 11. 参考

- Tabby: <https://github.com/Eugeny/tabby>
- Hyper: <https://github.com/vercel/hyper>
- iTerm2 Python API: <https://iterm2.com/python-api/>
- kitty kittens: <https://sw.kovidgoyal.net/kitty/kittens_intro/>
- Zellij plugins: <https://zellij.dev/documentation/plugins>
- tmux plugins: <https://github.com/tmux-plugins>
- WezTerm: <https://wezterm.org/>
