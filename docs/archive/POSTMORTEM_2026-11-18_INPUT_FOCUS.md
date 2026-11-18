# 2026-11-18 输入焦点事故复盘（Profile 菜单吞键）

状态：已修复；最终 release 已重新构建并在本地验证，`~/.local/bin/tervo` 已替换为修复版。  
影响：用户打开 `>_` Profile 菜单后，普通按键被菜单的 `FocusScope` 吞掉；菜单关闭后焦点没有回到终端
`TextInput`，表现为“完全不能输入”。同时把此前已存在的“codex 崩溃后 CSI-u 回显乱码”一起处理，
但两者是**独立问题**，不能混为一谈。

## 1. 一句话结论

问题不在 `+` / `>_` 的点击逻辑，而在我为了“去掉 Profile 菜单搜索框”引入的
`FocusScope`：它抢走了终端 `TextInput` 的唯一键盘焦点，且 Slint 在它被销毁后不会自动把
焦点还给终端。菜单打开时可打印键被静默吞掉，关闭后则彻底没有接收者。

## 2. 时间线（本地时间）

```text
11:56–12:02  定位到 codex 退出后 kitty flags=7 残留，shell 回显 9;5:3u/7;129u；
             同时改 TabBar：+ 直接新 Tab、>_ 去搜索框
12:01        把 >_ 菜单里的 TextInput 换成 FocusScope（init => self.focus()）
12:12        构建 debug；只用 xclick/xwin + 截图验证点击、↑↓、Enter
12:22–12:26  构建 release 并跑 2m soak；用户启动后报告“都不能正常输入”
12:31        用 TERVO_KEYLOG 复现：菜单打开时按 x/def 没有任何 [key] 日志；
             确认 FocusScope 吞键、关闭后焦点不回终端
12:31–12:33  删除 FocusScope；菜单键改由 Rust handle_input 处理；
             构建最终 release
12:35        安装到 ~/.local/bin/tervo
12:36        给 e2e 增加“菜单打开按 x 仍应产生 [key]”步骤，debug/release 均 PASS
```

## 3. 根因

### 3.1 直接原因：FocusScope 抢焦点且不归还

菜单原来的搜索 `TextInput` 自带焦点，也天然消费所有可打印键；去掉搜索框后，我为了保留
↑↓/Enter/Esc，加了：

```slint
profilemenu-focus := FocusScope {
    init => { self.focus(); }
    key-pressed(event) => {
        if (event.text == "\u{F700}") { root.profilemenu-move(-1); return EventResult.accept; }
        if (event.text == "\u{F701}") { root.profilemenu-move(1); return EventResult.accept; }
        if (event.text == "\u{a}" || event.text == "\u{d}") { root.profilemenu-accept(); return EventResult.accept; }
        EventResult.reject
    }
}
```

Slint 的实际行为：

```text
1. FocusScope 拿到焦点后，终端 TextInput 失焦。
2. key-pressed 返回 reject 的可打印键不会转发给 TextInput——菜单打开时按键被吞。
3. 菜单关闭（Rust set_profilemenu_open(false)）销毁 FocusScope，但 Slint 不会把焦点
   自动还给之前的 TextInput——之后所有按键没有接收者。
```

这正好解释了两段症状：打开菜单后打字没反应；关掉菜单后仍然没反应。

### 3.2 同期修复的另一个问题：kitty flags 残留（不是本次输入失灵的根因）

codex 在崩溃/被 kill 时不会发 `CSI < u` 或关闭 `modifyOtherKeys`，shell 前台恢复后
Tervo 仍按 flags=7 编码，shell 回显后只剩 `9;5:3u`/`7;129u` 这类尾巴。这个问题在
11:19 的 `~/.local/bin/tervo` 里就存在，早于本次 UI 改动；修复是读线程用 `tcgetpgrp`
观察前台进程组，回到 shell 时复位键盘协议状态。它解决“乱码”，不解决“完全无按键”。

## 4. 为什么没拦住

```text
1. 没有先写失败测试：改动前只验证了“点击能开菜单、↑↓能移动、Enter能打开”，
   没有验证“菜单打开时按普通键”“菜单关闭后继续输入”。
2. 截图验证的盲区：截图能证明菜单长什么样，不能证明键盘焦点在谁身上。
   正确的断言是 [key]/[send] 日志或真实 PTY 字节，而不是像素。
3. 现有 app 单测不启动 Slint 窗口，覆盖不到 FocusScope/TextInput 焦点；
   现有 e2e 也只点鼠标，不测关闭菜单后的键盘。
4. 同一轮里同时改 TabBar 交互、Profile 菜单、core 键盘状态；问题被“乱码”
   表象掩盖，第一次归因没有区分“乱码”和“无输入”。
5. 构建后直接替换用户启动的 ~/.local/bin/tervo，没有先让用户在一个
   隔离实例里做输入冒烟。
```

对应到 `AGENTS.md` 的排障纪律：违反了“先复现、保留证据”和“没有失败测试不写修复”；
截图不是输入链路的有效证据。

## 5. 修复

```text
1. 菜单不再创建 FocusScope，也不抢焦点；终端 TextInput 始终是唯一键盘入口。
2. `>_` 菜单的 ↑↓/Enter/Esc 在 Rust handle_input 里处理：
   - ↑↓ 移动高亮；
   - Enter 打开；
   - Esc 关闭；
   - 其它键先关菜单、归还焦点，再按原路径分发（Ctrl+Tab 等快捷键不受影响）。
3. 关闭菜单的所有路径都显式 focus-terminal（Slint 外部点击/Rust 取消/普通键）。
4. e2e 新增步骤 2b：菜单打开时按 x，必须出现 [key] "x"，防止再次出现“吞键”。
```

## 6. 门禁与后续规则

```text
1. 任何新增/删除可聚焦 UI（FocusScope/TextInput/弹层）必须同时提交
   “输入链路” e2e：打开→按普通键→关闭→再输入，断言 [key]/[send] 日志。
2. scripts/e2e_tabbar_plus_profile.sh 纳入发布前验收；它现在覆盖：
   + 直接新 Tab、>_ 菜单无搜索、↑↓+Enter、菜单打开时普通键不丢。
3. 不再未经用户明确同意覆盖 ~/.local/bin/tervo；先构建到 target/release，
   在隔离 Xvfb/实例里验证，再由用户或用户授权的命令安装。
4. 输入类问题一律用 TERVO_KEYLOG=1 复现并保留 [key]/[send]/[keyboard] 日志；
   禁止只用截图判定“键盘正常”。
5. UI 交互改动与 core 键盘协议改动分 commit、分验证，避免归因混淆。
```

## 7. 验证证据

```text
app tests                666 passed / 2 ignored
core tests               全绿（含 foreground_command_exit_resets_stale_kitty_flags）
e2e_tabbar_plus_profile  debug/release 均 PASS（含步骤 2b 吞键回归）
e2e_workspace_ui         PASS
2m soak                  PASS：RSS 385.7→454.9 MiB，threads 112 / fds 123 不变，fatal=0
安装产物                 ~/.local/bin/tervo 12:35 构建，重启后生效
```

## 8. 相关提交

```text
ad52a3c fix(input+ui): TabBar 旧交互回归 + 崩溃应用键盘状态复位
scripts/e2e_tabbar_plus_profile.sh
crates/core/tests/keyboard_protocol.rs::foreground_command_exit_resets_stale_kitty_flags
```
