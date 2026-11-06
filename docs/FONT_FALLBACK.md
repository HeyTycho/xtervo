# 字体回退、彩色 emoji 与宽字符渲染

> 状态（2026-11-06）：主等宽字体缺字时按系统字体链回退；中文、emoji、Nerd Font
> 图标、组合音标都有明确行为；字体链全部按需解析，纯 ASCII 会话不增加启动/RSS。
> **彩色 emoji 默认开启**：Noto Color Emoji 等 CBDT/CBLC/sbix 位图字体由
> `ttf-parser` 读位图、`png` 解码，缩放后缓存为 RGBA，绘制在 emoji 原有的
> 1/2 格网格内。找不到彩色字体、COLR/SVG 字体或解码失败时，保持单色回退与
> 明确占位框，不崩溃、不画 tofu。

## 1. 字体链

内置候选（按用途分组；只有相关组会被真正解析）：

| 组 | 候选（按顺序尝试） | 用途 |
|---|---|---|
| CJK | WQY Micro Hei → Droid Sans Fallback → Noto Sans CJK SC(face 7) → WQY Zen Hei Mono | 中文/日文/韩文/全角 |
| SYMBOL | Noto Sans Symbols 2 → DejaVu Sans | 数学/箭头/框线/单色 emoji（😀 ❤ ✔ 等） |
| EMOJI | Symbola（Ubuntu fonts-symbola）→ Noto Emoji（单色） | 覆盖更全的单色 emoji（有则用） |
| ICON | `SymbolsNerdFontMono-Regular.ttf` 常见系统路径 | Nerd Font / Powerline 私用区图标 |
| COLOR | Noto Color Emoji（Debian/Ubuntu/Fedora 等常见路径） | 彩色位图 emoji，独立管线；不参与 fontdue 轮廓链 |

不同字符只走相关组：

- emoji：**COLOR 彩色位图 → SYMBOL → EMOJI 单色**；彩色字体缺失/不覆盖时自动
  回落单色。彩色管线只探测 COLOR 组与 `TERVO_FALLBACK_FONTS`，**不会**为了 emoji
  解析 18 MB 的 Noto CJK；
- 中文：CJK → SYMBOL → EMOJI（不碰 ICON/COLOR）；
- Nerd/私用区图标：ICON → SYMBOL；没有 Nerd 字体时直接占位，不去解析 CJK；
- 其它缺字：SYMBOL → CJK → EMOJI。

所有候选都是**惰性**的：`FontSet::load` 只解析主字体；回退链的列表在第一次缺字时
建立（不解析）；每个字体在第一次被这个字符需要时才读文件/解析。解析失败结果也会
缓存，后续同字符不会反复重试。彩色位图字体的文件只读一次、`ttf-parser` 表只解析
一次，失败（文件不存在/不是彩色字体）同样缓存。

### 环境变量

| 变量 | 作用 |
|---|---|
| `TERVO_FALLBACK_FONTS` | `:` 分隔的额外字体路径（可写 `/path/font.ttc#7`），优先于内置 SYMBOL 组。典型用途：指向用户目录里的 Nerd Font 或自带彩色 emoji 字体 |
| `TERVO_COLOR_EMOJI=0` | 关闭彩色 emoji（`0`/`off`/`false`/`no`），退回本轮之前的单色回退/占位行为；默认开启 |
| `TERVO_FONTLOG=1` | 打印每个回退字体的解析耗时/失败，以及彩色字体探测/解码信息 |

示例：

```bash
TERVO_FALLBACK_FONTS=$HOME/.local/share/fonts/SymbolsNerdFontMono-Regular.ttf \
TERVO_FONTLOG=1 tervo

# 指定用户目录里的彩色 emoji 字体（会被当作彩色候选探测）
TERVO_FALLBACK_FONTS=$HOME/.local/share/fonts/NotoColorEmoji.ttf \
  scripts/font_glyph_shot.sh .logs/font-emoji/custom-color
```

> 静态路径只覆盖 Debian/Ubuntu/Fedora 常见位置；其它发行版直接用环境变量指定。
> 没有做 fontconfig 查询（`fc-match`），避免渲染线程启动子进程。

## 2. 网格宽度规则（重要）

宽度由终端核心（alacritty 0.26 + `unicode-width`）统一决定，渲染层**不能**改网格：

- 宽字符 / emoji 按 **2 格**：首格 `WIDE_CHAR`，第二格 `WIDE_CHAR_SPACER`；
- 组合音标、ZWJ（U+200D）、变体选择符（VS15/16）宽度为 0，挂在**前一个格子**的
  `Cell::zerowidth()` 上，不额外占格；它们不产生 alacritty damage，因此
  `pane.rs` 检测到零宽码位时会强制下一帧整屏重取并扫描零宽表；
- `e + U+0301` 只占 1 格；`❤ + VS16` 只占 1 格（`❤` 本身是窄字符）；
- `👨‍👩‍👧` 在 alacritty 的网格里是 **3 个宽字符 = 6 格**（每个 emoji 2 格，两个 ZWJ
  零宽挂在各自前一个 emoji 上）。彩色渲染按码位逐个组件绘制，不合并成单簇。

### 渲染侧保证“不撑破网格”

- 彩色位图先等比缩放到**该字符占的格子**内：宽 emoji 最大 `2*cell_w`，窄 emoji
  （`❤+VS16`）最大 `1*cell_w`，高度不超过 `cell_h`；水平/垂直居中；
- `blit_color_buf` 横向严格裁剪到 `[格左, 格左 + span*cell_w)`，纵向裁剪到本行；
  即使字体位图异常、DPR/尺寸变化或帧数据损坏，也不会画进相邻格/相邻行；
- 单色字形步进超过 `span * cell_w` 时按比例缩小字号重新栅格化；`blit` 同样横向
  裁剪到 `[格左, 格左 + span*cell_w)`；
- 组合音标没有 GPOS mark attachment：fontdue 不提供锚点，渲染层把标记位图的
  水平中心对齐基字形的墨迹中心；ZWJ/VS 等控制符跳过不画。

## 3. 彩色 emoji 管线

1. 仅对 emoji 码位（`placeholder_kind == Emoji`）调用 `GlyphCache::get_color`；
   ASCII/中文/私用区图标完全不触碰彩色候选；
2. 按“`TERVO_FALLBACK_FONTS` 追加字体 → 内置 COLOR 组”顺序，用
   `owned_ttf_parser`（持有文件所有权，内部是 `ttf-parser`）查 cmap 与
   `glyph_raster_image`；支持 CBDT/CBLC、EBDT/EBLC、sbix 的 PNG 位图，
   以及 CBDT/EBDT 的预乘 BGRA32；
3. PNG 由 `png` 解码（统一到 8-bit），预乘 BGRA 反预乘为直通 alpha RGBA；
4. 按面积平均缩小到网格内（预乘 alpha 累加，避免透明边缘黑边）；
5. 结果按 `(字符, 样式, 占格数)` 缓存在 `GlyphCache.color_map`，`Rc<Vec<u32>>`
   让缓存命中只复制指针；命中不读字体、不解析字体、不重复解 PNG/缩放；
6. 绘制用独立 RGBA 混合（不套文字 coverage 的 gamma，不乘终端前景色）。

彩色 emoji 保留字体自带颜色，**忽略终端前景色与 INVERSE**（和主流终端一致）；
块状光标下彩色 emoji 仍按彩色绘制，不会被压成占位框。

## 4. 缺字与占位

| 情况 | 行为 |
|---|---|
| 主字体缺字、回退链有轮廓 | 正常绘制（如中文、组合音标、单色符号） |
| emoji 有彩色位图 | 彩色绘制（😀🚀❤👨‍👩‍👧 等）；宽 emoji 占两格，窄 emoji 缩进一格 |
| emoji 只有 COLR/SVG/JPEG sbix（本轮不支持） | 回落 SYMBOL/EMOJI 单色轮廓；仍没有则占位框 |
| 彩色字体存在但不覆盖该码位 | 回落单色轮廓；仍没有则占位框 |
| Nerd/私用区图标完全缺失 | 明确占位框：1px 空框 + 中心竖条 |
| 普通缺字（生僻 CJK、未分配码位） | 保持留空（避免满屏空框） |
| 组合标记缺字 | 跳过该标记，不影响基字形 |

占位框是**主动标记**，与字体 `.notdef` tofu 不同：它严格画在字符自己的格子里，
可以通过“空框/中心标记”一眼看出是后端不支持而不是字体没装。彩色字体命中后
会停止继续找单色轮廓，不会为了一个 emoji 去解析整条 CJK 链。

## 5. 测试与验收

```bash
# 单元测试（宽字符/组合/emoji 的 cell 宽度、截断、占位、彩色解码/缓存/裁剪）
cargo test -p tervo-core
cargo test -p tervo-app --bin tervo

# GUI 截图（Xvfb :121，脚本只 kill 自己启动的进程；不碰 :1/Xephyr）
cargo build -p tervo-app --bin tervo
FONT_SHOT_DISPLAY=:121 scripts/font_glyph_shot.sh .logs/font-emoji/color-after
TERVO_COLOR_EMOJI=0 FONT_SHOT_DISPLAY=:121 \
  scripts/font_glyph_shot.sh .logs/font-emoji/color-off
```

已验收截图（本机 2026-11-06，Xvfb :121，font-size 16）：

- `color-after.png`：默认开启——😀🚀❤️✔️👍⚡🎉 为彩色，👨‍👩‍👧 / 👩‍💻 / 🏳️‍🌈
  按 3 个/2 个组件分别彩色绘制，仍占 6/4 格；CJK、组合音标、框线不受影响；
- `color-off.png`：`TERVO_COLOR_EMOJI=0`——退回单色轮廓/占位框，用于对照与排障；
- 旧一轮字体回退截图 `before.png` / `after-default.png` / `after-nerd.png` 仍在
  `.logs/font-emoji/`，记录彩色 emoji 之前的占位行为。

测试覆盖（本轮新增）：

- `color_emoji_decodes_cbdt_and_caches_after_first_time`：CBDT 解码、彩色像素、
  两格/行高尺寸、缓存命中不重复解码（`Rc::ptr_eq`）、emoji 不解析 CJK 字体；
- `color_emoji_narrow_heart_fits_one_cell`：`❤` 单格缩放；
- `color_emoji_is_not_probed_for_ascii_or_cjk`：ASCII/中文不建立彩色链；
- `color_font_loader_rejects_non_color_and_missing_files`：非彩色/缺失字体安全跳过；
- `color_raster_decode_handles_premultiplied_bgra`：预乘 BGRA 反预乘、损坏 PNG；
- `color_scaling_fits_grid_and_keeps_color`：缩放尺寸/颜色、0 尺寸/全透明不 panic；
- `color_emoji_draws_rgb_inside_wide_cell`、`vs16_heart_color_stays_in_one_cell`、
  `zwj_family_color_components_stay_inside_grid`：真实渲染的彩色像素与网格裁剪。

## 6. 性能

- ASCII：主字体命中，不建立/解析回退链；
- 中文：第一次缺字只解析 CJK 组第一个可用字体（本机 wqy-microhei）；
- emoji：彩色字体文件读取 + `ttf-parser` 解析一次（本机 NotoColorEmoji 约 4.8ms
  debug），每个新 emoji 首次出现时解 PNG + 面积缩放一次，之后 `color_map` 命中
  只做 RGBA 混合；**不会**为 emoji 解析 18 MB 的 Noto CJK；`TERVO_FONTLOG=1`
  的日志里可看到彩色候选探测与 CJK 加载相互独立；
- 单色 emoji 仍走原轮廓链；整屏 pull 默认保持无分支热路径；只有“帧里已有零宽
  记录”或“本批输入含零宽码位”时才逐格读 `Cell::zerowidth()`。旧一轮 A/B（debug
  bench，波动较大）：dirty 单行约 21µs → 18µs，整屏约 101µs → 85µs；解析
  10MB 约 49–51ms，与基线持平。

## 7. 限制

- **COLR/CPAL 与 SVG 彩色字形本轮不绘制**。`ttf-parser` 能识别 COLR 字体，
  但本实现没有颜色图层光栅化器；此类字体（如 Twemoji Mozilla）会回落到单色
  轮廓/占位框，而不是彩色。CBDT/CBLC、EBDT/EBLC、sbix 的 **PNG** 位图可用；
  sbix 里的 JPEG/TIFF 图像不支持。
- ZWJ 家族 emoji 不会像 kitty/wezterm 那样合并成单个 2 格字形：alacritty 0.26
  的网格按码位计宽，本仓库只做渲染层逐码位彩色绘制，不修改终端宽度语义。若两个
  组件共用同一 ZWJ 序列的单字形（如家庭/彩虹旗），当前不会使用该合字位图，而是
  分别绘制各组件并顺序覆盖；肤色/旗帜等由网格决定是独立码位还是零宽标记。
- 彩色位图按**逻辑字号**解码/缩放，不按 DPR 超采样；HiDPI 下由 Slint/合成器放大，
  与现有文字光栅化策略一致。
- 彩色 emoji 忽略终端前景色/INVERSE，这是有意的显示行为；如需要主题化，只能关闭
  彩色 emoji。
- 组合音标用“居中到基字形墨迹中心”的近似，没有 GPOS 锚点；极端字体/多标记叠加
  可能与排版引擎有 1–2px 差异。
- 内置路径是 Debian/Ubuntu/Fedora 常见位置；其它发行版请用 `TERVO_FALLBACK_FONTS`。
- 回退字体按需解析仍可能占用较大 RSS（wqy-microhei 约 170 MB、Noto CJK 更大）；
  只在真正遇到缺字时发生。彩色 emoji 位图缓存在内存（约几十字节/像素），
  大量不同 emoji 会线性增长。
