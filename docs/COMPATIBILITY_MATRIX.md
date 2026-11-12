# 旧 Tabby 插件兼容矩阵（COMPATIBILITY_MATRIX，初版）

> 审计日期：2026-11-05 ｜ 代码基线：`8327e20` ｜ 上游：**未核实**（网络不可用，见 [`TABBY_UPSTREAM_AUDIT.md`](TABBY_UPSTREAM_AUDIT.md)）
> 本文回答需求 C13：新插件 API 与旧 Tabby 插件的兼容分级。**初版基线时仓库没有插件宿主**；后续已合入 Plugin API v3 process 宿主，并新增窄 WASM `terminal.output_filter`（见 `PLUGIN_V3.md` 第 6 节）。但本文的 Tabby/Node/WebView 兼容分级仍是旧基线，未重新审计。

## 1. 结论

1. **本文初版的新插件 API 基线为 0/6**。后续已有 v3 process 宿主与窄 WASM 输出过滤器；WIT/任意 WASM Host API 仍未实现。本文不重写旧 Tabby 评级。
2. **旧插件加载：0 条链路**。无 `package.json` 扫描、无 `tabby-plugin` keyword 识别、无 Node.js sidecar/RPC、无 `tabby-core`/`tabby-terminal` shim、无 WebView UI Host。
3. **不能承诺全量兼容**。需求 C13 明确“不要承诺全量兼容”；Angular Component/Template/DI 不能由 Slint 执行，依赖 Electron 私有 API/原生 `.node` 模块的插件预期为 D 级。
4. **14 个代表性插件全部 `needs_verification`**：来源仓库、最近维护状态、实际使用的 API 均未核实；当前只能给出“若做桥接，预计需要哪些宿主能力/风险面”。
5. **可复用的数据兼容**：配色（16 色对象）是唯一已具备数据层兼容基础的部分——`themes.rs` 已能导入 iTerm2/WT/Alacritty/kitty/Xresources/YAML，但尚无 `TerminalColorSchemeProvider` 插件接口。

## 2. 兼容等级定义

| 等级 | 含义 | 判定标准 |
|---|---|---|
| **L0** | 当前宿主完全无法加载 | 无插件 API/运行时/包扫描；任何插件都加载失败 |
| **L1** | 纯数据可兼容 | 插件只提供配色/配置/静态数据，可被 Rust 直接解析，无 JS 执行 |
| **L2** | Node sidecar + API shim 可兼容 | 非 UI 逻辑可在 Node 子进程运行，通过 RPC 调 Rust Host；需 Commands/Config/Terminal stream 等接口 |
| **L3** | 需要可选 WebView UI Host | 插件含 Angular 组件/模板/DI 或依赖 DOM，必须在隔离 WebView 中渲染 |
| **L4** | 明确不支持 | 依赖 Electron 私有 API、原生 Node 模块、未公开内部 API，或无法在 Slint/WebView 边界内安全实现 |
| **D** | 不建议兼容 | 安全/维护/权限风险高于收益 |

> 目标等级是“若投入桥接工作，理论上可达到的等级”，不是承诺；需上游审计后逐插件确认。

## 3. 新插件 API 就绪度（C13-001–006）

| ID | 能力 | 当前实现 | 状态 | 阻塞项 |
|---|---|---|---|---|
| PLUG-001 | 稳定、版本化、带生命周期的 Plugin API | 无 | not_started | 需先定 Host API 版本策略/生命周期 |
| PLUG-002 | 12 类扩展点（Commands、Hotkeys、Toolbar、Settings、Profile Providers、Tab/终端菜单、Decorator、Stream Processor、Middleware、Color Scheme Provider、Linkifier） | 无 | not_started | 宿主侧多数能力本身未实现（命令/设置/工具栏/连接均缺） |
| PLUG-003 | 注册/注销资源、事件订阅、异步任务管理 | 无 | not_started | 无插件生命周期/取消令牌 |
| PLUG-004 | 权限声明：terminal read/write、fs、network、clipboard、secret、process | 无 | not_started | 无 capability 模型/授权 UI |
| PLUG-005 | 崩溃隔离、超时、日志、升级/版本兼容检查 | 无 | not_started | 无运行时/看门狗/诊断 |
| PLUG-006 | 新插件优先 WASM/WIT（先 Host API 后 Runtime） | 无 | not_started | 未做 Wasmtime/WIT 原型；Cargo 无依赖 |

## 4. 旧插件 API 面兼容分级

| 上游 API 面 | 典型用途 | Rust 宿主现状 | 当前等级 | 目标等级 | 需要的工作/阻塞 |
|---|---|---|---|---|---|
| `tabby-core` Config | 读写配置、注册配置 schema | 有版本化 TOML（`config.rs`），但无插件配置命名空间/事件 | L0 | L2 | YAML→TOML 映射、插件配置分区、变更通知 |
| `tabby-core` Commands/Hotkeys | 注册命令、快捷键、上下文 | 快捷键硬编码，无命令注册表 | L0 | L2 | 先做 C9 命令/快捷键系统 |
| `tabby-core` Theme / ColorSchemeProvider | 注册 UI/终端配色 | 12 内置 + 6 格式导入；无 provider 接口 | L0 | L1/L2 | 数据层可复用；需 provider 注册与命名空间 |
| `tabby-core` Notifications | 通知/进度 | 仅 `CoreEvent::Bell/Exited`，UI 未消费 | L0 | L2 | 先做 C11 通知总线/权限 |
| `tabby-core` Toolbar/Tab 菜单/终端菜单 | 注册 UI 项 | 菜单/工具栏均硬编码，无扩展点 | L0 | L2/L3 | 需 UI 扩展点 + 权限；Angular UI 项需 WebView |
| `tabby-terminal` Session/Terminal | 读写终端流、发送输入 | `PaneHandle` 有 input/copy/search，但无公开插件 API | L0 | L2 | 需稳定的 Session 抽象（当前 Session 与 Pane 同生命周期） |
| `tabby-terminal` Middleware/Stream Processor | 处理输出流、拦截/转换 | 无流处理器链 | L0 | L2 | 需在 actor 解析前后加 hook；注意高吞吐不得阻塞 |
| `tabby-terminal` Decorator/Linkifier | 高亮/链接识别/装饰 | 无 hyperlink 数据、无装饰器 | L0 | L2/L3 | 需 Frame 携带链接/样式扩展与渲染 hook |
| `tabby-terminal` Profile Provider | 提供 SSH/Serial/Docker 等连接类型 | 无 Profile/Provider 模型 | L0 | L2 | 先做 B5/C8 Profile 模型 |
| `tabby-local` | Local Shell Profile | 仅全局 shell + CLI args | L0 | L2 | 需 B5-013 Profile 字段与 UI |
| `tabby-ssh` | 认证/跳板/转发/Vault | 无 SSH | L0 | L2 | 先做 C3；安全边界必须原生实现 |
| `tabby-serial` | 串口参数/模式/时间戳 | 无 Serial | L0 | L2 | 先做 C6；`serialport` 依赖未加入 |
| `tabby-settings` Angular 设置页 | 插件设置 UI | 无 WebView/设置扩展点 | L0 | L3 | 必须 WebView UI Host；默认不启动 |
| `tabby-plugin-manager` | 安装/权限/版本 | 无插件管理器 | L0 | L2 | 需包扫描、签名/校验、权限 UI |
| `tabby-electron` 私有 API | 窗口/原生能力 | 非 Electron；无对应 API | L0 | L4 | 预期不支持；逐插件确认 |
| Node 原生 `.node` 模块 | 原生扩展 | 无 Node/ABI 兼容层 | L0 | L4 | 预期不支持；sidecar 也需匹配 ABI/平台 |
| Angular Component/Template/DI | 插件 UI | Slint 无法执行 Angular | L0 | L3 | 仅隔离 WebView；不得默认拉起浏览器运行时 |

## 5. 代表性旧插件逐项分级（需求 C13 清单）

> “上游形态/维护状态”均未核实；插件名按需求文档原文，不代表已确认仓库存在或仍在维护。影响面指“若用户安装但宿主不支持，会缺失什么能力”。

| ID | 插件/能力 | 预期功能 | 依赖的上游 API/能力 | 当前等级 | 目标等级（若做桥） | 影响面 | 主要风险/无法兼容原因 | 状态 |
|---|---|---|---|---|---|---|---|---|
| LEGACY-001 | Docker 容器连接 | 列出 Docker 容器并作为连接打开 | Profile Provider、Session、进程/网络 | L0 | L2 | 无 Docker Profile/连接入口 | 需 Docker socket 权限与安全边界；sidecar 进程管理 | needs_verification |
| LEGACY-002 | title-control | Tab 标题前缀/后缀/字符串过滤 | Tab 标题 API/事件、配置 | L0 | L2 | 无法自定义 Tab 标题 | 当前标题只活动 Tab 首 pane 更新；需先修 B2-008 | needs_verification |
| LEGACY-003 | quick-cmds | 向一个/多个 Tab 快速发送命令 | Commands、Session write、Tab 枚举 | L0 | L2 | 无法批量发命令 | 写终端是高风险权限；需广播/范围提示；当前无命令系统 | needs_verification |
| LEGACY-004 | save-output | 保存终端输出到文件 | Terminal stream/read、fs | L0 | L2 | 无法导出会话输出 | 需流式 hook（不能阻塞解析）＋文件权限；C1-015 未实现 | needs_verification |
| LEGACY-005 | sync-config | 配置同步（远端/文件） | Config 读写、网络、secret | L0 | L2 | 无法同步配置 | 触及凭据/网络安全；当前无导入导出/同步 | needs_verification |
| LEGACY-006 | workspace-manager | 保存/恢复 Workspace/Profile 模板 | Tab/Pane/Session 创建、配置 | L0 | L2/L3 | 无 Workspace 模板管理 | 当前只有匿名 Restart 快照，无命名 Workspace | needs_verification |
| LEGACY-007 | search-in-browser | 用浏览器搜索选中文本 | Clipboard read、process/网络 | L0 | L2 | 无法右键“浏览器搜索” | 需剪贴板权限 + 外部进程/URL 打开；安全提示 | needs_verification |
| LEGACY-008 | sftp-tab | 独立 SFTP Tab | SFTP 后端、Tab UI | L0 | L3 | 无 SFTP 能力 | 原生 SFTP 尚未实现；UI 插件需 WebView 或原生等价 | needs_verification |
| LEGACY-009 | background | 背景图/背景样式（CSS） | Electron DOM/CSS、UI Theme | L0 | L3/L4 | 无法应用背景图/样式 | Slint 无 DOM/CSS；只能 WebView 或原生重做背景层；预期 D 级 | needs_verification |
| LEGACY-010 | highlight | 终端关键词高亮 | Terminal Decorator/流处理、渲染 hook | L0 | L2/L3 | 无关键词高亮 | 需 decorator/样式扩展；当前 Frame 无链接/装饰数据 | needs_verification |
| LEGACY-011 | web-auth-handler | 处理 Web Auth 弹窗（OAuth 等） | Electron `webContents`/窗口 | L0 | L4 | 无 Web Auth 弹窗处理 | 依赖 Electron 私有/WebView 能力；无 Electron 运行时 | needs_verification |
| LEGACY-012 | mcp-server | MCP Server 集成（外部工具调用） | 进程/网络、Commands、Session | L0 | L2 | 无 MCP 集成 | 外部进程/权限/工具调用安全模型；需 capability | needs_verification |
| LEGACY-013 | ssh-keymap | SSH Key 名称→本地路径映射 | SSH 认证/Profile、fs | L0 | L2 | 无 SSH key 映射 | SSH 未实现；密钥路径/权限需安全设计 | needs_verification |
| LEGACY-014 | serial-timestamp | 串口输出时间戳 | Serial 后端、Stream Processor | L0 | L2 | 无串口时间戳 | Serial 未实现；需流处理 hook 与时间戳格式 | needs_verification |

## 6. 插件包识别/加载检查清单（当前全部未实现）

| 检查项 | 目的 | 当前实现 | 状态 |
|---|---|---|---|
| `package.json` 存在与 JSON 解析 | 识别 npm 包 | 无 | not_started |
| `keywords` 含 `tabby-plugin`（旧包 `terminus-plugin`） | 判定 Tabby 插件 | 无扫描器；keyword 事实转引 `tabby-themes.md` | not_started |
| `main`/`module` 入口与依赖树 | 定位入口、判断 Node 依赖 | 无 | not_started |
| `peerDependencies` 中 `tabby-core`/`tabby-terminal` 版本 | 兼容 API 版本 | 无 | not_started |
| Angular `NgModule`/装饰器检测 | 预判是否需要 WebView | 无 | not_started |
| `require('electron')`/原生 `.node` 检测 | 预判 L4 | 无 | not_started |
| 签名/哈希/来源校验 | 安装安全 | 无 | not_started |
| 权限声明读取与展示 | 最小权限 | 无 | not_started |
| 卸载后配置清理/保留策略 | 生命周期 | 无 | not_started |

## 7. 旧插件兼容验收基线（未来实现时必须满足）

1. **可安装性**：给定一个 fixture 插件包，能识别 keyword/入口/版本，非法包给出可读错误。
2. **隔离性**：Node sidecar 崩溃/超时不得拖垮主进程；超时后强制终止并记录。
3. **权限**：默认无 terminal read/write、fs、network、clipboard、secret、process 权限；越权调用返回明确错误并记录。
4. **兼容范围**：L2 插件至少能注册 1 个命令 + 1 个配置项 + 1 个 Terminal Middleware；L3 插件仅在显式启用 WebView 时加载。
5. **不启动浏览器**：未启用 UI 插件时进程树中不得出现 Node/WebView 子进程。
6. **回归**：每个代表性插件至少 1 个 fixture/最小复现；不能只写“兼容 tabby-core”。
7. **性能**：终端字节流不得经过插件 RPC；高频路径只允许本地 Rust hook，插件流处理必须有界/可丢弃策略（需求 A3）。

## 8. 结论与建议顺序

- 现在**不要**开始逐个适配旧插件：宿主侧 Commands、Profile、Settings、Session、Terminal stream 都还没有稳定接口，适配会反复返工。
- 建议顺序：**新插件 API 设计（WIT/Host API）→ 命令/快捷键/配置/主题 provider 四个最小扩展点 → 插件管理器 UI → Node sidecar + shim → 14 个代表性插件分级验证**。
- 旧插件兼容以“分级 + fixture + 权限”呈现，不以“支持 Tabby 插件”一句概括；无法兼容的（Electron 私有 API、原生模块、纯 Angular UI）逐项写明原因。
