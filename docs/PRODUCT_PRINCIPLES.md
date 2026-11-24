# xtervo 产品原则与路标

状态：**产品北极星文档**。实现细节可以变，下面的定位、非目标、决策优先级
和阶段门禁不随单个功能改动。修改本文需要明确说明理由。

最后更新：2026-11-12

---

## 0. 北极星

```text
xtervo 是一个 Linux 原生的开发者工作终端：
像原生终端一样快，像现代终端一样完整，
数据可信、界面克制、扩展有边界。
```

目标用户：

```text
长时间泡在 shell 里的 Linux 开发者 / 运维 / SSH 用户。
每天开很多 tab / split，使用 vim/tmux/dsh/TUI，
需要 SSH/SFTP、图片协议、低输入延迟和可靠会话。
```

一句话定位：

```text
给 Linux 开发者的、用着顺手的原生终端：
交互灵感参考 Tabby 等现代终端，但不做替代品，也不追求像素级复刻。
不是功能最多的终端，而是最值得每天用的终端。
```

---

## 1. 非目标

明确不做：

```text
AI 终端 / 核心 AI provider
云端 IDE / 账号体系 / 默认云依赖
插件市场平台（首发前）
Tabby 像素级复制品
全平台首发产品
任意 Slint UI 插件平台
插件承载 MCP
为了生态先做没有信任链的市场
```

AI Doctor 以后可以作为**独立 assistant 插件**存在；核心不实现模型/provider/密钥。
MCP 是宿主内嵌功能，不是插件能力。

---

## 2. 六条不可妥协

### 2.1 原生、GPU-first，但 GPU 没有存在感

```text
Rust + Slint
GPU 默认
CPU 自动 fallback
输入低延迟
滚动/resize 不闪
图片正确合成
内存有边界
```

用户不应感觉到“GPU 在渲染”，只应感觉到快、顺、没有奇怪闪烁。
不能为了某个 GPU 路径牺牲默认可用性；失败必须回退而不是黑屏。

### 2.2 终端正确性优先

```text
VT/xterm 行为
Kitty keyboard
Sixel / Kitty / IIP
鼠标 / 焦点 / 括号粘贴
IME
链接 / OSC
TUI 兼容
```

终端首先是终端。插件、面板、AI、动画都不能破坏字节流和显示正确性。
图片/过滤层改动必须有逐字节或逐像素差分测试。

### 2.3 数据可信

```text
崩溃不丢 session
配置不静默损坏
更新可回滚
本地优先
默认不联网
权限显式
```

这是 xtervo 与“功能 demo”最大的区别。
产品级标准不是“能启动”，而是：

```text
kill -9、断电、磁盘满、配置损坏之后，用户仍然知道发生了什么、能恢复。
```

### 2.4 键盘优先、界面克制

```text
command palette
可编辑快捷键
tab/split 快速操作
搜索/复制/滚动可键盘完成
设置不藏关键项
不抢焦点
不弹无关通知
```

好的终端应该消失在工作流里，而不是一直提醒用户它存在。

### 2.5 扩展有边界

```text
进程插件 v3
WASM output filter
声明式 panel/settings
权限确认
本地安装/回滚/签名
```

但：

```text
不允许任意 Slint UI
不允许插件 MCP
不允许插件绕过宿主渲染
不允许未确认的权限静默生效
```

插件是“扩展终端”，不是“把终端变成插件容器”。

### 2.6 本地自动化，而不是云端智能

```text
MCP 核心内嵌
本地 stdio / live socket
读写分级
token + 显式开关
审计
```

AI Doctor 可以后置为插件，但核心不接 AI provider，不默认上传终端内容，
不把终端变成聊天窗口。

---

## 3. 体验契约

合格版本必须做到：

```text
打开快
输入跟手
滚动稳
resize 不闪
图片不飘
TUI 不花
SSH 不断
崩溃能恢复
更新不吓人
配置可读可改
出问题能一键生成支持包
```

用户的最终评价应该是：

```text
“没什么存在感，但我不想换回去。”
```

而不是：

```text
“功能很多，但偶尔丢 tab / 花屏 / 卡一下。”
```

---

## 4. 决策优先级

功能/架构冲突时，按以下顺序取舍：

```text
1. 用户数据不丢
2. 终端字节/显示正确
3. 延迟、稳定、内存
4. 日常 UX 完整
5. 可维护、可诊断
6. 插件生态
7. AI / 市场 / 跨平台
```

推论：

```text
不为插件市场牺牲稳定性
不为 AI 牺牲终端正确性
不为跨平台牺牲 Linux 首发质量
不为了“功能齐全”增加默认联网/常驻后台
```

---

## 5. 发布形态

首发：

```text
Linux-first
单个原生二进制
XDG 配置/状态目录
TOML 配置
本地 SSH/SFTP/Profile
本地插件目录
本地 MCP
无云依赖
GPL-3.0-only
```

分发目标：

```text
deb / rpm / AppImage
Flatpak（建议）
签名包 + 签名元数据
自动更新 + 失败回滚
stable / beta / nightly 通道
```

跨平台是后续决策，不是首发承诺。

---

## 6. 路标

```text
M0 功能完整 RC
M1 数据安全与支持闭环
M2 发布工程与安装更新
M3 真机可靠性、安全与合规
M4 v0.4.0 stable
M5 产品 Beta / v1.0
M6 插件生态与跨平台
```

### M0 功能完整 RC（已完成）

```text
GPU-native + text/image damage scissor
MCP live terminal control
插件 v3 完整宿主
WASM output filter
本地插件安装/更新/回滚/签名
Feedback bundle
30m/CI/e2e
```

### M1 数据安全与支持闭环

```text
会话 autosave + unclean shutdown 恢复
session/config fsync + last-known-good
配置/快照损坏恢复 UI
panic/backtrace/crash marker
一键支持包（GPU/renderer/更新通道/crash/log/config）
kill -9、ENOSPC、权限拒绝、配置损坏测试
```

### M2 发布工程与安装更新

```text
deb/rpm/AppImage/Flatpak
稳定仓库/下载页
签名包 + 更新元数据
自动更新 + 回滚
stable/beta/nightly
配置 schema 迁移
```

### M3 真机可靠性、安全与合规

```text
NVIDIA/AMD/Intel 真机
X11 + Wayland
多显示器/高 DPI/休眠恢复
24h soak
CVE/SBOM/供应链
Vault 威胁模型
插件信任边界
隐私/遥测 opt-in
GPL 合规
```

### M4 v0.4.0 stable

```text
无未关闭 P0
安装/更新/回滚可用
崩溃不丢 session/config
真机矩阵通过
30m + 24h 证据
文档/隐私/许可证齐全
```

### M5 产品 Beta / v1.0

```text
首次启动引导
从 Tabby 导入 profile/SSH/主题/快捷键
设为默认终端
Shell integration
无障碍/键盘可达性
中英文完整
支持工单/反馈闭环
```

### M6 插件生态与跨平台

```text
插件 registry / 市场
远程更新
根信任链 / 密钥轮换 / 吊销
在线 key 分发
插件沙箱/签名发布者
AI Doctor 独立插件
Windows/macOS（如果决定做）
```

原则：

```text
先有签名和信任链，再做市场 UI
先有 Linux 产品，再决定跨平台
AI Doctor 是插件，不进核心 provider
```

---

## 7. 成功与失败判定

产品成功的最低标准：

```text
普通用户安装后能完成第一次使用
连续工作一天不丢 tab / 配置
崩溃/断电后能恢复最近会话
更新失败能回滚
真实 GPU 和 Wayland 不黑屏
出问题能一键生成支持包
```

失败信号：

```text
为了新功能牺牲终端正确性
崩溃后静默重置配置/会话
默认联网或收集数据
插件权限提示形同虚设
更新通道没有签名/回滚
只在 llvmpipe 上验证就宣称支持 GPU
```

---

## 8. 文档关系

```text
产品定位/原则/路标      docs/PRODUCT_PRINCIPLES.md
当前实现状态/已知差距    docs/MAINLINE_STATUS.md
功能对照                  docs/FEATURE_MATRIX.md
插件协议                  docs/PLUGIN_V3.md
插件安装/签名             docs/PLUGIN_INSTALL.md
MCP 控制                  docs/MCP_TERMINAL_CONTROL.md
发布流程                  docs/RELEASING.md
```
