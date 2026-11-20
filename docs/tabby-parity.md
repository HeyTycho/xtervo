# Tabby 功能对照表

目标：替代 Tabby。下表按 Tabby 的公开功能面盘点现状，作为后续排期依据。

图例：✅ 已实现 ｜ 🟡 部分 ｜ ⬜ 未实现

## 终端核心

| 功能 | 状态 | 说明 |
|---|---|---|
| 本地 shell | ✅ | `$SHELL` / 配置 `shell` |
| VT 解析（alacritty_terminal） | ✅ | 0.26，行级 damage |
| 真彩色 / 256 色 | ✅ | 主题调色板 |
| 主题配色 | ✅ | 12 套内置（Yaru Dark/Light、Tokyo Night、Catppuccin 4 变体、Dracula、Nord、Gruvbox、One Dark、Solarized Light），终端 + UI chrome 同步换色；默认 Auto 跟随系统（Yaru 映射）；可直接加载 iTerm2/WT/Alacritty/kitty/Xresources/YAML 主题 |
| 字体大小 | ✅ | 设置面板实时生效 |
| 字体族 / 字重 / 连字 | 🟡 | 固定 JetBrains Mono + DejaVu；中文自动回退 WenQuanYi Micro Hei / Droid / Noto CJK，暂不支持自选字体与连字 |
| 历史回滚行数 | 🟡 | 可配置，对新 pane 生效 |
| 滚轮回滚 | ✅ | 鼠标滚轮，支持回看历史 |
| 文本选择 / 复制粘贴 | ✅ | 拖选自动复制、Ctrl+Shift+C/V、右键菜单 |
| 超链接识别 | ⬜ | |
| 搜索（Ctrl+Shift+F） | ✅ | 增量搜索 + 命中高亮、Enter/Shift+Enter 跳转 |
| 输入法 / 中文输入 | ✅ | XIM（fcitx/ibus）；候选窗跟随光标，组合中回车/退格不写入 PTY |
| 宽字符 / 中文渲染 | ✅ | CJK 字体回退 + 两格宽字形居中；emoji 暂留空 |
| 光标闪烁 / 形状 | 🟡 | 形状已跟随 VT，未做闪烁 |
| 响铃 / 通知 | 🟡 | 事件已上报，未做 UI |
| 鼠标上报（vim/tmux） | ✅ | 1000/1002/1003 + SGR 1006 / UTF-8 1005 / legacy X10；Shift+点击绕过上报本地选词；滚轮随模式转为 64/65 |

## 窗口 / 布局

| 功能 | 状态 | 说明 |
|---|---|---|
| 标签页（新建/切换/关闭） | ✅ | Ctrl+Shift+T / Ctrl+Shift+W |
| 标签标题（OSC） | ✅ | 事件唤醒（<50ms），400ms 兜底 |
| 标签拖拽排序 / 重命名 / 配色 | ⬜ | |
| 分屏（左右/上下） | ✅ | 右键菜单；只显示分隔线，无整圈外框 |
| 分屏比例拖动 | ✅ | 拖动 1px 分隔线，比例夹在 10%–90% |
| 无边框窗口 + 自绘标题栏 | ✅ | no-frame |
| 最小化 / 最大化 / 关闭 | ✅ | |
| 双击标题栏最大化 | ✅ | |
| 窗口边缘缩放 | ✅ | 自绘四边/四角命中区，直接改 size/position |
| 透明度 / 背景图 / 模糊 | ⬜ | |
| 全屏 | 🟡 | 可用最大化替代 |
| Quake 模式（热键下拉） | ⬜ | |
| 会话恢复（重启还原布局） | ⚠️ partial | 菜单 Restart Session / `--restore` / 可选 `restore_session_on_startup`：只还原 tabs、分屏树、cwd、窗口几何；不还原运行中进程/滚动历史/远端连接 |

## 连接类型

| 功能 | 状态 | 说明 |
|---|---|---|
| SSH | ⬜ | 无原生 russh 实现；SSH Profile 目前调用系统 `ssh`，插件 SSH 走 sidecar |
| SSH 跳板 / 密钥 / agent | ⬜ | |
| 端口转发 | ⬜ | |
| SFTP 面板 | ⬜ | |
| Serial | ⬜ | 未实现；Cargo 无 serialport/tokio-serial 依赖 |
| Telnet | ⬜ | |
| tmux 集成 | ⬜ | |

## 配置 / 扩展

| 功能 | 状态 | 说明 |
|---|---|---|
| 设置界面 | ✅ | 主题 / 字号 / 回滚行数 |
| 配置文件持久化 | ✅ | `~/.config/tervo/config.toml`（可 `TERVO_CONFIG` 覆盖） |
| 配置热加载 | 🟡 | 启动时读取，运行中改动即时写回 |
| Profile（shell/args/env/cwd） | 🟡 | 仅 shell 可配 |
| 快捷键自定义 | ⬜ | |
| 插件系统 | ⬜ | 依赖已锁定 fidius（dylib + wasm） |
| 凭据库 / Vault | ⬜ | |
| 配置导入导出 / 同步 | ⬜ | |
| 自动更新 | ⬜ | |
| 多语言 | ⬜ | |

## 当前建议的下一批（按用户价值排序）

1. **主题热重载**：自定义主题文件目前只在启动时扫描，编辑后需重启；下一步加 `notify` 监听（格式与目录已完成，见 [`themes.md`](themes.md)）。
2. **Profile 完善**：args/env/cwd + 新建 tab 时选择 profile。
3. **SSH / Serial**：作为 pane 的传输层接入现有 actor。
4. **Quake 模式 / 标签拖拽排序 / 分屏快捷键**：体验打磨。
5. **超链接 / 光标闪烁**：终端兼容性细节（鼠标上报已完成，见上文状态表）。
6. **插件宿主**：最后做，且只做自有插件 API，不承诺兼容 Tabby 插件。
