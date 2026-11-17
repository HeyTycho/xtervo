# 核心终端从 69% 到 80%：17 条清单与今晚排班

> 2026-11-17 · 口径：只算 P0 终端本体（100 条），连接插件不算核心。
> 当前：45 满分 + 48 半分 + 1×0.25 = **69.2%**；到 80% 需 **+10.75 分**。

## 1. 数学

```text
5 条 not_started 全做完      +5.0
12 条 partial 全部补齐        +6.0
                              = 80.2%  ✅ 目标
```

也就是说：**17 条**，不是“全部 48 条 partial”。

## 2. 17 条清单（按用户感知强度排序）

### A. 今晚必做的 5 条 not_started

| # | ID | 内容 | 做法 | 估时 |
|---|---|---|---|---|
| 1 | UI-TERM-003 | 滚到顶/底命令 | 加 `view.scroll-top/bottom` 命令 + 默认键，pane 滚动 API 已有 | 20m |
| 2 | LOCAL-004 | 多本地 Shell Profile 同窗混用 | 已有 Profile 模型，去掉“全局单一 shell”假设，`+` 菜单按 Profile 开 Tab | 60m |
| 3 | UI-PANE-008 | Pane 拖拽/重排/移入其他 Tab | 复用 divider 拖拽框架，pane 加拖拽手柄 + 落点高亮 | 150m |
| 4 | UI-TERM-013 | 可配置右键菜单 + 插件装饰点 | 菜单项从硬编码改为表驱动，读 config + 插件注册 | 90m |
| 5 | TERM-015 | 终端录制/输出保存 API | 把 `save-output` 提到 core：录制 buf + 开始/停止/导出 | 90m |
| 6 | KEY-006 | 插件注册命令/快捷键/上下文 | v3 插件 API 已有命令通道，补 keybinding 注册与上下文过滤 | 60m |

> 小计约 8 小时；其中 3、4、5、6 是“结构性”的，做不完不要把半成品留树上。

### B. 12 条 partial 里选“感知最强”的

| # | ID | 缺口 | 做法 | 估时 |
|---|---|---|---|---|
| 7 | TERM-006 | emoji ZWJ 不合并（👨‍👩‍👧 占 6 格） | 写 cell 时按 grapheme cluster 合并，宽字符占位标记 | 180m |
| 8 | TERM-007 | 无 Nerd Font/Powerline、fallback 只有 CJK | 字体 fallback 链加 Nerd/Powerline ranges，测常用图标 | 90m |
| 9 | TERM-008 | 字体族/字重/行距/缩放不可配 | 配置项 + 重算 cell 尺寸 + 重排（设置页已有入口） | 90m |
| 10 | UI-TERM-002 | 双击选词、三击选行未做 | 鼠标多击状态机 + 词边界（Unicode word break） | 60m |
| 11 | UI-TERM-011 / TERM-012 | 路径含空格、`~user`、`file:line`；折行 URL 不拼接 | 链接识别器重写 + 行拼接 | 90m |
| 12 | UI-TERM-020 | dsh 预览黑灯箱（Kitty placement） | 按 §4 的帧数据定位：数据层/放置层/合成层 | 120m |
| 13 | UI-TAB-007 | Tab 重命名无撤销 | 重命名前存旧标题，Ctrl+Z 或再次重命名可恢复 | 30m |
| 14 | UI-TAB-011 | 无“最近使用 Tab”栈 | LRU 栈 + `tab.last` 快捷键/命令 | 40m |
| 15 | UI-TAB-009 | Tab 无连接/进度/失败状态 | 先做“活动/失败”两个状态 + 非纯颜色标记 | 60m |
| 16 | LOCAL-005 | shell 启动失败只打 stderr | 失败态面板 + 关闭/重试建议 | 40m |
| 17 | UI-WIN-008 | 关闭确认不列会话名 | 弹窗列出活动会话名 + 保留/结束选择 | 40m |

> 小计约 14 小时。**今晚不可能全做完**，见 §3 排班。

## 3. 今晚排班（18:00 起，三波）

### Wave 1 · 18:00–21:30（快赢，+3.5 分 → 72.7%）

```text
1. 滚到顶/底命令（20m）
2. 双击选词 / 三击选行（60m）
3. Tab 重命名撤销（30m）
4. 最近 Tab 栈（40m）
5. LOCAL-005 shell 失败态（40m）
6. 多本地 Profile（60m）
```

验收：单测 + 手动点一遍；不碰渲染主链路，风险低。

### Wave 2 · 21:30–01:30（观感，+2.5 分 → 75.2%）

```text
7. 链接识别（90m）——空格路径/~user/file:line + 折行拼接
8. dsh 预览黑灯箱（120m）——先抓帧判断层次
9. Nerd Font / Powerline fallback（90m）
```

风险：链接识别会碰点击命中；字体 fallback 碰渲染缓存，跑 2m 门禁。

### Wave 3 · 01:30–04:00（结构，+2 分 → 77.2%）

```text
10. Tab 状态（连接/失败）60m
11. 关闭确认列会话名 40m
12. UI-TERM-013 可配置菜单 90m
```

### 不做（今晚）

```text
emoji grapheme（3h，风险高，放 0.7 首发后再做）
pane 拖拽（2.5h）、录制 API（1.5h）、插件命令上下文（1h）
竖排 TabBar、Wayland、24h soak
```

## 4. 黑灯箱的定位方法（Wave 2 用）

```text
用户打开 dsh 预览后，我从 :1 抓窗口 → 看是哪种：
  a) Frame.images 为空      → 协议/数据层丢了（APC 解析或上限）
  b) 有图但 row/col/cells 错 → placement 计算（含 Kitty 的 c/r 与 x/y 源裁剪）
  c) 有图、位置对、全黑      → 纹理上传/合成（GPU/Skia 路径）
判据来自 core Frame dump（TERM_FRAME_DUMP）与 MCP list 的 pane 信息。
```

## 5. 今晚的完成判据

```text
硬指标：核心 69.2% → ≥75%（Wave 1+2 完成）
加分：≥77%（Wave 3 完成）
不做：不把 partial 硬改成 implemented —— CLOSURE 的 evidence 检查会拦
```
