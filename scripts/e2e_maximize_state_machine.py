#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only
"""隔离 Xvfb/Openbox 下的最大化状态机 e2e 驱动。

只通过 X11 协议操作窗口：按 `_NET_WM_PID` 找本测试实例（应用代码绝不按
标题查找），用 XTest 点 UI 按钮，用 EWMH ClientMessage 模拟外部 WM 改变。
场景由 `scripts/e2e_maximize_isolated_state_machine.sh` 调用。
"""

from __future__ import annotations

import argparse
import ctypes
import ctypes.util
import os
import re
import signal
import sys
import time

# ---------------------------------------------------------------------------
# Xlib / XTest 绑定
# ---------------------------------------------------------------------------

X11 = ctypes.CDLL("libX11.so.6")
XTST = ctypes.CDLL("libXtst.so.6")

Display = ctypes.c_void_p
Window = ctypes.c_ulong
Atom = ctypes.c_ulong

X11.XOpenDisplay.argtypes = [ctypes.c_char_p]
X11.XOpenDisplay.restype = Display
X11.XDefaultRootWindow.argtypes = [Display]
X11.XDefaultRootWindow.restype = Window
X11.XInternAtom.argtypes = [Display, ctypes.c_char_p, ctypes.c_int]
X11.XInternAtom.restype = Atom
X11.XFree.argtypes = [ctypes.c_void_p]
X11.XFree.restype = ctypes.c_int
X11.XFlush.argtypes = [Display]
X11.XFlush.restype = ctypes.c_int
X11.XSync.argtypes = [Display, ctypes.c_int]
X11.XSync.restype = ctypes.c_int
X11.XRaiseWindow.argtypes = [Display, Window]
X11.XRaiseWindow.restype = ctypes.c_int
X11.XSetInputFocus.argtypes = [Display, Window, ctypes.c_int, ctypes.c_ulong]
X11.XSetInputFocus.restype = ctypes.c_int
X11.XQueryTree.argtypes = [
    Display,
    Window,
    ctypes.POINTER(Window),
    ctypes.POINTER(Window),
    ctypes.POINTER(ctypes.POINTER(Window)),
    ctypes.POINTER(ctypes.c_uint),
]
X11.XQueryTree.restype = ctypes.c_int
X11.XGetWindowProperty.argtypes = [
    Display,
    Window,
    Atom,
    ctypes.c_long,
    ctypes.c_long,
    ctypes.c_int,
    Atom,
    ctypes.POINTER(Atom),
    ctypes.POINTER(ctypes.c_int),
    ctypes.POINTER(ctypes.c_ulong),
    ctypes.POINTER(ctypes.c_ulong),
    ctypes.POINTER(ctypes.POINTER(ctypes.c_ubyte)),
]
X11.XGetWindowProperty.restype = ctypes.c_int
X11.XChangeProperty.argtypes = [
    Display,
    Window,
    Atom,
    Atom,
    ctypes.c_int,
    ctypes.c_int,
    ctypes.POINTER(ctypes.c_ubyte),
    ctypes.c_int,
]
X11.XChangeProperty.restype = ctypes.c_int
X11.XSendEvent.argtypes = [Display, Window, ctypes.c_int, ctypes.c_long, ctypes.c_void_p]
X11.XSendEvent.restype = ctypes.c_int
X11.XTranslateCoordinates.argtypes = [
    Display,
    Window,
    Window,
    ctypes.c_int,
    ctypes.c_int,
    ctypes.POINTER(ctypes.c_int),
    ctypes.POINTER(ctypes.c_int),
    ctypes.POINTER(Window),
]
X11.XTranslateCoordinates.restype = ctypes.c_int
X11.XFetchName.argtypes = [Display, Window, ctypes.POINTER(ctypes.c_char_p)]
X11.XFetchName.restype = ctypes.c_int

XTST.XTestFakeMotionEvent.argtypes = [Display, ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_ulong]
XTST.XTestFakeMotionEvent.restype = ctypes.c_int
XTST.XTestFakeButtonEvent.argtypes = [Display, ctypes.c_uint, ctypes.c_int, ctypes.c_ulong]
XTST.XTestFakeButtonEvent.restype = ctypes.c_int

# 忽略 BadWindow 之类异步错误，避免窗口切换瞬间 stderr 噪声。
ErrorHandler = ctypes.CFUNCTYPE(ctypes.c_int, Display, ctypes.c_void_p)


@ErrorHandler
def _ignore_x_error(_display, _event):
    return 0


X11.XSetErrorHandler.argtypes = [ErrorHandler]
X11.XSetErrorHandler.restype = ctypes.c_void_p
X11.XSetErrorHandler(_ignore_x_error)


class XWindowAttributes(ctypes.Structure):
    _fields_ = [
        ("x", ctypes.c_int),
        ("y", ctypes.c_int),
        ("width", ctypes.c_int),
        ("height", ctypes.c_int),
        ("border_width", ctypes.c_int),
        ("depth", ctypes.c_int),
        ("visual", ctypes.c_void_p),
        ("root", Window),
        ("klass", ctypes.c_int),
        ("bit_gravity", ctypes.c_int),
        ("win_gravity", ctypes.c_int),
        ("backing_store", ctypes.c_int),
        ("backing_planes", ctypes.c_ulong),
        ("backing_pixel", ctypes.c_ulong),
        ("save_under", ctypes.c_int),
        ("colormap", ctypes.c_ulong),
        ("map_installed", ctypes.c_int),
        ("map_state", ctypes.c_int),
        ("all_event_masks", ctypes.c_long),
        ("your_event_mask", ctypes.c_long),
        ("do_not_propagate_mask", ctypes.c_long),
        ("override_redirect", ctypes.c_int),
        ("screen", ctypes.c_void_p),
    ]


X11.XGetWindowAttributes.argtypes = [Display, Window, ctypes.POINTER(XWindowAttributes)]
X11.XGetWindowAttributes.restype = ctypes.c_int


class ClientData(ctypes.Union):
    _fields_ = [
        ("b", ctypes.c_char * 20),
        ("s", ctypes.c_short * 10),
        ("l", ctypes.c_long * 5),
    ]


class XClientMessageEvent(ctypes.Structure):
    _fields_ = [
        ("type", ctypes.c_int),
        ("serial", ctypes.c_ulong),
        ("send_event", ctypes.c_int),
        ("display", Display),
        ("window", Window),
        ("message_type", Atom),
        ("format", ctypes.c_int),
        ("data", ClientData),
    ]


class XEvent(ctypes.Union):
    _fields_ = [
        ("type", ctypes.c_int),
        ("xclient", XClientMessageEvent),
        ("pad", ctypes.c_long * 24),
    ]


# 常量
CLIENT_MESSAGE = 33
SUBSTRUCTURE_NOTIFY = 1 << 19
SUBSTRUCTURE_REDIRECT = 1 << 20
PROP_MODE_REPLACE = 0
XA_CARDINAL = 6
XA_ATOM = 4
MAP_STATE_VIEWABLE = 2

DISPLAY_NAME = os.environ.get("DISPLAY", "")
DISPLAY = X11.XOpenDisplay(DISPLAY_NAME.encode() if DISPLAY_NAME else None)
if not DISPLAY:
    raise SystemExit(f"无法连接 DISPLAY={DISPLAY_NAME!r}")
ROOT = X11.XDefaultRootWindow(DISPLAY)


def atom(name: str) -> int:
    return int(X11.XInternAtom(DISPLAY, name.encode(), 0))


def get_property(window: int, name: str, max_len: int = 256):
    """返回 (format, values)；属性不存在返回 None。"""
    prop_atom = atom(name)
    actual_type = Atom()
    actual_format = ctypes.c_int()
    nitems = ctypes.c_ulong()
    bytes_after = ctypes.c_ulong()
    prop = ctypes.POINTER(ctypes.c_ubyte)()
    status = X11.XGetWindowProperty(
        DISPLAY,
        window,
        prop_atom,
        0,
        max_len,
        False,
        0,
        ctypes.byref(actual_type),
        ctypes.byref(actual_format),
        ctypes.byref(nitems),
        ctypes.byref(bytes_after),
        ctypes.byref(prop),
    )
    if status != 0 or not prop:
        if prop:
            X11.XFree(ctypes.cast(prop, ctypes.c_void_p))
        return None
    try:
        n = int(nitems.value)
        address = ctypes.addressof(prop.contents)
        if actual_format.value == 32:
            values = list((ctypes.c_ulong * n).from_address(address))
        elif actual_format.value == 8:
            values = bytes((ctypes.c_ubyte * n).from_address(address))
        else:
            values = []
        return actual_format.value, values
    finally:
        X11.XFree(ctypes.cast(prop, ctypes.c_void_p))


def get_atoms(window: int, name: str):
    prop = get_property(window, name)
    if prop is None:
        return []
    fmt, values = prop
    return [int(v) for v in values] if fmt == 32 else []


def get_window_attributes(window: int):
    attrs = XWindowAttributes()
    if not X11.XGetWindowAttributes(DISPLAY, window, ctypes.byref(attrs)):
        return None
    return attrs


def window_name(window: int) -> str:
    prop = get_property(window, "_NET_WM_NAME")
    if prop and prop[0] == 8:
        return bytes(prop[1]).decode("utf-8", "replace")
    name = ctypes.c_char_p()
    if X11.XFetchName(DISPLAY, window, ctypes.byref(name)) and name.value:
        try:
            return name.value.decode("utf-8", "replace")
        finally:
            X11.XFree(name)
    return ""


def query_children(window: int):
    root_ret = Window()
    parent_ret = Window()
    children = ctypes.POINTER(Window)()
    count = ctypes.c_uint()
    if not X11.XQueryTree(
        DISPLAY,
        window,
        ctypes.byref(root_ret),
        ctypes.byref(parent_ret),
        ctypes.byref(children),
        ctypes.byref(count),
    ):
        return [], int(parent_ret.value)
    try:
        return [int(children[i]) for i in range(count.value)], int(parent_ret.value)
    finally:
        if children:
            X11.XFree(children)


def walk_windows(window: int, depth: int = 0):
    if depth > 4:
        return
    children, _ = query_children(window)
    for child in children:
        yield child
        yield from walk_windows(child, depth + 1)


def window_pid(window: int):
    values = get_atoms(window, "_NET_WM_PID")
    return values[0] if values else None


def find_window_by_pid(pid: int, title: str | None = None):
    candidates = []
    # EWMH client list 是最可靠的 managed client 列表；无 WM 时退化为整棵树。
    for wid in get_atoms(ROOT, "_NET_CLIENT_LIST"):
        candidates.append(wid)
    candidates.extend(walk_windows(ROOT))
    seen = set()
    best = None
    best_area = 0
    for wid in candidates:
        if wid in seen or wid == ROOT:
            continue
        seen.add(wid)
        attrs = get_window_attributes(wid)
        if attrs is None or attrs.map_state != MAP_STATE_VIEWABLE:
            continue
        if attrs.width < 80 or attrs.height < 80:
            continue
        candidate_pid = window_pid(wid)
        if candidate_pid == pid:
            area = attrs.width * attrs.height
            if area > best_area:
                best, best_area = wid, area
        elif candidate_pid is None and title and title in window_name(wid):
            area = attrs.width * attrs.height
            if area > best_area:
                best, best_area = wid, area
    return best


def wait_window(pid: int, title: str | None, timeout: float = 15.0):
    deadline = time.time() + timeout
    while time.time() < deadline:
        wid = find_window_by_pid(pid, title)
        if wid:
            return wid
        time.sleep(0.05)
    raise E2EFailure(f"15s 内找不到 pid={pid} title={title!r} 的窗口")


def geometry(window: int):
    attrs = get_window_attributes(window)
    if attrs is None:
        raise E2EFailure(f"窗口 0x{window:x} 不可访问")
    root_x = ctypes.c_int()
    root_y = ctypes.c_int()
    child = Window()
    X11.XTranslateCoordinates(
        DISPLAY, window, ROOT, 0, 0, ctypes.byref(root_x), ctypes.byref(root_y), ctypes.byref(child)
    )
    return {
        "x": int(root_x.value),
        "y": int(root_y.value),
        "w": int(attrs.width),
        "h": int(attrs.height),
    }


def screen_size():
    attrs = get_window_attributes(ROOT)
    assert attrs is not None
    return int(attrs.width), int(attrs.height)


def query_state(window: int) -> str:
    states = set(get_atoms(window, "_NET_WM_STATE"))
    horz = atom("_NET_WM_STATE_MAXIMIZED_HORZ")
    vert = atom("_NET_WM_STATE_MAXIMIZED_VERT")
    has_h = horz in states
    has_v = vert in states
    if has_h and has_v:
        return "maximized"
    if not has_h and not has_v:
        return "normal"
    return f"partial(h={has_h},v={has_v})"


def wait_state(window: int, expected: str, timeout: float = 3.0):
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        last = query_state(window)
        if last == expected:
            return
        time.sleep(0.02)
    raise E2EFailure(f"等待 _NET_WM_STATE={expected} 超时，当前 {last}")


def sync():
    X11.XSync(DISPLAY, False)


def raise_window(window: int):
    # Openbox 会把 client reparent 到 frame；只 raise client 不会把 frame 提到最上层。
    _, parent = query_children(window)
    if parent and parent != ROOT:
        X11.XRaiseWindow(DISPLAY, parent)
    X11.XRaiseWindow(DISPLAY, window)
    X11.XFlush(DISPLAY)


def send_client_message(window: int, message_type: str, data, mask: int):
    event = XEvent()
    event.xclient.type = CLIENT_MESSAGE
    event.xclient.serial = 0
    event.xclient.send_event = 1
    event.xclient.display = DISPLAY
    event.xclient.window = window
    event.xclient.message_type = atom(message_type)
    event.xclient.format = 32
    for i, value in enumerate(data[:5]):
        event.xclient.data.l[i] = value
    status = X11.XSendEvent(DISPLAY, ROOT, False, mask, ctypes.byref(event))
    X11.XFlush(DISPLAY)
    if not status:
        raise E2EFailure(f"XSendEvent {message_type} 失败")


def activate(window: int):
    # 先请求 WM 激活/置顶，再直接 raise frame + client；两者都做以兼容无 WM。
    send_client_message(
        window,
        "_NET_ACTIVE_WINDOW",
        [2, 0, 0, 0, 0],  # source=2 (pager/test), timestamp=CurrentTime
        SUBSTRUCTURE_NOTIFY | SUBSTRUCTURE_REDIRECT,
    )
    raise_window(window)
    X11.XSetInputFocus(DISPLAY, window, 1, 0)  # RevertToPointerRoot
    X11.XFlush(DISPLAY)
    time.sleep(0.18)


def click_at(x: int, y: int, settle: float = 0.15):
    XTST.XTestFakeMotionEvent(DISPLAY, 0, int(x), int(y), 0)
    X11.XFlush(DISPLAY)
    time.sleep(0.06)
    XTST.XTestFakeButtonEvent(DISPLAY, 1, True, 0)
    X11.XFlush(DISPLAY)
    time.sleep(0.03)
    XTST.XTestFakeButtonEvent(DISPLAY, 1, False, 0)
    X11.XFlush(DISPLAY)
    if settle:
        time.sleep(settle)


def click_window(window: int, settle: float = 0.25, activate_first: bool = True):
    if activate_first:
        activate(window)
    geo = geometry(window)
    # 与既有 e2e 相同的命中区：标题栏右侧按钮中心。
    click_at(geo["x"] + geo["w"] - 50, geo["y"] + 17, settle=settle)


def external_set_maximized(window: int, maximized: bool):
    send_client_message(
        window,
        "_NET_WM_STATE",
        [
            1 if maximized else 0,
            atom("_NET_WM_STATE_MAXIMIZED_HORZ"),
            atom("_NET_WM_STATE_MAXIMIZED_VERT"),
            1,
            0,
        ],
        SUBSTRUCTURE_NOTIFY | SUBSTRUCTURE_REDIRECT,
    )


def set_workarea(width: int, height: int):
    # Openbox 的极简构建不创建 _NET_WORKAREA，xprop -set 不会凭空创建 atom；
    # 直接 XChangeProperty 创建该 root property（测试只关心它存在且足够大）。
    values = (ctypes.c_ulong * 4)(0, 0, width, height)
    status = X11.XChangeProperty(
        DISPLAY,
        ROOT,
        atom("_NET_WORKAREA"),
        XA_CARDINAL,
        32,
        PROP_MODE_REPLACE,
        ctypes.cast(values, ctypes.POINTER(ctypes.c_ubyte)),
        4,
    )
    check(status == 1, f"写入 _NET_WORKAREA 失败 status={status}")
    X11.XFlush(DISPLAY)
    X11.XSync(DISPLAY, False)
    time.sleep(0.15)


def workarea():
    prop = get_property(ROOT, "_NET_WORKAREA")
    if not prop or prop[0] != 32:
        return None
    values = [int(v) for v in prop[1]]
    return values[:4] if len(values) >= 4 else None


def inspect_log(path: str) -> str:
    try:
        with open(path, "r", encoding="utf-8", errors="replace") as handle:
            return handle.read()
    except FileNotFoundError:
        return ""


def fail(message: str):
    raise E2EFailure(message)


def check(condition, message: str):
    if not condition:
        fail(message)


class E2EFailure(Exception):
    pass


def log(message: str):
    print(f"[e2e] {message}", flush=True)


# ---------------------------------------------------------------------------
# 场景
# ---------------------------------------------------------------------------


def scenario_single(pid: int, title: str | None):
    wid = wait_window(pid, title)
    wait_state(wid, "normal", 3.0)
    before = geometry(wid)
    log(f"single window=0x{wid:x} before={before}")

    for cycle in (1, 2):
        click_window(wid)
        wait_state(wid, "maximized")
        maximized = geometry(wid)
        check(
            maximized["w"] >= before["w"] and maximized["h"] >= before["h"],
            f"最大化几何没有变大: {before} -> {maximized}",
        )
        click_window(wid)
        wait_state(wid, "normal")
        restored = geometry(wid)
        check(
            restored["w"] <= maximized["w"] and restored["h"] <= maximized["h"],
            f"恢复几何异常: {maximized} -> {restored}",
        )
        log(f"cycle {cycle}: max={maximized} restore={restored}")

    # 伪双屏回归：_NET_WORKAREA 是整块虚拟桌面，单屏最大化窗口永远小于它。
    # 状态机不得读取它；这里把它设成比真实屏幕大很多，再点按钮。
    sw, sh = screen_size()
    set_workarea(5120, 1413)
    check(
        workarea() is not None and workarea()[2] == 5120 and workarea()[3] == 1413,
        f"伪 _NET_WORKAREA 设置失败: {workarea()}",
    )
    click_window(wid)
    wait_state(wid, "maximized")
    fake_max = geometry(wid)
    check(
        fake_max["w"] <= sw and fake_max["h"] <= sh,
        f"wm 按 5120x1413 workarea 给出了超出屏幕的几何: {fake_max} screen={sw}x{sh}",
    )
    click_window(wid)
    wait_state(wid, "normal")
    log(f"pseudo-dual screen={sw}x{sh} workarea={workarea()} max={fake_max}")
    set_workarea(sw, sh)


def scenario_fast(pid: int, title: str | None, count: int, interval: float, log_path: str | None):
    wid = wait_window(pid, title)
    wait_state(wid, "normal", 3.0)
    activate(wid)
    time.sleep(0.3)
    screen_w, _ = screen_size()
    started = time.time()
    timestamps = []
    for index in range(count):
        geo = geometry(wid)
        # 窗口可能刚刚 resize；按当前几何重新算按钮位置。
        cx = min(geo["x"] + geo["w"] - 50, screen_w - 20)
        cy = geo["y"] + 17
        timestamps.append(time.time())
        click_at(cx, cy, settle=0.0)
        time.sleep(interval)
    elapsed = time.time() - started
    log(f"fast clicks={count} elapsed={elapsed:.2f}s intervals={[round(b - a, 3) for a, b in zip(timestamps, timestamps[1:])]}")
    # 等 WM 处理完所有请求并让状态机确认最后一个 PropertyNotify。
    time.sleep(1.8)
    state = query_state(wid)
    check(state == "normal", f"{count} 次偶数点击后应为 normal，实际 {state}")
    if log_path:
        text = inspect_log(log_path)
        clicks = re.findall(r"\[maximize\] click target=(true|false)", text)
        expected_targets = ["true" if index % 2 == 0 else "false" for index in range(count)]
        check(
            clicks[-count:] == expected_targets,
            f"最近 {count} 次 controller click 不是逐次翻转（有点击被 WM/resize 吞掉）: "
            f"{clicks[-count:]}",
        )
        log(f"controller clicks logged={len(clicks)} last={clicks[-count:]}")
    # 再点一次确认状态机没有卡在 pending。
    click_window(wid)
    wait_state(wid, "maximized")
    click_window(wid)
    wait_state(wid, "normal")


def scenario_slow(pid: int, title: str | None, count: int, interval: float):
    wid = wait_window(pid, title)
    wait_state(wid, "normal", 3.0)
    for index in range(count):
        expected = "maximized" if index % 2 == 0 else "normal"
        click_window(wid)
        wait_state(wid, expected)
        geo = geometry(wid)
        log(f"slow click {index + 1}/{count}: state={expected} geo={geo}")
        time.sleep(interval)


def scenario_two(pid_a: int, pid_b: int, title_a: str | None, title_b: str | None):
    a = wait_window(pid_a, title_a)
    b = wait_window(pid_b, title_b)
    wait_state(a, "normal", 3.0)
    wait_state(b, "normal", 3.0)
    log(f"two instances a=0x{a:x} b=0x{b:x}")

    click_window(a)
    wait_state(a, "maximized")
    check(query_state(b) == "normal", "A 最大化后 B 也被改变了")
    log("A max -> B still normal")

    click_window(b)
    wait_state(b, "maximized")
    check(query_state(a) == "maximized", "B 最大化后 A 的状态被破坏")
    log("B max -> both max")

    click_window(a)
    wait_state(a, "normal")
    check(query_state(b) == "maximized", "A 恢复后 B 被恢复")
    log("A restore -> B still max")

    click_window(b)
    wait_state(b, "normal")
    check(query_state(a) == "normal", "B 恢复后 A 状态异常")
    log("B restore -> both normal")


def scenario_external(pid: int, title: str | None):
    wid = wait_window(pid, title)
    wait_state(wid, "normal", 3.0)
    time.sleep(0.5)

    # 外部 WM（Super+Up 等价）最大化 -> PropertyNotify 同步 desired=true；
    # 再点按钮应该恢复，而不是再次“最大化”。
    external_set_maximized(wid, True)
    wait_state(wid, "maximized")
    time.sleep(0.5)
    click_window(wid)
    wait_state(wid, "normal")
    log("external max -> click -> restore")

    # 外部先最大化再恢复 -> desired=false；再点按钮应该最大化。
    external_set_maximized(wid, True)
    wait_state(wid, "maximized")
    external_set_maximized(wid, False)
    wait_state(wid, "normal")
    time.sleep(0.5)
    click_window(wid)
    wait_state(wid, "maximized")
    log("external max/restore -> click -> maximize")
    click_window(wid)
    wait_state(wid, "normal")


def scenario_timeout(pid: int, title: str | None, openbox_pid: int, log_path: str):
    wid = wait_window(pid, title)
    wait_state(wid, "normal", 3.0)
    activate(wid)
    time.sleep(0.4)
    text_before = inspect_log(log_path)
    clicks_before = len(re.findall(r"\[maximize\] click target=(true|false)", text_before))

    # 冻结 WM：它已经声明 _NET_SUPPORTED，但不再处理 ClientMessage，
    # 因此请求“已发出”，却永远不会产生 PropertyNotify（模拟通知丢失）。
    os.kill(openbox_pid, signal.SIGSTOP)
    try:
        time.sleep(0.3)
        click_window(wid, activate_first=False)
        # 超过 2s 请求超时；期间没有 WM resize，窗口仍是 normal。
        time.sleep(2.6)
        check(query_state(wid) == "normal", "WM 冻结时窗口不应自行最大化")

        text = inspect_log(log_path)
        new_text = text[len(text_before):]
        clicks = re.findall(r"\[maximize\] click target=(true|false)", text)
        check(
            len(clicks) == clicks_before + 1 and clicks[-1] == "true",
            f"冻结 WM 后第一次 click target 应为 true: {clicks[-3:]}",
        )
        check(
            re.search(r"PropertyNotify timeout after \d+ms \(target=true", new_text) is not None,
            f"没有看到 timeout 日志（不会改 target）:\n{new_text[-1500:]}",
        )
        check(
            "desired=true observed=false pending=Some(true)" in new_text,
            "timeout 后状态机不应改写 desired/pending/observed",
        )
        log("frozen WM: click target=true, timeout logged, target unchanged")

        # 再点一次：仍然只翻 desired（true -> false），不能因为失败/超时回退。
        click_window(wid, activate_first=False)
        time.sleep(0.5)
        text = inspect_log(log_path)
        clicks = re.findall(r"\[maximize\] click target=(true|false)", text)
        check(
            len(clicks) == clicks_before + 2 and clicks[-2:] == ["true", "false"],
            f"timeout 后第二次 click 应以 desired 翻转: {clicks[-3:]}",
        )
        check(query_state(wid) == "normal", "两次请求都不应改变实际窗口状态")
        try:
            os.kill(pid, 0)
        except OSError as error:
            fail(f"timeout 场景应用已退出: {error}")
        log("timeout did not mutate target; second click flipped true -> false")
    finally:
        # 断言失败时也必须恢复 Openbox，否则 bash cleanup 的 TERM 会被暂停态挂住。
        os.kill(openbox_pid, signal.SIGCONT)
        time.sleep(1.5)

    # 恢复 WM：排队的 ClientMessage 可能迟到；随后外部恢复 + 点击必须能恢复工作。
    external_set_maximized(wid, False)
    wait_state(wid, "normal", 3.0)
    time.sleep(0.5)
    click_window(wid)
    wait_state(wid, "maximized")
    click_window(wid)
    wait_state(wid, "normal")
    log("WM resumed: late notify tolerated, click still works")


def scenario_startup_max(pid: int, title: str | None):
    wid = wait_window(pid, title)
    wait_state(wid, "maximized", 5.0)
    # 等启动初始化 + PropertyNotify 同步完成。
    time.sleep(1.0)
    check(query_state(wid) == "maximized", "启动时 Openbox 没有保持最大化")
    click_window(wid)
    wait_state(wid, "normal")
    log("startup maximized -> click -> normal")
    click_window(wid)
    wait_state(wid, "maximized")
    click_window(wid)
    wait_state(wid, "normal")


def scenario_no_wm(pid: int, title: str | None, log_path: str):
    wid = wait_window(pid, title)
    time.sleep(0.6)
    check(query_state(wid) == "normal", "无 WM 环境不应有最大化状态")
    click_window(wid)
    time.sleep(0.5)
    click_window(wid)
    time.sleep(0.6)

    text = inspect_log(log_path)
    check("panic" not in text.lower(), "无 WM 时应用 panic")
    clicks = re.findall(r"\[maximize\] click target=(true|false)", text)
    check(
        len(clicks) >= 2 and clicks[0] == "true" and clicks[1] == "false",
        f"无 WM 时 click 未按 desired 翻转: {clicks}",
    )
    check(
        "[maximize] request failed" in text and "WM 未声明" in text,
        "无 WM 时没有明确的 send failed 日志",
    )
    check(query_state(wid) == "normal", "无 WM 不应出现 _NET_WM_STATE 最大化")
    try:
        os.kill(pid, 0)
    except OSError as error:
        fail(f"无 WM 场景应用已退出: {error}")
    log(f"no-wm clicks={clicks[:2]} request-failed-visible alive=yes")


def main():
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="scenario", required=True)

    def add_pid(p, name="--pid", required=True):
        p.add_argument(name, type=int, required=required)
        p.add_argument("--title", default=None)

    p = sub.add_parser("single")
    add_pid(p)
    p = sub.add_parser("fast")
    add_pid(p)
    p.add_argument("--count", type=int, default=6)
    p.add_argument("--interval", type=float, default=0.05)
    p.add_argument("--log", default=None)
    p = sub.add_parser("slow")
    add_pid(p)
    p.add_argument("--count", type=int, default=6)
    p.add_argument("--interval", type=float, default=1.2)
    p = sub.add_parser("two")
    p.add_argument("--pid-a", type=int, required=True)
    p.add_argument("--title-a", default=None)
    p.add_argument("--pid-b", type=int, required=True)
    p.add_argument("--title-b", default=None)
    p = sub.add_parser("external")
    add_pid(p)
    p = sub.add_parser("timeout")
    add_pid(p)
    p.add_argument("--openbox-pid", type=int, required=True)
    p.add_argument("--log", required=True)
    p = sub.add_parser("startup-max")
    add_pid(p)
    p = sub.add_parser("no-wm")
    add_pid(p)
    p.add_argument("--log", required=True)

    args = parser.parse_args()
    try:
        if args.scenario == "single":
            scenario_single(args.pid, args.title)
        elif args.scenario == "fast":
            scenario_fast(args.pid, args.title, args.count, args.interval, args.log)
        elif args.scenario == "slow":
            scenario_slow(args.pid, args.title, args.count, args.interval)
        elif args.scenario == "two":
            scenario_two(args.pid_a, args.pid_b, args.title_a, args.title_b)
        elif args.scenario == "external":
            scenario_external(args.pid, args.title)
        elif args.scenario == "timeout":
            scenario_timeout(args.pid, args.title, args.openbox_pid, args.log)
        elif args.scenario == "startup-max":
            scenario_startup_max(args.pid, args.title)
        elif args.scenario == "no-wm":
            scenario_no_wm(args.pid, args.title, args.log)
    except E2EFailure as error:
        print(f"[e2e] FAIL: {error}", flush=True)
        return 1
    print(f"[e2e] PASS {args.scenario}", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
