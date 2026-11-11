# 最大化按钮：工业级状态机与验收门禁

状态：**设计契约，53e1033 不是最终验收；禁止继续用启发式补丁迭代。**

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
[ ] 单屏 maximize -> restore -> maximize
[ ] 双屏虚拟桌面（_NET_WORKAREA 5120x1413，窗口 2560x1440）
[ ] 两个 Tervo 实例同时运行，各自独立 maximize/restore
[ ] 快速点击 6 次（<100ms 间隔）
[ ] 慢速点击 6 次（>1s 间隔）
[ ] 外部 Super+Up / WM 快捷键最大化后点按钮
[ ] 窗口已最大化时启动
[ ] 无 WM / EWMH 不支持 -> 明确日志，不崩溃
[ ] PropertyNotify 丢失/延迟 -> timeout 只报错，不改 target
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
1. cargo test -p tervo-app --bin tervo
2. cargo test --workspace
3. BIN=target/release/tervo scripts/e2e_maximize_isolated.sh
4. 双实例/双屏/外部最大化矩阵全绿
5. 失败时发布延期，不把 debug PASS 当 release PASS
```

## 6. 当前状态

```text
53e1033：删除几何校准，但仍是“desired 直接翻”的过渡实现；
        release 恢复路径在伪双屏测试中仍失败，不验收。
下一步：按本文件实现 MaximizeController + PropertyNotify + window id。
```
