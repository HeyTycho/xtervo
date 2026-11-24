# xtervo 文档

> 这里是详细文档的入口。根目录 [`../README.md`](../README.md) 只保留简介、主要功能与安装使用；
> 设计、矩阵、报告、历史归档都在本目录。

## 开始使用

| 文档 | 说明 |
|---|---|
| [怎么验证 xtervo（验收手册）](VERIFICATION.md) | 数字复核、构建/测试/soak 门禁、三个环境卡点的逐步验证、功能冒烟清单 |
| [xtervo 架构](ARCHITECTURE.md) | 状态：目标架构 + 当前实现边界。 |
| [xtervo 产品原则与路标](PRODUCT_PRINCIPLES.md) | 状态：产品北极星文档。实现细节可以变，下面的定位、非目标、决策优先级 |
| [xtervo UI / 视觉设计规范](UI_DESIGN.md) | 终端是纸，界面是页边距。 |
| [许可证与合规说明（Licensing）](LICENSING.md) | UI 依赖 Slint 1.18，其许可证为： |

## 使用指南

| 文档 | 说明 |
|---|---|
| [xtervo 插件 v3：本地安装 / 更新 / 回滚 / 签名校验](PLUGIN_INSTALL.md) | 本文只覆盖本地插件包管理，不包含托管市场、云同步、在线 key 分发或信任链。 |
| [xtervo 插件系统：目标、运行时、能力类型与市场调研](PLUGIN_SYSTEM.md) | V1 本设计稿的 Phase A 已按“稳定子集”落地为外部进程插件 V1；V2 在 V1 上只做加法， |
| [xtervo 外部进程插件 API（V1 + V2 AI 扩展，api = 1/2）](PLUGIN_API.md) | 状态：V1 稳定子集（2026-11-06）+ V2 AI Chat 扩展（2026-11-09）。本轮在已有 |
| [xtervo Plugin API v3](PLUGIN_V3.md) | 状态：v3 是 xtervo 唯一受支持的插件协议。宿主只加载 api = 3 且 |
| [传输插件（Transport Plugin）设计](PLUGIN_TRANSPORT.md) | 状态：进行中（2026-11-17）。POC 第一步已落地：crates/core/src/transport.rs |
| [SSH / SFTP / Vault 产品化（W-02）](SSH_SFTP.md) | profiles.toml v2 新增 additive 字段（旧文件缺字段自动补空/false，无需迁移）： |
| [官方 SSH 插件 POC（ACT-052 F5）](SSH_PLUGIN_POC.md) | 状态：POC 可用（2026-11-17）。目标是在已有 transport seam 上跑通 |
| [Vault：加密凭据存储与 SSH 密码 askpass（MVP）](VAULT.md) | - 加密文件 ~/.config/xtervo/vault.enc（XDG_CONFIG_HOME 生效；XTERVO_VAULT=/path 可覆盖）。 |
| [MCP 桥：离线诊断 + live terminal control](MCP.md) | 配置是 flat TOML（~/.config/xtervo/config.toml，可用 XTERVO_CONFIG 覆盖）： |
| [MCP Terminal Control](MCP_TERMINAL_CONTROL.md) | Status (2026-11-13): MCP is enabled by default but read-only by default. |
| [xtervo 诊断 CLI（harness-neutral）](DIAGNOSTICS.md) | 状态：阶段 A 已落地（2026-11-10）。 |
| [日志与诊断](LOGGING.md) | xtervo 默认把 stderr（包括 panic）同时写入按次启动的日志文件，排查“右键没反应 / 焦点不对 / 窗口挡住”时先看日志。 |
| [稳定性 soak（W-07）](SOAK.md) | 可复跑的长时稳定性压测：在独立 Xvfb 上持续制造输出/交互/图片/多 Tab/resize 压力， |
| [xtervo 版本与发布管理](RELEASING.md) | 一个版本 = 一个 SemVer 号 + 一个不可变的 annotated tag + 一组可复现产物。 |
| [键盘 / IME 已知缺口（TERM-016）](KEYBOARD_LIMITATIONS.md) | - Slint 的 KeyEvent.modifiers 只有 ctrl/alt/shift/meta，没有锁键。 |
| [工作区壳：Sidebar / Toolbar / 状态区（ACT-046 + ACT-041 前置）](WORKSPACE_SHELL.md) | 状态：进行中（2026-11-17）。已完成 ① config 字段 ② 侧栏骨架 ④ 设置页开关 ⑤ 状态区 pill 行 ③ 右边缘拖宽 + 宽度持久化 + Profiles 列表（复用下… |
| [xtervo Observability（P0）](OBSERVABILITY.md) | 状态：backend-neutral crate + CLI schema 已落地（2026-11-10）。 |
| [xtervo AI 诊断器（不是聊天框）](AI_DIAGNOSTICIAN.md) | 状态：方向文档（2026-11-11）。 |
| [Feedback / Bug Reports](FEEDBACK.md) | xtervo has a built-in feedback path in Settings → Help → Feedback. |

## 设计与内部

| 文档 | 说明 |
|---|---|
| [Vendored alacritty_terminal RowId patch](ALACRITTY_ROW_ID.md) | vendor/alacritty_terminal is upstream alacritty_terminal 0.26.1 with one focused change: |
| [内联图片合成层 v2](IMAGE_COMPOSITING.md) | 当前问题不是单点 bug，而是合成模型错误： |
| [Image Engine Contract (v1)](IMAGE_ENGINE_CONTRACT.md) | This file is the frozen interface for the image-engine integration. Implementations may not |
| [字体回退、彩色 emoji 与宽字符渲染](FONT_FALLBACK.md) | 内置候选（按用途分组；只有相关组会被真正解析）： |
| [多显示器 / dock 支持](MULTI_MONITOR.md) | 状态：基础支持（2026-11-10）。 |
| [xtervo 多窗口目标架构](MULTI_WINDOW_ARCHITECTURE.md) | 状态：Phase 1/2 已实现（feature flag） |
| [xtervo 多窗口 Phase 1/2 实现状态](MULTI_WINDOW_RUNTIME.md) | 状态：Phase 1/2/3/4 已完成并合入 main：默认单实例、同进程多窗口、session v2、共享完整回调、插件/MCP window_id。 |
| [GPU Damage Scissor Plan (0.4.0 blocker)](GPU_DAMAGE_PLAN.md) | Status: implementation pending (2026-11-12). Current RC6 only reuses the |
| [GPU 渲染性能目标与基线（2026-11-11）](GPU_PERFORMANCE.md) | 状态：stage 0 已可运行；本文定义“做到最好”的验收，不把 stage 0 当完成。 |
| [平台抽象与 GPU 渲染路线](PLATFORM_GPU.md) | Slint 自身已通过 slint 的 backend-winit 处理窗口/输入/IME；crates/app/Cargo.toml |
| [xtervo 渲染终极目标与路线图](RENDERING_ROADMAP.md) | 状态：规划基线（2026-11-11，feat/image-engine @ 1435187）。 |
| [PaneHandle 接线计划（ACT-052 第四步，18:00 起执行）](TRANSPORT_WIRING.md) | 状态：计划（2026-11-17 15:30）。前三步已落地：PaneTransport trait、 |
| [UI Shell：标签页 / 分屏 / 右键菜单](ui-shell.md) |  |
| [xtervo UI 交互矩阵（UI_INTERACTION_MATRIX）](UI_INTERACTION_MATRIX.md) | 1. 窗口壳：启动→拖空白→双击→最小化→任务栏恢复→最大化→拖边/角→关闭；每一步截图并记录子进程数。 |
| [终端基础行为：可听铃 / 滚动策略 / 滚动条 / Backspace・Delete](terminal-behavior.md) | 所有设置都在 Settings → Terminal，写入 ~/.config/xtervo/config.toml 并即时生效 |
| [终端菜单与查找：Match case / regex、Read Only、Reset、Save Contents](terminal-menu-find.md) | crates/core/src/pane.rs： |
| [终端 UI 基础能力：滚动条 / 视觉铃 / 拖放](terminal-ui-basics.md) | 此前 PaneHandle 只有 scroll(lines)，UI 无法知道当前回滚位置与总历史长度。 |
| [主题：内置、自定义与直接下载现成配色](themes.md) | 设置面板（⚙）里可选 12 套，终端 16 色 + 标签栏/菜单/设置面板/搜索框同步换色： |
| [Tabby 主题机制调研与 xtervo 主题方案](tabby-themes.md) | Tabby 把「界面外观」和「终端配色」拆成两套独立机制，另有插件与 customCSS 两个扩展口。配置文件是 YAML：Linux ~/.config/tabby/config.yaml，… |
| [稳定交互基线（窗口 / TabBar）](STABLE.md) | 日期：2026-11-06（polish 合并后）。以下行为已经验证过，后续改动不要直接在 master 上实验： |
| [旧 Tabby 插件兼容矩阵（归档，历史评估）](archive/TABBY_COMPAT_MATRIX_2026-11-05.md) | 1. 本文初版的新插件 API 基线为 0/6。后续已有 v3 process 宿主与窄 WASM 输出过滤器；WIT/任意 WASM Host API 仍未实现。本文不重写旧 Tabby 评… |
| [Tabby 上游审计记录（TABBY_UPSTREAM_AUDIT）](TABBY_UPSTREAM_AUDIT.md) | 因此： |
| [Tabby 插件生态参考笔记（不做兼容目标）](TABBY_PLUGIN_REFERENCE.md) | 不能在 xtervo 里直接加载 Tabby 的 npm 插件。 |
| [Tabby 功能参考（灵感清单，非替代目标）](tabby-reference.md) | 把 Tabby 的公开功能面当作参考清单，用来看 xtervo 自己还缺什么；不追求替代或逐项对齐。 |

## 状态与报告

| 文档 | 说明 |
|---|---|
| [xtervo 功能矩阵（FEATURE_MATRIX）](FEATURE_MATRIX.md) | - 并发说明：审计期间另一 Agent 提交了 b80e265（config schema/迁移/备份、字体惰性加载等），本矩阵已按该提交复核 config 相关行；此后 94baefb/99… |
| [功能矩阵闭环报告](CLOSURE.md) | 加权分 = (implemented + partial×0.5 + needs_verification×0.25) / 条目数， |
| [行动清单（ACTION_ITEMS）](ACTION_ITEMS.md) | 以下功能代码已存在且有部分记录，不要重写；先按验收步骤补证据，缺口用最小补丁修（标“需小修”的才动代码）。 |
| [xtervo Mainline Status / Mainline Audit](MAINLINE_STATUS.md) | Snapshot: 2026-11-16, branch main, HEAD 6102bb9 (v0.5.0, stable multi-window) |
| [性能与打包基线（xtervo）](PERFORMANCE.md) | 本文记录可复现的性能/体积数据与门禁。所有数字都在本仓库的开发机上测得 |
| [稳定性验收报告（feat/stability）](STABILITY_REPORT.md) | - 日期：2026-11-06（截止 18:00） |
| [范围审视：什么算核心，什么可以不做](SCOPE_REVIEW.md) | TERM / UI-TERM / KEY / UI-PANE / UI-TAB / UI-WIN / LOCAL / SESS |
| [核心终端从 69% 到 80%：17 条清单与今晚排班](CORE_TO_80.md) | text |
| [本轮夜班进度与续跑指引（2026-11-17）](NIGHT_PROGRESS.md) | 目的：把这几天连续推进的状态、证据、待办和坑位写在一处，换 session / 换人也能接着做。 |
| [今晚开工单（功能优先，测试后置）](EVENING_START.md) | 用户决定：先把功能堆叠完成，测试/门禁后补。本文件是 18:00（或即刻开工）后的执行顺序。 |
| [xtervo 任务看板](TASK_BOARD.md) | 用户已授权：周末任务不再中途确认。 |

## 归档（历史/一次性报告）

| 文档 | 说明 |
|---|---|
| [Checkpoint: 图片架构 / GPU 显示链路（2026-11-11）](archive/CHECKPOINT_2026-11-11_IMAGE_ENGINE.md) | 状态：本地 checkpoint，可回退；不是 master，不是正式发布。 |
| [xtervo 开发日程 / 本地提醒](archive/DEV_SCHEDULE.md) | 这是本地提醒器，不是 agent 自动调度器。 |
| [图片字节过滤链守恒审计（2026-11-11）](archive/FILTER_AUDIT_2026-11-11.md) | 状态：审计测试 + 3 个 Sixel filter 修复均已完成。 |
| [最终交付报告（xtervo）](archive/FINAL_REPORT.md) | bash |
| [XTERVO_GPU_NATIVE vertical slice（2026-11-11）](archive/GPU_NATIVE_SLICE_2026-11-11.md) | 分支：agent/gpu-native（worktree .worktrees/agent-gpu-native） |
| [XTERVO_GPU=1 WGPU 显示链路 spike（2026-11-11）](archive/GPU_NOW_SPIKE_2026-11-11.md) | 状态：可选 opt-in，非默认。默认路径（CPU 栅格化 -> Image::from_rgba8 -> Skia/ |
| [GPU 显示链路最小验收：真实 PTY framebuffer → Slint WGPU texture → take_snapshot](archive/GPU_TERMINAL_SLINT_SPIKE.md) | - 分支：agent/gpu-spike |
| [最大化按钮：工业级状态机与验收门禁](archive/MAXIMIZE_ENGINEERING.md) | 状态：已实现（2026-11-11，agent/max-controller）。 独立 crate |
| [单进程多窗口 Spike（Slint 1.18.1 + WGPU30）](archive/MULTI_WINDOW_SPIKE.md) | 状态：spike 完成，gate 通过（有条件） |
| [2026-11-11 渲染残留事故复盘](archive/POSTMORTEM_2026-11-11_RENDER_RESIDUE.md) | 状态：已修复，当前 release b5c8c6f0fcba 干净；本文是排障纪律，不是事后解释。 |
| [2026-11-18 输入焦点事故复盘](archive/POSTMORTEM_2026-11-18_INPUT_FOCUS.md) | Profile 菜单 FocusScope 吞键/不归还焦点；已修复并补“菜单打开时普通键仍到终端”的 e2e。 |
| [xtervo 最终人工复测清单](archive/RETEST_CHECKLIST.md) | text |
| [Wayland 原生支持 Spike（W-06）](archive/WAYLAND_SPIKE.md) | --- |
| [WGPU 文字 / Glyph Atlas 原型（2026-11-11）](archive/WGPU_TEXT_ATLAS_PROTOTYPE.md) | 分支：agent/wgpu-text |
| [Phase 0 报告：可行性验证](archive/phase0-report.md) | --- |
| [自我迭代方案：让终端模拟器每轮都变得更好](archive/self-iteration.md) |  |

## 其他

| 文档 | 说明 |
|---|---|
| [dsh-tui 图片预览 e2e（隔离 harness）](DSH_IMAGE_PREVIEW_E2E.md) | 目标：在不碰真实 :1 会话、不依赖手工操作的前提下，可复现地验证 |
