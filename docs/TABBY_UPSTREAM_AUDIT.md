# Tabby 上游审计记录（TABBY_UPSTREAM_AUDIT）

> **审计日期：2026-11-05** ｜ 审计对象：Eugeny/tabby（GitHub）与 docs.tabby.sh ｜ 审计人：本仓库产品/上游审计 Agent
> **结论先行：本轮网络不可用，未能抓取上游 README/HACKING/docs/源码，未能取得上游 commit SHA。** 下文只记录“可确认的失败事实 + 既有材料中已核的上游信息（转引）+ 待查清单”，不把需求文档的描述冒充上游证据。
> 本仓库审计基线：起始 HEAD `0ea6231`，审计期间另一 Agent 并发提交 `b80e265`、`94baefb`、`99d7895`、`8327e20`；代码结论以已提交状态为准（链接识别/OSC 8/命令注册表已复核），工作区其余未提交改动未纳入。
> 定位：审计 Tabby 的实现只为理解和借鉴；不代表要复刻、替代或兼容 Tabby。

## 1. 网络访问尝试记录

| # | URL / 动作 | 时间 | 结果 |
|---|---|---|---|
| 1 | `https://raw.githubusercontent.com/Eugeny/tabby/master/README.md` | 2026-11-05 | 失败：`URL hostname "raw.githubusercontent.com" resolves to a non-public IP address` |
| 2 | `https://docs.tabby.sh/` | 2026-11-05 | 失败：`URL hostname "docs.tabby.sh" resolves to a non-public IP address` |
| 3 | `https://api.github.com/repos/Eugeny/tabby/commits/master` | 2026-11-05 | 失败：`URL hostname "api.github.com" resolves to a non-public IP address` |
| 4 | `getent hosts github.com` / `docs.tabby.sh` | 2026-11-05 | 返回 `198.19.0.30` / `198.19.0.51`（非公网地址，DNS 被环境接管） |
| 5 | 查找本地只读上游镜像 `reference/tabby-upstream/` | 2026-11-05 | 不存在；仓库内无上游克隆 |

**因此：**
- 未取得上游 commit SHA；本文不写任何“当前 master SHA 是 …”的猜测。
- 未重新阅读 upstream `README.md`、`HACKING.md`、`tabby-core`/`tabby-terminal`/`tabby-ssh`/`tabby-serial`/`tabby-settings`/`tabby-plugin-manager` 源码。
- 需求文档 E 节列出的 6 个参考入口（README、Features、HACKING、docs、terminal API、tabby-clippy）本轮**全部未复核**。

## 2. 可用的上游信息（转引，非本轮抓取）

以下事实来自本仓库既有文档 [`tabby-themes.md`](tabby-themes.md)；该文档标注“调研对象：Eugeny/tabby `master` 分支（2026-11 抓取）”，并列出了上游文件路径。**本轮未重新抓取，仅作为“已核材料”转引，置信度低于本轮直读源码。**

| 主题 | 已记录事实 | 上游来源（转引） | 用途 |
|---|---|---|---|
| 配置文件 | Linux `~/.config/tabby/config.yaml`；Electron `userData`；`TABBY_CONFIG_DIRECTORY` 覆盖；写 `config.yaml.backup` | `app/lib/index.ts`、`app/lib/config.ts` | C8/C14 配置迁移对照 |
| 配置生命周期 | 启动读一次；设置页 “Save and apply” 走 `save()+load()+emitChange()`；外部改文件通常需重启 | `tabby-core/src/services/config.service.ts` | 配置热加载/迁移策略 |
| 配色数据结构 | `TerminalColorScheme{name,foreground,background,cursor,colors[16],selection?,selectionForeground?,cursorAccent?}` | `tabby-core/src/api/theme.ts`、docs typedoc | C10/C13 配色兼容 |
| 配色配置键 | `terminal.colorScheme` / `terminal.lightColorScheme` 存完整对象；`terminal.customColorSchemes[]`；UI 主题 `appearance.theme`；自定义 CSS 键为 `css` | `tabby-core/src/configDefaults.yaml` 等 | C10 导入/映射 |
| UI 主题 | 内置只有 `Follow the color scheme`；主题插件提供 `Theme` 服务；`Theme.followsColorScheme` 决定 chrome 是否跟随 | `tabby-core/src/index.ts`、`theme.ts`、`themes.service.ts` | C10/THEME-001/007 |
| 终端前端 | 默认 `xterm-webgl`（xterm.js WebGL canvas）；CSS 不能按单元格/ANSI 索引改色 | `tabby-terminal/src/config.ts`、`tabby-themes.md` 分析 | C1 兼容基线 |
| 插件包结构 | npm 包 `main: dist/index.js`；keyword `tabby-plugin`（旧包 `terminus-plugin`）；peer 依赖 `tabby-core`/`tabby-terminal`；default export NgModule，按 multi-provide 注册 | Hype/Noctis 插件实例、`tabby-themes.md` | C13/PLUG-008/012 |
| 社区配色 | Catppuccin / Rosé Pine 不是 npm 插件，只提供 YAML 片段；内置 `tabby-community-color-schemes` 约 200 个 Xresources 方案 | `tabby-community-color-schemes`、社区仓库 | C10/THEME-004 |
| UI 主题入口 | Settings → Window → Theme；仅 `themes.length > 1` 时显示 | `tabby-settings/src/components/windowSettingsTab.component.pug` | B7 Window 设置对照 |

> 这些材料足以支撑“Tabby 配置是 YAML、主题/插件有明确 API 形态”的结论；**不足以**支撑具体 UI 菜单项、快捷键、Tab 关闭生命周期、Pane 拖拽、Quake 行为等断言。

## 3. 待查清单（按上游模块）

| 上游模块/页面 | 必须回答的问题 | 影响矩阵条目 |
|---|---|---|
| `README.md` / Features | 当前版本宣称的 Tab、SSH、SFTP、Serial、Telnet、Zmodem、Quake、插件能力范围；哪些是内置、哪些是插件 | B4/B8/B9/B10/B11/C3–C7/C13 |
| `HACKING.md` / 插件架构 | 插件加载、服务注入、模块生命周期、Angular 依赖边界 | C13/PLUG-001–013 |
| `tabby-core` | 命令/快捷键注册与 multi-chord 格式；Tab 关闭生命周期；Tab 右键菜单项；Tab 重命名触发方式；TabBar 位置；多窗口；Pane zoom/broadcast；命令面板；通知/进度协议 | B1-007、B2-004/006/007/009/010、B3-007/008/011、B4-005、C9、C11、C13 |
| `tabby-terminal` | 搜索是否支持大小写/正则/计数；终端右键菜单完整项；链接识别器；鼠标协议；焦点报告；OSC 52/颜色查询；IME；进度/标题 | B6-004/010/011/013、C1-006/010/011/016、C11-001 |
| `tabby-local` | Local Shell Profile 字段（cwd/env/args/title）；shell integration 方式 | B5-013、C2-003/006 |
| `tabby-ssh` | 认证方式、known_hosts、agent forwarding、X11、ProxyJump/ProxyCommand、端口转发、login scripts、SSH config 导入范围、Vault 关系 | B5-011、B8、C3/C12 |
| `tabby-serial` | 输入模式 Text/Readline/byte/Hex 的精确定义；输出 Hexdump；换行转换；非标准波特率；时间戳 | B9-003/004/005、C6 |
| `tabby-telnet` | IAC 协商实现；是否支持 Zmodem | C7 |
| `tabby-settings` | 设置分类、Hotkeys 录入/冲突、Vault、Config File、插件设置页 | B7、C9 |
| `tabby-plugin-manager` | 安装来源、权限展示、版本检查、插件目录、崩溃诊断 | B11、C13 |
| `tabby-electron` | Quake/全局快捷键、窗口定位、原生主题/Vibrancy、通知、更新 | B10、C11、C14 |
| `tabby-web` | 与桌面版的完成度边界 | C15（明确排除） |
| `tabby-clippy` 示例插件 | Terminal Decorator/Hotkeys/Context Menu/Config/Tab Settings 的实际 API 用法 | C13/PLUG-002/010 |

## 4. 本轮标记为 `needs_verification` 的 21 条

这些条目并非“确认 Tabby 没有”，而是**上游行为/触发方式/协议未核实**，或本仓库行为未经查证；上游审计完成前不得把它们写成“Tabby 已有功能”。

| 类别 | ID |
|---|---|
| 窗口/Tab/Pane/导航 | UI-WIN-007（TabBar 四向位置）、UI-TAB-007（重命名触发）、UI-PANE-007（pane zoom）、UI-NAV-005（Command Palette） |
| Profile | UI-PROF-004（复制/拖排/分组）、UI-PROF-006（当前/新 Tab 打开）、UI-PROF-011（SSH 字段分组） |
| Serial/Telnet | UI-SER-003/004/005/008、SERIAL-004/005/006/009/010、TELNET-004 |
| SSH | SSH-006（SSH config 导入范围） |
| 主题/通知/插件 | THEME-007（旧 CSS 主题策略）、NOTIFY-001（进度协议）、PLUG-013（Electron/原生模块逐插件评估） |

另有 30+ 条 `not_started` 条目在 “Upstream evidence” 列标注了 `U-AUDIT` 或“待核”（如 Tab 拖出、关闭其他、Tab 菜单项、终端右键完整项、pane 拖拽），同样不能当作已确认的上游行为。

## 5. 本轮明确“不写入”的上游断言

以下说法在本轮材料中**没有**可靠来源，已刻意避免写入矩阵：

- “Tabby 的 TabBar 支持上/下/左/右四向位置” —— 未核实，矩阵用 `needs_verification`。
- “Tabby 支持 Pane 放大/还原/拖拽重排” —— 未核实，矩阵用 `needs_verification` 或 `not_started`。
- “Tabby 终端右键菜单固定为 Copy/Paste/Find/Clear/Profile/拆分” —— 未核实完整项。
- “Tabby 搜索支持正则/大小写/匹配计数” —— 未核实，矩阵只写本仓库能力。
- “Tabby 内置 150+ 主题” —— 需求文档提到“曾提到”，本轮未核实当前数量；矩阵只写本仓库 12 套 + 导入能力。
- “某旧插件仍维护/可安装” —— 14 个代表性插件来源与现状均未核实，见 `archive/TABBY_COMPAT_MATRIX_2026-11-05.md`。
- 任何上游 commit SHA —— 未取得，不写。

## 6. 下次审计 SOP（网络恢复后）

1. `git clone --depth 1 https://github.com/Eugeny/tabby /tmp/tabby-upstream`（只读参考目录，不放入本 Cargo workspace）。
2. 立即记录 `git -C /tmp/tabby-upstream rev-parse HEAD` 与抓取日期，写入本文顶部。
3. 按 §3 模块逐项阅读：优先 `tabby-core/src/services/config.service.ts`、`tabby-core/src/api/*`、`tabby-terminal/src/*`、`tabby-settings/src/components/*`、`tabby-plugin-manager/src/*`；UI 行为以 `.pug`/`.ts` 组件为准，API 以 `api/*.ts` 与 docs typedoc 为准。
4. 为每个 `needs_verification` 条目记录：文件路径 + 行号/组件 + 行为结论；能截图的上游 UI 另存 `docs/upstream/`。
5. 回填 `FEATURE_MATRIX.md` 的 Upstream evidence 与 Status，并在本文记录“已核实/仍待查”的变更。
6. 复核 14 个代表性插件：仓库 URL、最近提交、`package.json` keyword/入口/peerDeps、使用的 Tabby API；更新 `archive/TABBY_COMPAT_MATRIX_2026-11-05.md` 的分级。
7. 审计只读：不得把上游代码/资源复制进本仓库（引用路径与结论即可）。

## 7. 参考入口（本轮均不可达）

- https://github.com/Eugeny/tabby
- https://github.com/Eugeny/tabby/blob/master/README.md
- https://github.com/Eugeny/tabby/blob/master/HACKING.md
- https://docs.tabby.sh/
- https://docs.tabby.sh/terminal/
- https://github.com/Eugeny/tabby-clippy
- 本仓库已核材料：[`tabby-themes.md`](tabby-themes.md)
