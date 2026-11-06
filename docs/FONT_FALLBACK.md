# 字体回退、emoji 与宽字符渲染

> 状态（2026-11-06）：主等宽字体缺字时按系统字体链回退；中文、emoji、Nerd Font
> 图标、组合音标都有明确行为；字体链全部按需解析，纯 ASCII 会话不增加启动/RSS。
> 彩色位图 emoji（Noto Color Emoji）**不能**用 fontdue 光栅化，按“明确占位框”
> 处理，不崩溃、不画 tofu。

## 1. 字体链

内置候选（按用途分组；只有相关组会被真正解析）：

| 组 | 候选（按顺序尝试） | 用途 |
|---|---|---|
| CJK | WQY Micro Hei → Droid Sans Fallback → Noto Sans CJK SC(face 7) → WQY Zen Hei Mono | 中文/日文/韩文/全角 |
| SYMBOL | Noto Sans Symbols 2 → DejaVu Sans | 数学/箭头/框线/单色 emoji（😀 ❤ ✔ 等） |
| EMOJI | Symbola（Ubuntu fonts-symbola）→ Noto Emoji（单色） | 覆盖更全的单色 emoji（有则用） |
| ICON | `SymbolsNerdFontMono-Regular.ttf` 常见系统路径 | Nerd Font / Powerline 私用区图标 |
| COLOR | Noto Color Emoji | 只用于识别“系统确实有该 emoji”；fontdue 取不到轮廓 |

不同字符只走相关组：

- 中文：CJK → SYMBOL → EMOJI → COLOR（不会为了中文解析 Nerd Font）
- emoji：SYMBOL → EMOJI → COLOR（**不会**为了 emoji 解析 18 MB 的 Noto CJK）
- Nerd/私用区图标：ICON → SYMBOL；没有 Nerd 字体时直接占位，不去解析 CJK
- 其它缺字：SYMBOL → CJK → EMOJI → COLOR

所有候选都是**惰性**的：`FontSet::load` 只解析主字体；回退链的列表在第一次缺字时
建立（不解析）；每个字体在第一次被这个字符需要时才读文件/解析。解析失败结果也会
缓存，后续同字符不会反复重试。

### 环境变量

| 变量 | 作用 |
|---|---|
| `TERVO_FALLBACK_FONTS` | `:` 分隔的额外字体路径（可写 `/path/font.ttc#7`），优先于内置 SYMBOL 组。典型用途：指向用户目录里的 Nerd Font |
| `TERVO_FONTLOG=1` | 打印每个回退字体的解析耗时/失败，便于现场排查“为什么这个字是空的” |

示例：

```bash
TERVO_FALLBACK_FONTS=$HOME/.local/share/fonts/SymbolsNerdFontMono-Regular.ttf \
TERVO_FONTLOG=1 tervo
```

> 静态路径只覆盖 Debian/Ubuntu 常见位置；其它发行版直接用环境变量指定。
> 没有做 fontconfig 查询（`fc-match`），避免渲染线程启动子进程。

## 2. 网格宽度规则（重要）

宽度由终端核心（alacritty 0.26 + `unicode-width`）统一决定，渲染层**不能**改网格：

- 宽字符 / emoji 按 **2 格**：首格 `WIDE_CHAR`，第二格 `WIDE_CHAR_SPACER`；
- 组合音标、ZWJ（U+200D）、变体选择符（VS15/16）宽度为 0，挂在**前一个格子**的
  `Cell::zerowidth()` 上，不额外占格；它们不产生 alacritty damage，因此
  `pane.rs` 检测到零宽码位时会强制下一帧整屏重取并扫描零宽表；
- `e + U+0301` 只占 1 格；`❤ + VS16` 只占 1 格（`❤` 本身是窄字符）；
- `👨‍👩‍👧` 在 alacritty 的网格里是 **3 个宽字符 = 6 格**（每个 emoji 2 格，两个 ZWJ
  零宽挂在各自前一个 emoji 上）。渲染层按这个网格逐字回退/占位，不会为 ZWJ
  额外多算格子。

### 渲染侧保证“不撑破网格”

- 字形步进超过 `span * cell_w` 时按比例缩小字号重新栅格化。典型：`❤️` 只有 1 格，
  但 DejaVu 的 emoji 轮廓原生步进约 12px（单格约 9px），会缩到单格内；
- `blit` 横向严格裁剪到 `[格左, 格左 + span*cell_w)`；即使帧数据损坏/网格把 emoji
  标成 1 格，也不会画进相邻格；
- 组合音标没有 GPOS mark attachment：fontdue 不提供锚点，渲染层把标记位图的
  水平中心对齐基字形的墨迹中心；ZWJ/VS 等控制符跳过不画。

## 3. 缺字与占位

| 情况 | 行为 |
|---|---|
| 主字体缺字、回退链有轮廓 | 正常绘制（如中文、😀、❤、✔、Nerd 图标配好字体时） |
| 彩色位图 emoji（只有 CBDT/CBLC/COLR，无轮廓） | 明确占位框：1px 空框 + 中心点；宽 emoji 占两格 |
| Nerd/私用区图标完全缺失 | 明确占位框：1px 空框 + 中心竖条 |
| 普通缺字（生僻 CJK、未分配码位） | 保持留空（避免满屏空框） |
| 组合标记缺字 | 跳过该标记，不影响基字形 |

占位框是**主动标记**，与字体 `.notdef` tofu 不同：它严格画在字符自己的格子里，
可以通过“空框/中心标记”一眼看出是后端不支持而不是字体没装。彩色 emoji 字体命中后
会停止继续扫描（不会为了一个 emoji 去解析整条 CJK 链），直接交给占位框。

## 4. 测试与验收

```bash
# 单元测试（宽字符/组合/emoji 的 cell 宽度、截断、占位、裁剪）
cargo test -p tervo-core
cargo test -p tervo-app --bin tervo

# GUI 截图（Xvfb :116，脚本只 kill 自己启动的进程；不碰 :1/Xephyr）
cargo build -p tervo-app --bin tervo
scripts/font_glyph_shot.sh .logs/font-emoji/after-default
TERVO_FALLBACK_FONTS=/path/SymbolsNerdFontMono-Regular.ttf \
  scripts/font_glyph_shot.sh .logs/font-emoji/after-nerd
```

已验收截图（本机 2026-11-06，均为 `.logs/font-emoji/`）：

- `before.png`：改动前——emoji 大部分空白、组合音标丢失、Nerd 图标空白；
- `after-default.png`：改动后——😀/❤/✔ 单色回退、组合音标叠加、彩色 emoji 与
  缺失 Nerd 图标显示占位框；
- `after-nerd.png`：同一画面，额外设置 `TERVO_FALLBACK_FONTS` 指向
  `SymbolsNerdFontMono-Regular.ttf`，Nerd 图标正常显示。

测试覆盖：

- core：`combining_marks_stay_in_one_cell`、`emoji_and_variation_selectors_use_grid_width`、
  `zwj_sequence_does_not_add_cells_for_joiners`、`wide_char_at_last_column_wraps_without_half_cell`、
  后到的组合音标刷新；
- app：CJK/单色 emoji/窄 emoji 缩放、占位框、组合音标叠加、ZWJ 家族不越界、
  1 格 emoji 不撑破相邻格。

## 5. 性能

- ASCII：主字体命中，不建立/解析回退链；
- 中文：第一次缺字只解析 CJK 组第一个可用字体（本机 wqy-microhei）；
- emoji：先解析 DejaVu Sans（约 0.1s debug），不会解析 Noto CJK；
- 整屏 pull：默认保持无分支热路径；只有“帧里已有零宽记录”或“本批输入含零宽码位”
  时才逐格读 `Cell::zerowidth()`。200x50 同机 A/B（debug bench，波动较大）：
  dirty 单行约 21µs → 18µs，整屏约 101µs → 85µs；解析 10MB 约 49–51ms，与基线持平。

## 6. 限制

- **fontdue 不支持彩色位图/COLR/CPAL/SVG emoji**。Noto Color Emoji（CBDT/CBLC）和
  Twemoji Mozilla（COLR）只能识别覆盖，不能绘制；这类 emoji 显示为占位框。要显示
  彩色 emoji 需要引入 PNG 位图/CBDT 解码 + RGBA 图集，属于后续工作。
- ZWJ 家族 emoji 不会像 kitty/wezterm 那样合并成单个 2 格字形：alacritty 0.26 的
  网格按码位计宽，本仓库只做渲染层回退，不修改终端宽度语义。若要单簇渲染，需要
  在 VT 输入层做 grapheme cluster 化（超出本轮范围）。
- 组合音标用“居中到基字形墨迹中心”的近似，没有 GPOS 锚点；极端字体/多标记叠加
  可能与排版引擎有 1–2px 差异。
- 内置路径是 Debian/Ubuntu 常见位置；其它发行版请用 `TERVO_FALLBACK_FONTS`。
- 回退字体按需解析仍可能占用较大 RSS（wqy-microhei 约 170 MB、Noto CJK 更大）；
  只在真正遇到缺字时发生。
