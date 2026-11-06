# Tervo 最终人工复测清单

> 状态：模板，最终 release 后填充实际 commit/版本/截图路径。  
> 目标：自动化无法覆盖的真机/输入法/GPU/真实网络项，交给用户逐项签字。

## 0. 版本信息（最终填写）

```text
commit:
release binary:
stripped binary:
构建时间:
测试结果:
e2e:
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
- [ ] 点 Terminal Tab 后 Settings Tab 仍在；点回能恢复
- [ ] 只有 ×/Esc/齿轮关闭；关闭后终端键盘正常
- [ ] 主题下拉/光标/滚动条/Backspace/Delete 修改即时生效
- [ ] `TERVO_SETTINGS_LEGACY=1` 可回旧 modal
- [ ] 搜索 `sixel` 能跳 Terminal/Images

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

## 10. 已知限制（不要求修复）

- fcitx XIM 候选窗位置在部分输入法下仍受 winit/XIM 限制
- 彩色位图 emoji 之外（COLR/SVG）回退单色/占位
- Sixel 局部 ECH 擦除不逐像素清图；scrollback 淘汰后锚点可能偏移
- Wayland 仅 spike，不承诺完整
- Bidi/非 UTF-8/rewrap/OSC133/Kitty graphics 未实现

## 11. 签字

```text
复测人：
日期：
结论：通过 / 有条件通过 / 阻塞项
备注：
```
