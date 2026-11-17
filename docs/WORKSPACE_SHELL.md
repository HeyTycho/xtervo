# 工作区壳：Sidebar / Toolbar / 状态区（ACT-046 + ACT-041 前置）

状态：**进行中（2026-11-17）**。已完成 ① config 字段 ② 侧栏骨架 ③ 右边缘拖宽 + 宽度持久化 + Profiles 列表（复用下沉后的 `rebuild_profile_choices`，启动即填充，点击开 Tab）（默认关，截图见
`.logs/sidebar_on.png` / `.logs/sidebar_off.png`）；待做 ③ 拖宽 + Profiles 列表 ④ Toolbar
⑤ 状态区 ⑥ Connection Manager ⑦ 插件注入点。目标是把“入口”从 TabBar 菜单
升级成常驻壳，并让 Connection Manager / 插件条目有地方挂。

## 1. 验收（摘自 ACTION_ITEMS §8）

```text
Sidebar 显隐 + 宽度可调；Toolbar 按钮分组/禁用态；状态区显示连接/进度/完成；
所有图标有 tooltip/无障碍标签；Connection Manager 入口常驻且可键盘到达。
```

## 2. 现状

- UI 是单个 `crates/app/ui/main.slint`（约 8.5k 行），根布局用**绝对定位**：
  TabBar 可切上/下（见 `main.slint:1992` 注释），内容区在 TabBar 另一侧。
- 入口目前只有 TabBar 右侧的 `+` 下拉（`profilemenu-*`）与右键菜单。
- 已有数据源：`Profile` 模型（`profile.rs`，四类 + 校验）、`workspace` tabs/panes、
  插件 v3 的声明式 panel/settings 与 `ui.status`、`plugin-status` 单行状态栏。

## 3. 布局决策

```text
Window
├─ Toolbar（可选，高 36px，与 TabBar 同侧）
├─ 主体 HorizontalLayout
│   ├─ Sidebar（默认隐藏，宽 200–360px，可拖分隔条）
│   └─ 终端区（现有 tabbar + panes + 分隔条，保持不变）
└─ 状态区（底栏一行，复用现有 plugin-status 高度）
```

- **不改**现有 panes/dividers 的坐标计算：Sidebar 只让终端区的 `x` 平移，内容区尺寸
  由 Rust 侧继续用 `root.content` 推导（避免两套几何来源）。
- TabBar 上/下切换不受影响；Sidebar 始终在终端区左侧（后续可加右侧选项）。
- 状态区先复用 `plugin-status` 那一行：`status_items[]` 里的条目按优先级显示，
  溢出折叠成 `+N`；hover 出 tooltip。

## 4. 数据模型（Rust → Slint）

```rust
struct ShellModel {
    sidebar_visible: bool,
    sidebar_width: f32,          // 持久化到 config: sidebar_width
    sidebar_section: u8,         // 0 = Profiles, 1 = Sessions, 2 = Plugins
    toolbar_items: Vec<ShellButton>,  // { id, icon, label, tooltip, enabled, disabled_reason }
    status_items: Vec<StatusItem>,    // { id, text, tooltip, kind: Info|Progress|Error }
}
```

- `toolbar_items` 与 `status_items` 都由**命令 ID**驱动（`commands.rs`），点击走既有
  分发；未实现的功能必须 `enabled=false` + `disabled_reason`（AGENTS.md 禁止假按钮）。
- 插件条目：v3 `ui.status` 已有最小状态行，这里扩成 `status_items` 的一个来源；
  插件声明的 panel 走 Sidebar 的 `Plugins` 分区（复用声明式渲染）。

## 5. 施工顺序（每步都要截图进 `.logs/ui-audit/`）

```text
1. ShellModel + config 字段（sidebar_visible/width/section），默认全关，行为与现在完全一致
2. Sidebar 骨架（可拖宽、可关），内容先放 Profiles 列表（复用已有 profile 行）
3. Toolbar（New Tab / Profiles / Split / Settings 四个入口，全部走命令 ID）
4. 状态区（把现有 plugin-status 迁进来，加 hover tooltip）
5. Connection Manager 面板（ACT-041 的 UI 部分）：连接中/失败/取消三态
6. 插件注入点：ui.status → status_items；声明式 panel → Sidebar.Plugins
```

## 6. 风险与回退

- Slint 绝对定位改动容易把 TabBar 上/下、最大化状态机、IME 候选点带崩：每一步都跑
  `scripts/e2e_maximize_isolated.sh` + IME 手工清单，并留存前后截图。
- 默认全关时**必须**与 v0.5.0 像素一致（拿 `scripts/e2e_split_repaint.py` 的截图口径
  做一次 before/after 对比），否则视为回归。
- 回退方式：`config.sidebar_*` 三个字段删掉即回到旧布局，UI 侧不留死代码分支。
