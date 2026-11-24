# 范围审视：什么算核心，什么可以不做

> 2026-11-17 修订（第二版）· 数据脚本 `python3 scripts/scope_tiers.py`
>
> 口径修正：**SSH / Serial / Telnet 是官方插件（可卸载），不是核心功能**；
> 我们也不追求和 Tabby 一一对应。矩阵只是“上游能力清单”，不是需求清单。

## 1. 结论先行

| 尺子 | 条目 | 加权 | 说明 |
|---|---|---|---|
| **终端本体** | 100 | **66.8%** | 唯一该叫“核心完成度”的数字 |
| 终端 + 工作区体验 | 173 | 57.2% | 加上侧栏/设置/通知/诊断 |
| 连接插件（可选） | 39 | 32.7% | SSH/Serial/Telnet/SFTP，**不装也能用** |
| 高级连接 | 14 | 12.5% | ProxyCommand/X11/Login Scripts/Hex 等，默认不做 |
| 不建议/观察 | 18 | 25.0% | Node sidecar、Angular UI、Zmodem、Quake、平台项 |
| 全量 | 244 | 48.4% | 把可选项也算进分母，参考价值最低 |

## 2. 五个大类

### P0 终端本体（100 条，66.8%）← 核心

`TERM / UI-TERM / KEY / UI-PANE / UI-TAB / UI-WIN / LOCAL / SESS`

PTY、渲染、键位、选择复制粘贴、查找、Tab、分屏、窗口、会话恢复、本地 Profile。
**不做就不是终端**。45 条 partial 是边角（查找高亮细节、主题个别字段等）。

### P1 工作区与设置（73 条，44.2%）

`UI-NAV / UI-PROF / UI-SET / UI-PLUG / NOTIFY / SEC / OPS / THEME`

侧栏/工具栏/状态区、Profile 编辑、设置页、插件 UI、通知、安全、连接诊断、主题。
它决定“好不好用”，但不决定“能不能用”，可以分批。

### P2 连接插件（39 条，32.7%）← 可选

SSH / UI-SSH / SERIAL / UI-SER / TELNET / SFTP。

**官方插件、可卸载**：不装连接插件时，xtervo 就是一个纯本地终端，功能完整。
和 Tabby 的差别就在这：Tabby 把 SSH 做进主程序，我们做成插件，
所以**这 39 条不该进“核心完成度”**。

### P3 高级连接（14 条，12.5%）← 默认不做

SSH ProxyCommand / X11 Forwarding / 持久转发 / Login Scripts / 算法配置 / WinSCP、
串口 Hex/换行/时间戳、Telnet Zmodem 协商。

面向重度 SSH/嵌入式用户，且 7 条连上游字段都没核实。等真有用户要再做。

### P4 不建议/观察（18 条，25.0%）

| 条目 | 建议 |
|---|---|
| PLUG-007..013（Node sidecar + tabby shim + Angular WebView） | **不做**：在 Rust 终端里养 Electron 兼容层，与 v3 原生插件路线冲突 |
| ZMODEM×4 + UI-SER-008 | **不做**：需内建协议状态机 + 文件 UI，留给第三方插件 |
| THEME-007（CSS 主题映射） | **不做**：CSS 无法等价映射到 Slint |
| UI-QUAKE×5 | **降级**：3 条已实现，剩余受 Wayland 全局热键限制 |
| PLAT-* | 按目标平台决定，不阻塞终端本体 |

## 3. 我们不打算和 Tabby 一样的地方

```text
连接       Tabby 内置 SSH → 我们做成官方插件（SSH/Serial/Telnet 可卸载）
旧插件     Tabby 的 Node/Angular 插件生态 → 我们只做 v3 原生（JSON-RPC 子进程）
主题       Tabby CSS 主题 → 我们只做内建 8 主题 + TOML 自定义
Quake      不做 Wayland 全局热键适配，保留现有 X11 实现
Zmodem     不做，交给社区插件
```

反过来，矩阵里没有、但我们自己加的东西（属于自研路线，不该被 48.4% 掩盖）：

```text
Workspace 命名布局（保存/打开/面板选择器）
侧栏工作区壳 + 状态区 pill
Safe mode（连续崩溃 → 本次不加载插件）
PaneTransport seam（插件提供 pane 后端的接口）
IME 候选框跟随真实光标
命令默认键兜底分发 / Profile 热键
```

## 4. 51 条 not_started 的真实构成

```text
 9 条  P0 终端本体（边角）
15 条  P1 工作区与设置（Connection Manager、设置页、插件注入…）
15 条  P2 连接插件（SSH/Serial/Telnet 主干，等 transport seam）
 6 条  P3 高级连接
 6 条  P4（Node sidecar、平台项等）
```

## 5. 0.6 的目标（建议）

```text
必修  终端本体 66.8% → 80%：把 partial 里影响手感的边角补掉
必修  工作区 44.2% → 60%：Connection Manager、设置页收尾、插件注入点
插件  transport seam + SSH POC（不追求一次做完 Serial/Telnet）
不动  P3/P4 全部；24h soak 与 Wayland 留给 0.7
```
