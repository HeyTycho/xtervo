# 内联图片合成层 v2

> 状态：v2 第一刀已落地（2026-11-09）
> 目标：替换 `PaneImage` 独立 Slint overlay，把 Sixel/IIP 图片变成 terminal framebuffer 内的 cell-aware 图层。
> 参考：xterm.js `@xterm/addon-image`、WezTerm（Rust）、Kitty/iTerm2 图片协议。
> Tabby：只借产品交互/设置体验，不借 Electron/xterm.js DOM 实现。

## 0. 结论

当前问题不是单点 bug，而是合成模型错误：

```text
图片是独立 Slint Image overlay
→ 不占 cell、不参与文本 damage/ECH/滚动裁剪
→ TUI 按 cell 栅格擦图时旧 overlay 不消失
→ 滚动/resize/清屏/字符画互相干扰
```

v2 改成：

```text
终端 framebuffer:
  background
  → image layer（按 cell 矩形裁剪）
  → text cells（覆盖图片）
  → cursor/selection/link
```

图片仍然**不进入文本/选区/搜索**；点击灯箱仍使用原始 RGBA。

## 1. 目标与非目标

### 目标

- 图片锚定到绝对 scroll 行 + cell 列，滚动时与文本一起移动。
- 文本/ECH/空格重绘只覆盖对应 cell 区域，不需要“任何 ECH 清全部图”。
- resize/reflow 后图片按 cell 重算，不跑位。
- Sixel/IIP 共用同一层，独立开关/清除。
- 点击图片打开灯箱，使用原始像素；仍是独立 UI 交互。
- 对 dsh-tui 这类“图片画在 TUI 布局里”的程序行为稳定。
- 可回退到 v1 overlay（feature flag）直到 v2 测试全绿。

### 非目标

- 不重写 alacritty_terminal/PTY/actor。
- 不实现任意 Tabby/Electron 插件。
- 不做图片动画、部分透明 Sixel、跨窗口拖图。
- 不把图片复制进每个 cell 的文本数据（避免选区/搜索污染和内存爆炸）。

## 2. 参考模型

| 项目 | 可借鉴 | 不照搬 |
|---|---|---|
| xterm.js addon-image | 图片与 terminal buffer/cell 绑定；滚动/清屏/遮挡走终端模型 | DOM canvas 生命周期 |
| WezTerm | Rust 原生 terminal 图片 placement/crop/scroll | 代码/API 直接复制 |
| Kitty graphics | placement、z-index、delete、cell 对齐 | 扩展协议全集 |
| iTerm2 IIP | inline/width/height/preserveAspectRatio 语义 | 上传/文件流扩展 |
| Tabby | 灯箱/缩放/设置/视觉交互 | Electron/xterm.js 实现 |

许可证：借鉴架构与协议语义；如复制代码，按原项目许可证保留声明。Tervo 自身 GPL-3.0-only。

## 3. 数据模型

### 3.1 Core

```text
StoredImage {
  id
  kind: Sixel | Iip
  abs_line: i64          // 单调滚动绝对行
  col: u16
  width/height: u32      // 原始像素
  rgba: Arc<[u8]>
  fixed_row: Option<i32> // sixel_scrolling=false
  bytes
}
```

`Frame.images` 只输出当前视口可见的图片：

```text
InlineImage {
  id, row (可为负), col,
  width, height,
  data: Arc<[u8]>
}
```

v2 增加/保持：

- `row/col` 是视口坐标，文本帧同一坐标系。
- `data` 是原始 RGBA（不预乘）。
- 不再需要 UI 侧 Slint Image 作为视觉层；只在灯箱点击时缓存。

### 3.2 App renderer

```text
Renderer {
  framebuffer: Vec<u32>  // 每帧背景/图片/文本合成结果
  image_cache: HashMap<id, CachedImage> // 点击灯箱用
  image_hits: Vec<ImageHit>  // pane 内可点击矩形
}
```

`Frame.images` 每帧参与 `draw`：

1. 行背景填默认色
2. 把与 dirty row 相交的图片像素 blit 到 framebuffer（按 cell row 裁剪）
3. 画该行 cells（文本/背景/宽字符）
4. 画选区/搜索/下划线/光标 overlay（光标仍独立 Slint overlay）

图片像素在文本下面；文本 cell 有内容时覆盖图片。空白 cell 不覆盖，保留图片。

### 3.3 点击/灯箱

- `ImageHit { image_id, x, y, w, h }` 由 app 计算，Slint 只放透明 TouchArea。
- 点击时 app 用 `image_cache` 生成 `slint::Image`，打开现有灯箱。
- 没有独立 `ListView` 视觉 Image overlay，避免双重绘制。

## 4. 合成规则

### 4.1 裁剪

图片像素坐标：

```text
dst_x = col * cell_w
dst_y = row * cell_h
```

如果 `row < 0`，跳过顶部 `-row*cell_h` 像素；如果 `row + height` 超出 pane，裁底部；左右同理。

### 4.2 文本覆盖

cell 绘制时：

- 有字符/非默认背景：先填 cell 背景，再画字形 → 覆盖图片。
- 空白 cell、默认背景：保留图片像素。
- 宽字符占两格，按 span 覆盖。
- ECH/空格重绘：由 core 把图片覆盖区域标记为需要清除（见 4.3），renderer 不应“任何空格都保留旧图”。

### 4.3 ECH / 擦除

不再“所有 ECH 清所有图”。

core 扫描 `CSI ... X`/`K` 后，给受影响的 **cell 区域**打 `image_damage`：

```text
ImageDamage { row, col_start, col_end }
```

`ImageStore` 在下一帧：

- 图片完全被覆盖 → drop；
- 部分覆盖 → 记录 clip/mask（v2 先整图 drop，保留正确性）；
- renderer 对受影响行不保留旧像素。

过渡期可保留 `ECH → drop overlapping images`，但不得全量清图。

## 5. 滚动与生命周期

- 滚动：`row = abs_line - scroll_total + display_offset`；图片与 cells 同源。
- 用户滚动只改 `display_offset`，不重建图片数据。
- 图片完全滚出可达 history → `retain_reachable` drop。
- resize/reflow：`scroll_total` 不因 reflow 增长；图片按新 cell/grid 重算。
- 关闭 Sixel/IIP：清对应 kind，不影响另一种。

## 6. 性能与内存

- 图片源数据已在 core 上限 64 张/64 MiB；UI 不再额外常驻整屏副本。
- 合成只处理 **dirty rows**；图片跨行时按 dirty row 裁剪。
- 图片 ID → 像素缓存只用于灯箱，点击时惰性上传。
- 大图缩放仍在 terminal 内 fit；灯箱 100% 原图。
- 每帧不重新解码 PNG/JPEG；Sixel/IIP 解码仍只在 core 一次。

## 7. 迁移策略

新增环境开关：

```text
TERVO_IMAGE_LAYER=v2   # 默认 v2；v1 用于回退对照
```

阶段：

1. core `Frame.images` 保持兼容；app 增加 framebuffer 合成路径。
2. v2 视觉层关闭独立 `PaneImage` Image，只保留点击命中。
3. v1 overlay 保留一个 release 周期，确认无回归后删除。
4. 旧 ECH 全量清图逻辑删除，改为区域 image damage。

## 8. 测试矩阵

### 单元/集成

- 图片 row/col/裁剪；负 row；右/下越界。
- 文本覆盖图片 cell；空白 cell 保留图片。
- ECH 只清对应区域，不清无关图片。
- 滚动 offset 改变 row；图片与文本一起移动。
- resize/字体改变后按 cell 重算。
- Sixel/IIP 独立开关/清除。
- 灯箱原图、Fit/100%/pixelated。

### 真实 TUI

新增 `scripts/e2e_dsh_images.sh`（隔离 Xvfb + 可写 DSH_HOME）：

- dsh-tui 启动
- 含图片的会话/设置页（或 fixture）
- 滚动上/下多个 offset，截图 + frame 断言
- 确认图片出现在文本锚点、无旧图残留、文字列不漂移
- ECH/清屏后旧图消失
- 2m soak

### 对照

- v1 vs v2 同输入截图：
  - 图片位置/可见性
  - 文本行/列
  - 内存
- 已有 Sixel/IIP/context/help e2e 全绿。

## 9. 验收

```text
滚动任意 offset：图片不丢、不漂、不覆盖错误文字
ECH/空格重绘：只影响覆盖区域
dsh 图片可见且清晰（真实 cell 像素）
resize/字号：图片重算
灯箱：原图、Fit、100%、pixelated
2m soak PASS
无 P0/SSH/插件语义变化
```

## 10. 已落地（2026-11-09）

```text
0b62596  framebuffer 合成第一刀：图片在背景之后、文本之前
c7f13ac  ECH 按当前光标行清图，不再任何 ECH 清全部图
3131ca5  XTWINOPS/PTY 像素尺寸 + 块元素几何绘制
57a30cc  ECH 清旧图（已被 v2 行级清理取代）
```

验证：

```text
BEFORE → Sixel → AFTER：AFTER 正确覆盖图片
offset=0/3/6/9/12/15：图片 row 与文本同步移动；offset=18 滚出视口
app 387 passed
core Sixel 8 passed
2m soak PASS
15m soak PASS（RSS +70.1 MiB，R²=0.30，线程/FD 稳定）
```

仍未完成：

```text
旧 Slint 点击模型/Image 缓存（仅点击，不再作为视觉层）
ECH 部分遮挡的像素级裁剪（目前整张 drop）
块字符画与 dsh 参考截图的最终对照
真实 dsh 含图会话多 offset 截图
```

## 11. 时间表（18:00 后）

```text
18:00–18:30  设计冻结 + dsh fixture
18:30–20:30  core image damage/clip + renderer 合成
20:30–21:30  点击命中/灯箱迁移 + v1 回退
21:30–22:30  dsh 滚动/ECH/resize e2e
22:30–23:00  2m/15m 内存门禁 + 文档
```

不回退到“再加一个 overlay 补丁”的路线。
