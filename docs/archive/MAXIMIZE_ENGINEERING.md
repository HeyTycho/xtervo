# 最大化按钮：工业级状态机与验收门禁

状态：**已实现（2026-11-11，agent/max-controller）。** 独立 crate
`xtervo-maximize` 提供纯状态机 + X11 PropertyNotify 运行时；app 只做最小接线，
release 隔离 e2e 覆盖单屏/伪双屏/双实例/快慢点击/外部 WM/无 WM/通知丢失。

## 1. 这次为什么错

不是 X11/Slint 的“偶发 bug”，是设计错误：

```text
1. 没有单一状态源
   AppState / Slint window-maximized / WindowItem.maximized / EWMH
   四个状态在不同时刻被当成真值。

2. 用几何猜最大化
   _NET_WORKAREA 是 per-desktop 虚拟桌面 (5120x1413)，
   单屏最大化窗口 (2560x1440) 永远不满足 >= workarea。
   这个启发式在双屏 100% 错。

3. 用 800ms 窗口做“校准”
   每次静置点击先把 desired 写回 false，再翻转 true；
   重复发 add，窗口已最大化 -> 看起来没反应。

4. 用 title 找窗口
   find_own_window(title) 依赖 WM_NAME 不变；多实例/改标题时脆弱。

5. 测试只覆盖单屏 Xvfb
   旧 bug 在单屏不会触发，导致错误修复被当成 PASS。
```

结论：

```text
禁止 geometry heuristic / 禁止按 title 找窗口 / 禁止用 UI 缓存当权威
```

## 2. 单一状态机

```rust
struct MaximizeController {
    desired: bool,       // 用户最后一次意图
    observed: bool,      // EWMH _NET_WM_STATE 的真实观察
    pending: Option<bool>, // 已发出、尚未被 PropertyNotify 确认的请求
    last_error: Option<String>,
}
```

事件：

```text
click:
  desired = !desired
  pending = Some(desired)
  send(desired)
  UI icon = desired          // 只做显示

PropertyNotify(_NET_WM_STATE):
  observed = horz && vert
  if pending == Some(observed) { pending = None; desired = observed }
  else if pending.is_none()    { desired = observed } // 外部 WM 改变

request timeout:
  只记录错误/日志；绝不根据猜测改写 desired
```

不变量：

```text
1. click 永远翻转 desired；不受 observed/geometry/timer 影响
2. target = desired
3. 只有 PropertyNotify 能清除 pending
4. 外部最大化通过 PropertyNotify 同步，不轮询
5. 无 800ms、无 180ms、无 workarea、无 title 查找
```

## 3. 窗口身份

禁止：

```text
find_own_window(title)
```

必须：

```text
从 winit raw window handle 获取 X11 window id
所有 EWMH send/query 直接使用该 id
```

这样多实例、改标题、隐藏窗口都不会发错目标。

## 4. 必须的测试矩阵

全部跑 **release binary**，不是 debug：

```text
[x] 单屏 maximize -> restore -> maximize（两轮）
[x] 伪双屏 _NET_WORKAREA（Xvfb 1280x800 上伪造 5120x1413）
[x] 两个 xtervo 实例同时运行，各自独立 maximize/restore
[x] 快速点击 6 次（实测 <1s 完成，逐次翻转）
[x] 慢速点击 6 次（>1s 间隔）
[x] 外部 Super+Up / WM 快捷键最大化后点按钮
[x] 窗口已最大化时启动（Openbox rc.xml 规则）
[x] 无 WM / EWMH 不支持 -> 明确日志，不崩溃
[x] PropertyNotify 丢失/延迟 -> timeout 只报错，不改 target
    （e2e 用 SIGSTOP 冻结 Openbox 模拟通知丢失）
```

隔离环境：

```text
Xvfb + Openbox
独立 HOME/config
禁止 :1 注入
伪 _NET_WORKAREA 覆盖双屏
```

## 5. 发布门禁

```text
1. cargo test -p xtervo-app --bin xtervo
2. cargo test --workspace
3. BIN=target/release/xtervo scripts/e2e_maximize_isolated_state_machine.sh
4. 双实例/双屏/外部最大化矩阵全绿（脚本拒绝 debug binary）
5. 失败时发布延期，不把 debug PASS 当 release PASS
```

## 6. 实现与验收

```text
crates/maximize（crate: xtervo-maximize）
  controller.rs  纯状态机：desired / observed / pending / last_error + 超时注入
  x11.rs         worker：持久 X11 连接、显式 xid、PropertyChangeMask、
                 PropertyNotify -> 读取 _NET_WM_STATE -> 状态机事件

app 接线（crates/app/src/main.rs）
  - ui.show() 后按 50ms 重试 winit raw handle，拿到 Xlib/Xcb xid 才启动运行时
  - click -> controller.click()，图标立即显示 desired；发送在 worker
  - worker 回调经 slint::invoke_from_event_loop 同步图标/圆角
  - xconn 中按 title 查找的 maximize 发送/查询入口已删除
```

事件语义：

```text
click             desired 翻转，pending = desired，返回 target=desired
PropertyNotify    读取真实 _NET_WM_STATE：
                    pending == observed -> Confirmed（清 pending，desired=observed）
                    pending == None     -> ExternalSync（外部 WM 改变）
                    pending 已超时      -> TimeoutRecovery（迟到通知结束过期请求）
                    其余                -> StaleWhilePending（中间态/旧通知，保持）
send failed       last_error 可见；清 pending（请求未离开进程）；desired 不变
timeout           只写 last_error；pending 保留；desired/target 不变
```

门禁结果（2026-11-11，release artifact）：

```text
cargo test -p xtervo-maximize           17 passed（含 click/pending/observed 顺序、
                                       丢失通知、外部改变、发送失败、超时）
cargo test -p xtervo-maximize --release 17 passed
cargo test -p xtervo-app --bin xtervo    413 passed, 1 ignored
cargo test --workspace                 PASS
2m soak                               PASS（22 tabs/3 splits；见 .logs/soak-maximize-2m）
release artifact                      sha256=1f672f894bf861c4709c394bd400b7ca6c09428e3f1e39221aa5409de89bb08a
BIN=target/release/xtervo scripts/e2e_maximize_isolated_state_machine.sh
  PASS single（单屏 max/restore x2 + 伪 _NET_WORKAREA 5120x1413）
  PASS fast（6 连点 <1s，逐次翻转，无吞点击）
  PASS slow（6 连点间隔 >1s）
  PASS two（双实例各自独立）
  PASS external（外部 _NET_WM_STATE add/remove 后点按钮）
  PASS timeout（SIGSTOP 冻结 Openbox：超时只报错，target 不改）
  PASS startup-max（Openbox 规则启动即最大化，点按钮恢复）
  PASS no-wm（明确 send failed，click 仍翻 desired，不崩溃）
```

已知限制：

```text
- 仅 X11/EWMH；Wayland 或拿不到 xid 时明确回退 winit set_maximized。
- 2s 超时只用于错误上报；慢于 2s 的 WM 会先记 timeout，迟到通知仍会恢复。
- 外部改变与 pending 冲突时若未超时，按“中间态”保留用户 target；
  超时后的下一条真实通知会结束过期请求并同步 actual。
- 无 WM 时没有 _NET_WM_STATE 可观察，UI 显示 desired 但实际窗口不会改变。
```
