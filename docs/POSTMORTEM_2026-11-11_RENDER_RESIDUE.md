# 2026-11-11 渲染残留事故复盘

状态：已修复，当前 release `b5c8c6f0fcba` 干净；本文是排障纪律，不是事后解释。  
影响：dsh-tui 在 Tervo 中滚动/图片预览后，旧字符或图片像素残留在错误行；用户被迫反复重启验证，定位时间远超实际代码改动量。

## 1. 一句话结论

这不是一个 bug，而是**三个独立的上游缺陷叠加**，加上 Tervo 在字节过滤、Frame 缓存、图片几何三层都没有“真相校验”，导致我们从像素层反向猜了很多轮。

```text
IIP filter 在 Term 之前吞字节
图片 row<0 的目标 y 写穿到未标 damage 的行
ImageClearScanner 把已匹配的 2J 留在 tail，下一段文本再次清屏
```

`Frame`/damage/renderer 的大部分改动是**下游防御**；它们没有、也无法修复第一项。

## 2. 根因

### 2.1 IIP 过滤器静默丢字节（主因）

位置：`crates/core/src/iip.rs::IipFilter`，在字节交给 vte 之前。

两个缺陷：

```text
1. replay_ground() 的 ESC x fallback 只 pass.push(b)，没有把状态从
   Escape 复位到 Ground。
   之后正文里的 ] 被当作 OSC 前缀；紧跟 ESC 时，该字节被当作
   “空 OSC”丢弃。

2. 0x9D 既是 8-bit C1 OSC，又是 UTF-8 续字节（” = E2 80 9D）。
   旧代码把正文里的 0x9D 当 OSC 起点，后跟 ESC 时同样丢字节。
```

真实 dsh raw 797 KB：

```text
修复前：丢失 9 字节（1×0x9D + 8×]）
修复后：0
```

后果：

```text
丢 1 个字符 -> 该行后续整体左移
-> 行尾/空白处保留上一帧旧字符
-> 字体缩放触发 SIGWINCH 整屏重绘才消失
```

修复：commit `7e50800`，新增 IIP 最小序列/真实 PTY 管道回归。

### 2.2 图片 row<0 的目标 y 错位

位置：`crates/app/src/render.rs::draw_image_row`。

旧公式：

```rust
let dest_y0 = (top.max(0.0) + dy0 as f32).round() as usize;
```

图片顶部滚出视口（`row < 0`）时，`dy0` 已含 `-top` 的裁剪偏移；再加 `top.max(0)`，等于整张图又向下平移 `|top|` 像素，写进本行之外、不在 `row_span` 里的行。

该行没有 damage，partial 帧永远不会重绘它；字体缩放重建 renderer 后才消失。

修复：

```rust
let dest_y0 = row_top.max(top).max(0.0).round() as usize;
```

同时 `row_span` 对“完全在视口上方”的旧 placement 会先把负 `last` 转成巨大 `usize`，`target[first..end]` 直接 panic；修复为先判 `last <= 0 || first >= rows`。

证据：

```text
46bc: 图片消失，row 2 残留 54×22 色带
修复: rows 0+1 正确，row 2 干净
scripts/e2e_image_scroll_clip.sh: PASS=5 FAIL=0
```

### 2.3 ImageClearScanner 重复命中 2J

位置：`crates/core/src/pane.rs::ImageClearScanner`。

旧实现把上一段 32 字节 tail 拼进下一段扫描；已匹配的 `\x1b[2J` 仍在 tail 中，下一段普通文本会再次命中清屏，误清刚 place 的 Kitty 图片。

修复：只接受**结束于新字节**的序列；ECH 也只在结束于新字节时记录。

证据：单元测试 `image_clear_scanner_does_not_replay_matched_tail`。

### 2.4 Frame 局部路径的 zerowidth 残留

`Frame::put()` 在普通 cell 覆盖时不清理 `zerowidth` side table，旧组合 marks 会挂到新字符上。修复为覆盖时移除旧记录；`fill_frame` 对账也补上 zerowidth/URI 比较。

## 3. 为什么定位花了这么久

### 3.1 多个独立缺陷，症状相同

“旧字符留在屏幕上”可以由至少六层产生：

```text
filter 吞字节
Term 本身错
Frame 局部合并错
damage 漏标
renderer 写错行
Slint/纹理/窗口合成旧内容
```

每一层都能解释肉眼现象，但只有第一处不一致才是根因。

### 3.2 修错了层

实际顺序变成了：

```text
先改整屏重绘
-> 再改 Frame 整行刷新
-> 再改全 grid 对账
-> 再改 Slint 每帧新 buffer
```

这些都让某些情况“看起来好了”，但没有先证明：

```text
raw bytes -> filter 后 bytes -> Term -> Frame
```

最先错的是 IIP filter；下游无论怎么对账，都只是在同一份被污染的 Term 上自洽。

### 3.3 整屏重绘是止痛药

字体缩放/整屏重绘能清掉残影，这应该是**分层线索**，不是修复。  
我们一度把“全屏重绘后消失”当成 renderer/damage 的证据，实际上它只说明“增量缓存里有旧状态”，并不说明在哪一层。

### 3.4 缺少分层观测

当时没有：

```text
filter 输入/输出字节守恒
partial Frame vs force_full Term 自动对账
增量 framebuffer vs fresh full 像素对照
framebuffer vs 截图对照
行号/坐标 overlay
```

只能靠 xwd 截图和肉眼，且每次都要用户重启、滚动、再截图。

### 3.5 活窗口调试成本太高

```text
每次验证都重启用户窗口
多窗口/透明背景制造假线索
dsh 鼠标上报/图片预览让操作路径不稳定
故障现场无法保留
```

中途出现的“重复 Tervo 窗口 + background_opacity=0.54”确实是一个假线索；它会造成透出，但不是本次主因。正确做法是先记录窗口数量/Map State，再用单窗口隔离，而不是把假线索当结论。

### 3.6 测试金字塔断层

有 core 单测、有 renderer 单测，但缺：

```text
原始 PTY bytes -> Term -> Frame -> renderer -> framebuffer -> Slint
```

两端都绿，中间链路没人验证。  
IIP 的 9 字节丢失只有真实 raw 全链路测试才会暴露。

### 3.7 多 agent 启动得晚

最后并行三个 agent 才快速收敛：

```text
core-damage: 用 raw 证明 IIP 吞字节
scroll-render: 用 row<0 payload 复现图片写穿
image-scroll: 用 A/B 像素验证图片裁顶/清理
```

但前期主要是我按假设修，没有共享的层次证据板，也做了重复工作。

## 4. 架构/功能层次出了什么问题

### 4.1 Frame 同时是快照和增量缓存

`Frame` 由 UI 复用，`fill_frame` 只合并 damage，renderer 又按整行绘制。  
隐含契约是：

```text
damage 必须覆盖所有变化行/列
```

这个契约没有被强制、没有运行时校验、没有单一所有权说明。

### 4.2 字节过滤链没有守恒契约

IIP/Sixel/Kitty/OSC 是手写状态机，直接决定哪些字节进入 vte。  
没有“输入 = 透传 + 明确消费；静默丢弃必须为 0”的不变量，也没有字段/计数器暴露丢失。

### 4.3 图片几何有多份实现

```text
core: row / row_id / source_rect / cells
renderer: dest_y / display_w/h / crop
app: hit geometry
Slint: TouchArea
```

负 row、裁顶、crop、cell 尺寸各层各算一遍。  
两个图片 bug 都出在几何重复计算，而不是业务逻辑。

### 4.4 两套渲染架构并存

legacy `Frame + CPU Renderer` 与 `Scene/Renderer/WGPU` 骨架同时存在；图片、RowId、diagnostics 都只迁移了一半。  
`docs/ARCHITECTURE.md` 原则写“图片不写文本 framebuffer”，legacy CPU 路径实际上仍把图片合成进 framebuffer；语义边界不一致，排障时容易误判“图片层没有第二条路径”。

### 4.5 观测系统上线晚，且没接主路径

`crates/diagnostics` 有指标/场景差分，但 live metrics/trace 没有接 app 渲染循环；真正排障时仍靠截图。

## 5. 以后强制怎么查

### 5.1 分层阶梯，先找第一处分歧

任何“画面不对”先按顺序查，不要从像素猜：

```text
1. raw bytes vs filter 输出的字节数/内容
2. filter 输出 vs Term 文本
3. Term vs partial Frame（逐 cell/zerowidth/URI/row_id）
4. partial Frame + damage vs force_full Frame
5. 增量 framebuffer vs fresh full framebuffer（逐像素）
6. framebuffer vs xwd/take_snapshot
```

一旦某层出现第一处分歧，停在该层，不要再改下游。

### 5.2 先写失败测试

硬规则：

```text
没有失败测试，不允许写修复。
```

最小测试来源：

```text
用户 raw PTY 片段
最小转义序列
最小 Frame + damage 序列
图片 row=-1 / row=-5 / 裁顶 payload
```

### 5.3 禁止下游兜底

```text
禁止用“每帧整屏重绘”当根因修复
禁止用“每帧新 buffer”掩盖缓存问题
禁止在没有字节/Frame 证据时改 renderer/Slint
```

整屏重绘只能作为 debug 开关/分层诊断，不能默认开启。

### 5.4 故障现场协议

以后报告必须保留：

```text
raw PTY
TERVO_FRAME_DUMP
截图（带行号）
release commit
窗口数量 + Map State
dsh/session id
```

不要用“再重启一次看看”代替证据。

### 5.5 agent 并行协议

```text
core-damage: raw bytes / Term / Frame
scroll-render: renderer / framebuffer
image-scroll: 图片几何 / 图片生命周期
presentation: Slint / 窗口合成
```

每个 agent 必须先交：

```text
失败测试
复现命令
第一处分歧
证据路径
```

一个 integrator 统一合，避免各改一层后互相抵消。

## 6. 已落地的防复发项

- `crates/core/tests/frame_consistency.rs`：
  partial Frame vs force_full Term，真实 raw、滚动/CUP/alt screen/宽字符/组合字符/OSC8。
- `crates/app/src/image_layer_diff_tests.rs`：
  图片裁顶/负 row/滚动/删除，增量 vs 整屏逐像素。
- `crates/app/tests/slint_image_presentation.rs`：
  MinimalSoftwareWindow + SwappedBuffers，排除 Slint 纹理缓存。
- `scripts/e2e_image_scroll_clip.sh`：
  真实 Xvfb + Kitty，裁顶图片 5/5。
- `TERVO_FRAME_DUMP=/tmp/tervo-frame.txt`：
  每帧 Frame 文本落盘；只在显式设置时启用。
- `TERVO_LINE_NUMBERS=1`：
  终端行号 overlay，仅调试；会盖住最左侧约 3 个字符。
- `b5c8c6f`：
  修复 2J tail 重放；回退没有证据支持的每帧整屏 buffer copy。

## 7. 待办（按优先级）

### P0

- [ ] `FilterAudit`：每个 filter 的 input/passed/consumed/dropped 计数；debug 下 `dropped != 0` 直接报错。
- [ ] `tervo diag trace-layer`：一条命令输出上面 6 层的首个分歧和最小字节区间。
- [ ] 把 frame_consistency / image_diff / slint presentation / image-scroll e2e 接进 CI gate。
- [ ] 图片几何收敛为单一 `PlacementRect`，renderer/app/Slint 不再重复算 dest_y/crop/size。

### P1

- [ ] Frame partial 契约显式化：`Frame` 增加“上一帧基线”语义或拆成 `TermSnapshot` + `DamageCache`。
- [ ] live diagnostics 接主渲染循环：frame/row/image/damage/stale 计数进 `tervo.diag.v1`。
- [ ] dsh 滚动/预览自动复现脚本进入标准回归，不依赖用户窗口。
- [ ] legacy CPU 与 Scene 路径二选一/明确 adapter，禁止两套图片合成语义并存。

### P2

- [ ] 逐步迁移到 backend-independent Scene/Renderer，WGPU 只替换后端。
- [ ] 15m/30m soak 按规模进入发布门禁。
- [ ] 所有 debug overlay/环境变量在发布前确认默认关闭。

## 8. 本次事故的检查清单

修复前：

```text
[ ] 有可重放的 raw / 最小序列
[ ] 有第一处分歧的层次证据
[ ] 有失败测试
[ ] 明确不会靠整屏重绘/新 buffer 掩盖
```

修复后：

```text
[ ] 测试修复前失败、修复后通过
[ ] core + app + 对应 e2e
[ ] 没有新增默认兜底
[ ] 图片/过滤路径有逐像素/逐字节对照
[ ] 文档更新
```

## 9. 相关提交与文档

```text
7e50800  IIP filter 状态泄漏吞 ]/UTF-8 续字节
7cf220b  裁顶图片目标行几何 + row_span 防空
55c8cc4  图片 row<0 dest_y + zerowidth 清理
b5c8c6f  2J/ECH 只命中新字节；回退每帧 buffer copy
328003b  merge：IIP + 图片 + 一致性测试
```

相关文档：

- [`ARCHITECTURE.md`](ARCHITECTURE.md)
- [`IMAGE_ENGINE_CONTRACT.md`](IMAGE_ENGINE_CONTRACT.md)
- [`DIAGNOSTICS.md`](DIAGNOSTICS.md)
- [`DSH_IMAGE_PREVIEW_E2E.md`](DSH_IMAGE_PREVIEW_E2E.md)
