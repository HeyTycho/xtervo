# Tervo 最终人工复测清单

> 状态：模板，最终 release 后填充实际 commit/版本/截图路径。  
> 目标：自动化无法覆盖的真机/输入法/GPU/真实网络项，交给用户逐项签字。

## 0. 版本信息（最终填写）

```text
commit:    dfdb25bab0c5
release:   dist/tervo-stripped（40.4 MB，sha256 16d7a477…73321cb）
tar.gz:    dist/tervo-0.1.0-linux-x86_64.tar.gz（sha256 d88a4479…c76fd3）
deb:       dist/tervo_0.1.0-1_amd64.deb（sha256 8dc34bf7…a76c604）
built:     2026-11-06T19:26:31Z（SOURCE_DATE_EPOCH）
测试:      app 369 passed / 1 ignored；workspace 全绿
e2e:       15 个脚本全 PASS（SSH/SFTP、插件、设置、图片、键盘、事件、渲染、P0、URL/右键等）
```

## 1. 必测：中文输入法（最高优先）

- [ ] 启动 Tervo，打开 dsh-tui
- [ ] `/btw test` 打开右侧 BtwComposer
- [ ] 用鼠标点右侧 composer
- [ ] 输入中文拼音，候选框出现在右侧输入框附近
- [ ] 组合中退格/回车只作用于输入法，不写入 shell
- [ ] 左 Shift / 右 Shift 不删除 dsh 已输入内容
- [ ] 切回普通 shell，输入法正常

证据：`TERVO_IME_LOG=1` 日志中 `[ime] spot` 的 `win=` 在右侧；截图。

## 2. 必测：GPU / 高分辨率

- [ ] 有 `/dev/dri` 时日志 `actual_renderer=skia-opengl`、`GL_RENDERER` 为 NVIDIA
- [ ] 拖动窗口大小实时跟手，字符不拉伸
- [ ] 2560x1413 / 高 DPI 下文字清晰，无黑屏/闪烁
- [ ] 无 GPU 时自动 software fallback，不崩溃

## 3. 必测：Settings Tab

- [ ] 齿轮打开 Settings Tab
- [ ] 左侧分类图标/分组正常，窗口小时侧栏可滚动
- [ ] 点 Terminal Tab 后 Settings Tab 仍在；点回能恢复，状态/搜索词不丢
- [ ] 点 + / Profile 只 suspend，不销毁 Settings Tab
- [ ] 只有 Settings Tab × / 页头 × / Esc / 齿轮关闭；关闭后终端键盘正常
- [ ] 主题下拉/光标/滚动条/Backspace/Delete 修改即时生效
- [ ] Dim/Terminal opacity 滑块只有左键按住拖动才变，悬停不跟随
- [ ] 放大窗口时设置页侧栏/内容卡片跟着变宽
- [ ] `TERVO_SETTINGS_LEGACY=1` 可回旧 modal
- [ ] 搜索 `sixel` 能跳 Terminal/Images
- [ ] Appearance 顶部 Language 切 English/中文：整页即时切换，无需重启；config 写回 `language`
- [ ] en 下 Settings 无中文（语言选项“中文”除外），zh 下无整句英文；搜索 `sixel`/`光标` 命中项行跟随语言
- [ ] 重启后语言保持；外部编辑 config.toml 的 `language` 能热更新
- [ ] 终端右键菜单（Link/Clipboard/Pane/View/Session）与 TabBar 菜单跟随语言；en/zh 均无混排
- [ ] 小窗口（约 760×520）与 `TERVO_SETTINGS_LEGACY=1` 下语言卡片/文字不溢出
- [ ] Settings 齿轮明确可辨（8 齿 + 中心孔），窗口右上角/Tab 图标一致
- [ ] 终端右键菜单无图标，文字与分组标题左对齐；280px 宽/窄窗滚动/右下翻转正常
- [ ] Pin/Unpin Tab：pinned 固定在左侧、图钉角标、重启后保留
- [ ] 拖拽 Tab：ghost/插入线/让位动画；pinned 跨组不亮线不移动；Esc 取消复位
- [ ] 拖拽后顺序正确，关闭/重命名/复制无回归

## 4. 必测：右键菜单美化

- [ ] 字号明显大于旧版，分组标题/分隔线清晰
- [ ] 菜单分类符合：Clipboard / Pane / View / Session / Link
- [ ] 窄窗口/靠右靠底不溢出屏幕
- [ ] 所有旧动作仍可用：Copy/Paste/Split/Close/Find/Select All/Clear/Save/Read Only/Reset
- [ ] TUI 鼠标上报模式下右键/Shift+右键行为不变

## 5. 必测：Sixel / emoji

- [ ] 发送真实 Sixel 图片，按光标位置显示
- [ ] 滚动/resize/切 Tab 后图片不跑位
- [ ] 关闭 `sixel` 后已有图清除，DA1 不再含参数 4
- [ ] 😀🚀❤️ 显示彩色；👨‍👩‍👧 组件彩色不越界
- [ ] `TERVO_COLOR_EMOJI=0` 回退单色/占位

## 6. 必测：剪贴板/链接

- [ ] 拖选自动复制；Ctrl+Shift+C 同时写剪贴板/PRIMARY
- [ ] 中键优先 PRIMARY，空时回退剪贴板
- [ ] OSC52 set/query/clear；`osc52=deny` 拒绝
- [ ] Ctrl+click http/https/file URL 打开；OSC8 优先
- [ ] 文件路径 Ctrl+click 打开/失败提示
- [ ] URL hover 手型；右键 Open/Copy Link

## 7. 必测：Profiles / 会话恢复

- [ ] 新建/编辑/复制/删除 Profile
- [ ] 每 Profile 字体/颜色/命令/登录 shell/起始目录生效
- [ ] 重启后恢复 Tab/分屏/cwd/滚动位置
- [ ] 设置 default_profile_id 后首次启动直接打开该 Profile（SSH 默认 Profile 不再落本地 shell）
- [ ] 关闭窗口确认行为符合配置

## 8. 必测：SSH/SFTP/Vault

- [ ] SSH 密码/密钥/agent 登录
- [ ] known_hosts 首次确认/变更提示
- [ ] SFTP 列目录/上传/下载/取消/重命名/删除
- [ ] 远端挂起/断网时 UI 不冻结，10s 超时/可取消
- [ ] Vault 解锁/锁定/错误密码；解锁时 UI 不卡

## 9. 必测：打包/升级

- [ ] 干净目录安装/启动
- [ ] 配置/Profile 从旧版本迁移
- [ ] 升级/回滚不丢配置
- [ ] 桌面文件/图标/卸载

## 9. 必测：搜索 / URL / 键盘协议

### 搜索
- [ ] Ctrl+F 打开即聚焦，输入显示 `2/3` 计数
- [ ] Enter/Shift+Enter 正向/反向环绕
- [ ] 鼠标点击/拖选/Ctrl+A/C/V 只作用于搜索框
- [ ] Match case / regex；非法 regex 红框不崩溃
- [ ] Esc 关闭后终端立即能输入，查询词不泄漏

### URL / 路径
- [ ] URL hover 变手型，离开恢复
- [ ] Ctrl+click URL/OSC8 打开；失败有底部提示
- [ ] 本地路径 Ctrl+click 打开；缺失/无权限不 spawn
- [ ] 右键 Open/Copy Link 正常

### 键盘协议
- [ ] 普通 shell 输入字节与旧版一致（Backspace/Delete/方向键）
- [ ] Kitty flags=1 应用：Ctrl+Shift+J、Shift+Enter 序列正确
- [ ] modifyOtherKeys level 2：Ctrl+A = `CSI 27;5;97~`
- [ ] 右 Shift 不再删除 dsh 输入
- [ ] 应用未请求协议时无额外 CSI-u

## 9.1 必测：Profile / 会话恢复

- [ ] Profile 表单可设置字体/主题/光标/滚动/命令/登录 shell
- [ ] 打开 Profile 应用对应外观；空字段继承全局
- [ ] 正常退出后重启恢复 Tab/分屏/焦点/cwd
- [ ] 损坏快照回退新 Tab，不崩溃
- [ ] SSH/Serial 不自动重连（已知限制）

## 9.2 必测：Sixel / IIP

- [ ] 真实 Sixel 图片按光标显示；滚动/resize/切 Tab 不跑位
- [ ] 关闭 Sixel 立即清图，DA1 不含参数 4
- [ ] OSC 1337 File PNG/JPEG 显示；滚动/resize 正常
- [ ] 关闭 IIP 立即清图，不影响 Sixel
- [ ] 非法/超限图片只丢图，不崩溃

## 9.3 必测：打包/升级

- [ ] tar.gz 干净 prefix 安装/启动/卸载
- [ ] .deb `dpkg-deb -x` 解包后 `--version/--help` 正常
- [ ] 升级保留上一版；`install.sh --rollback` 可回退
- [ ] 升级/回滚/卸载不丢 config/Profile（保留 .bak）
- [ ] 运行中覆盖不杀旧进程

## 9.4 必测：插件 V1（若最终包含）

- [ ] Plugins 页显示外部插件状态/启用禁用/错误
- [ ] 命令插件在命令面板执行并返回结果
- [ ] 插件崩溃/超时被禁用，不拖死终端
- [ ] 权限未声明的能力被拒绝

## 9.5 内存收敛结果（已修复）

- 极端 22 Tab + 3 split（4 pane）+ 图片/emoji/搜索/resize 的 30m soak：
  RSS 753.2→817.2 MiB，**+64.0 MiB（<96 MiB 门禁）**，+0.78 MiB/min，R²=0.13，
  约 6 分钟后平台、末段略降；线程/FD/子进程稳定，无 panic/OOM。
- 验收矩阵（同一修复 HEAD）：2m +39.5MiB PASS；15m +74.2MiB PASS；
  30m +64.0MiB PASS（30m 低于 15m 是 run 间基线与末期回落，不是泄漏）。
- 根因：IIP/Sixel 图片锚点依赖 alacritty `history_size()`；全局 scrollback 预算封顶后
  锚点不再推进，滚出历史的图片永久保留。修复为单调 `scroll_total` + `retain_reachable`。
- 对照：修复前 30m +209.6 MiB；消融 full vs no-images 10m 为 +87.4 vs +60.0 MiB，
  图片模块贡献约 27 MiB/10m。
- 残余：预热前 3 分钟还有一次性 ~64MiB（scrollback 行/cell 容量 + Skia/llvmpipe 缓存），
  之后平台；这是高水位，不是线性泄漏。30m 结果不外推 24h。
- 如你复测发现日常 Tab 数下 RSS 持续上涨/卡顿，请记录 Tab 数、操作、`/proc/<pid>/status` 采样。

## 10. 已知限制（不要求修复）

- fcitx XIM 候选窗位置在部分输入法下仍受 winit/XIM 限制；dsh Btw 侧栏需先点击 composer 锚点
- 彩色位图 emoji 之外（COLR/SVG）回退单色/占位
- Sixel/IIP 局部 ECH 擦除不逐像素清图；scrollback 淘汰后锚点可能偏移
- IIP 覆盖文本、不占光标行；不做 inline=0 文件下载/动画
- 会话恢复不恢复 scrollback/屏幕内容/运行中进程；SSH/Serial 不自动重连
- 打包不含自动更新/崩溃上报/AppImage/Flatpak/Snap/GPG 签名
- Wayland 仅 spike，不承诺完整
- Bidi/非 UTF-8/rewrap/OSC133/Kitty graphics 未实现

## 11. 签字

```text
复测人：
日期：
结论：通过 / 有条件通过 / 阻塞项（当前建议：功能有条件通过，内存限制知情）
备注：
```
