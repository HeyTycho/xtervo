#!/usr/bin/env python3
"""XTEST 小工具：move/activate/click/type/key，供 verify_ime_x11.sh 调用。"""
import ctypes
import ctypes.util
import sys
import time

x11 = ctypes.CDLL(ctypes.util.find_library("X11"))
xtst = ctypes.CDLL(ctypes.util.find_library("Xtst"))

x11.XOpenDisplay.restype = ctypes.c_void_p
x11.XOpenDisplay.argtypes = [ctypes.c_char_p]
x11.XDefaultRootWindow.restype = ctypes.c_ulong
x11.XDefaultRootWindow.argtypes = [ctypes.c_void_p]
x11.XStringToKeysym.restype = ctypes.c_ulong
x11.XStringToKeysym.argtypes = [ctypes.c_char_p]
x11.XKeysymToKeycode.restype = ctypes.c_ubyte
x11.XKeysymToKeycode.argtypes = [ctypes.c_void_p, ctypes.c_ulong]
x11.XQueryTree.argtypes = [ctypes.c_void_p, ctypes.c_ulong,
                           ctypes.POINTER(ctypes.c_ulong), ctypes.POINTER(ctypes.c_ulong),
                           ctypes.POINTER(ctypes.POINTER(ctypes.c_ulong)),
                           ctypes.POINTER(ctypes.c_uint)]
x11.XMoveWindow.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int, ctypes.c_int]
x11.XSetInputFocus.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int, ctypes.c_ulong]
x11.XSetInputFocus.restype = ctypes.c_int
x11.XRaiseWindow.argtypes = [ctypes.c_void_p, ctypes.c_ulong]
x11.XFlush.argtypes = [ctypes.c_void_p]
x11.XSync.argtypes = [ctypes.c_void_p, ctypes.c_int]
x11.XInternAtom.restype = ctypes.c_ulong
x11.XInternAtom.argtypes = [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_int]
x11.XSendEvent.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int,
                           ctypes.c_long, ctypes.c_void_p]
xtst.XTestFakeMotionEvent.argtypes = [ctypes.c_void_p, ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_ulong]
xtst.XTestFakeButtonEvent.argtypes = [ctypes.c_void_p, ctypes.c_uint, ctypes.c_int, ctypes.c_ulong]
xtst.XTestFakeKeyEvent.argtypes = [ctypes.c_void_p, ctypes.c_uint, ctypes.c_int, ctypes.c_ulong]


class ClientMessage(ctypes.Structure):
    _fields_ = [
        ("type", ctypes.c_int),
        ("serial", ctypes.c_ulong),
        ("send_event", ctypes.c_int),
        ("display", ctypes.c_void_p),
        ("window", ctypes.c_ulong),
        ("message_type", ctypes.c_ulong),
        ("format", ctypes.c_int),
        ("data", ctypes.c_long * 5),
    ]


ERRHANDLER = ctypes.CFUNCTYPE(ctypes.c_int, ctypes.c_void_p, ctypes.c_void_p)


def main() -> int:
    # 安全护栏：必须显式指定 --display，绝不允许回退到当前会话的 $DISPLAY。
    # 之前就是这里漏了，helper 拿 :1 + Xvfb 的窗口 id，按键打进了用户正在用的终端。
    if "--display" not in sys.argv:
        print("refuse: 必须显式传 --display :N（禁止隐式使用 $DISPLAY）", file=sys.stderr)
        return 2
    idx = sys.argv.index("--display")
    try:
        disp = sys.argv[idx + 1]
    except IndexError:
        print("refuse: --display 缺少值", file=sys.stderr)
        return 2
    del sys.argv[idx:idx + 2]
    dpy = x11.XOpenDisplay(disp.encode())
    if not dpy:
        print("no display", file=sys.stderr)
        return 1
    x11.XSetErrorHandler.restype = ctypes.c_void_p
    x11.XSetErrorHandler.argtypes = [ERRHANDLER]
    _handler = ERRHANDLER(lambda d, e: 0)
    x11.XSetErrorHandler(_handler)
    args = sys.argv[1:]
    wid = int(args[0], 16) if args and args[0].startswith("0x") else 0
    root = x11.XDefaultRootWindow(dpy)

    def ancestors(win):
        chain = [win]
        cur = win
        for _ in range(8):
            r = ctypes.c_ulong(); p = ctypes.c_ulong()
            kids = ctypes.POINTER(ctypes.c_ulong)(); n = ctypes.c_uint()
            if not x11.XQueryTree(dpy, cur, ctypes.byref(r), ctypes.byref(p),
                                  ctypes.byref(kids), ctypes.byref(n)):
                break
            parent = p.value
            if parent in (0, root, cur):
                break
            chain.append(parent)
            cur = parent
        return chain

    for op in args[1:]:
        kind, _, value = op.partition(":")
        if kind == "move":
            x, y = map(int, value.split(","))
            top = ancestors(wid)[-1]
            x11.XMoveWindow(dpy, top, x, y)
            for w in reversed(ancestors(wid)):
                x11.XRaiseWindow(dpy, w)
            x11.XSync(dpy, 0)
            time.sleep(0.4)
        elif kind == "activate":
            ev = ClientMessage()
            ev.type = 33  # ClientMessage
            ev.window = wid
            ev.message_type = x11.XInternAtom(dpy, b"_NET_ACTIVE_WINDOW", 0)
            ev.format = 32
            ev.data[0] = 1
            ev.data[1] = 0
            x11.XSendEvent(dpy, root, 0, (1 << 19) | (1 << 20), ctypes.byref(ev))
            x11.XRaiseWindow(dpy, wid)
            x11.XSetInputFocus(dpy, wid, 1, 0)   # 无 WM 时也能拿到键盘焦点
            x11.XSync(dpy, 0)
            time.sleep(0.3)
        elif kind == "click":
            x, y = map(int, value.split(","))
            xtst.XTestFakeMotionEvent(dpy, -1, x, y, 0)
            xtst.XTestFakeButtonEvent(dpy, 1, 1, 0)
            xtst.XTestFakeButtonEvent(dpy, 1, 0, 0)
            x11.XSync(dpy, 0)
            time.sleep(0.3)
        elif kind == "type":
            for ch in value:
                name = {" ": "space", "-": "minus", ".": "period", "\\": "backslash",
                        "'": "apostrophe", "/": "slash", "|": "bar", ";": "semicolon"}.get(ch, ch)
                kc = x11.XKeysymToKeycode(dpy, x11.XStringToKeysym(name.encode()))
                xtst.XTestFakeKeyEvent(dpy, kc, 1, 0); x11.XFlush(dpy); time.sleep(0.05)
                xtst.XTestFakeKeyEvent(dpy, kc, 0, 0); x11.XFlush(dpy); time.sleep(0.05)
            x11.XSync(dpy, 0)
        elif kind == "key":
            kc = x11.XKeysymToKeycode(dpy, x11.XStringToKeysym(value.encode()))
            xtst.XTestFakeKeyEvent(dpy, kc, 1, 0); x11.XFlush(dpy); time.sleep(0.06)
            xtst.XTestFakeKeyEvent(dpy, kc, 0, 0); x11.XFlush(dpy)
            x11.XSync(dpy, 0)
        elif kind == "chord":
            mods, _, key = value.partition("+")
            mks = {"ctrl": "Control_L", "shift": "Shift_L", "alt": "Alt_L", "meta": "Super_L"}
            codes = [x11.XKeysymToKeycode(dpy, x11.XStringToKeysym(mks[m].encode())) for m in mods.split("-") if m in mks]
            for kc in codes:
                xtst.XTestFakeKeyEvent(dpy, kc, 1, 0); x11.XFlush(dpy); time.sleep(0.04)
            kc = x11.XKeysymToKeycode(dpy, x11.XStringToKeysym(key.encode()))
            xtst.XTestFakeKeyEvent(dpy, kc, 1, 0); x11.XFlush(dpy); time.sleep(0.04)
            xtst.XTestFakeKeyEvent(dpy, kc, 0, 0); x11.XFlush(dpy); time.sleep(0.03)
            for kc in reversed(codes):
                xtst.XTestFakeKeyEvent(dpy, kc, 0, 0); x11.XFlush(dpy); time.sleep(0.04)
            x11.XSync(dpy, 0)
        elif kind == "sleep":
            time.sleep(float(value))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
