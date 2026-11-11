# 图片字节过滤链守恒审计（2026-11-11）

状态：**审计测试 + 3 个 Sixel filter 修复均已完成**。

- `10df3dd` test(core)：审计测试、真实 raw fixtures、只读访问器；7 个已知
  失败测试 `#[ignore]` 隔离，证据见
  [`docs/evidence/filter-audit-2026-11-11/`](evidence/filter-audit-2026-11-11/pre-fix-failing-tests.txt)。
- `be14f08` fix(core)：修复 F1/F2/F3，全部测试去掉 `#[ignore]` 并通过。

## 0. 结论

- 三张 filter（Sixel DCS / IIP `OSC 1337;File` / Kitty APC）的
  “输入 = 透传 + 显式消费 + 暂存”守恒契约现在是**可测试、可复现**的。
- IIP 与 Kitty 的 chunk 边界处理经本套测试验证无明显静默丢字节。
- **Sixel filter 有 3 个独立缺陷**（2 个 High），会让普通 UTF-8 文本暂存、
  把原始 ESC 重复交给 vte、并把后续 Sixel 漏给 vte。与 2026-11-11 的
  “IIP filter 吞字节”是同一层、同一类问题；IIP 的兄弟缺陷（0x9D）已由
  `7e50800` 修复，**Sixel 的同类缺陷当时没有修**。本次已修复并加回归。

## 1. 方法

`crates/core/tests/filter_conservation.rs` 把 `pane.rs::feed` 的三级分派
（Sixel → IIP → Kitty APC → vte）逐行镜像成可独立驱动的管线，复用生产
filter 类型，不复制图片解码逻辑。断言分四类：

1. **逐字节守恒**：纯文本输入在单 chunk / 每个切点 / 逐字节 drip / 随机
   多切点下，`pass == input`。
2. **只吞自己**：图片序列前后文本完整、顺序不变；`Image`/`Command` 计数
   正确；`input == pass + swallowed(sequence) + held`。
3. **恢复性**：截断/非法序列遇到 ESC、CAN/SUB、下一个合法序列后，后文
   仍原样到达 vte。
4. **chunk 不变性**：同一输入任意切分，最终 `(pass, images, commands)`
   完全一致（专门覆盖 ESC/OSC/0x9D/UTF-8 续字节中间切断）。

### 1.1 三类 filter 的字节关系（审计表）

| filter | 输入 | 放行（Pass） | 显式消费 | 跨 chunk 暂存 | 中止/截断 |
| --- | --- | --- | --- | --- | --- |
| `SixelFilter` | 任意 PTY 字节 | 非 DCS/Sixel 字节；非 Sixel DCS 头+载荷原样 | 真 Sixel DCS（`ESC P ... q ` / C1 `0x90`）头+payload；解码失败也吞 | `pending`（`ESC`/DCS 头） | CAN/SUB 丢图回 Ground；BEL/ST 结束；超限吞到终止符 |
| `IipFilter` | Sixel 放行字节 | `OSC 1337;File` 之外全部；前缀不匹配时 pending+prefix 原样回放 | 完整 `ESC ] 1337;File=...`（body + 终止符）；非法 base64/超限只丢图不丢序列 | `pending`（`ESC`/C1 OSC）+ `prefix`（已匹配的 `1337;File=`） | BEL/C1 ST/`ESC \` 结束；CAN/SUB 丢图；下一 ESC 中止 |
| `KittyParser` | IIP 放行字节 | 非 Kitty APC 全部（含非 Kitty APC 原样） | `ESC _ G ...` APC（解析失败也吞）；分片 `m=1` 跨 chunk 保留 | `scan` 里的 `ESC`/`ESC _` + `apc` payload | `ESC \`/C1 ST 结束；CAN/SUB 中止；`MAX_APC_BYTES` 溢出丢弃 |

组合关系（`pane.rs::feed`，由防漂移测试锁定）：

```text
PTY bytes
  -> SixelFilter.feed            None => 原始 bytes 直接进下一级
       Pass(chunk) -> IipFilter.feed   None => 原始 chunk 进下一级
                        Pass(chunk) -> KittyParser.feed（无 ESC 且无暂存时直接透传）
                                         Pass(bytes) -> vte
       Image -> add_sixel_image   Image -> add_iip_image   Command -> kitty 处理
```

守恒定义：对任意输入 `I`、任一 filter 的一次/多次 feed，

```text
len(I) == len(所有 Pass) + len(显式消费的完整图片序列) + held
```

`held` 由只读访问器 `held_bytes()` 暴露；不满足即存在静默丢弃/重复。
本次没有引入内部计数器，而是用“外部账本 + 三个只读访问器”实现同样的
可检测性，避免在热路径加字段/分支；如需线上自检，可后续把账本做成
`#[cfg(debug_assertions)]` 计数器。

防漂移：`pipeline_mirror_matches_pane_source` 读取 `pane.rs` 源码，断言
`feed → sixel → feed_iip → process_vte_chunk → kitty.feed` 的调用顺序仍成立；
生产分派改动会让审计测试失败并提示同步。

审计访问器（只读，不参与生产逻辑）：
`SixelFilter::held_bytes`、`IipFilter::held_bytes`、
`KittyParser::held_bytes`，用于守恒账本里的“暂存”项。

## 2. 覆盖矩阵

| 需求 | 测试 |
| --- | --- |
| 纯文本 → 输出逐字节等于输入（多 chunk） | `plain_text_is_byte_identical_for_every_chunking` |
| 非法/截断 IIP → 后文不丢、不挂起 | `truncated_iip_is_aborted_by_next_escape_sequence`、`..._by_can_and_following_text_survives` |
| 非法/截断 Sixel → 后文不丢、不挂起 | `truncated_sixel_is_aborted_by_next_escape_sequence`、`c1_st_terminators_are_preserved_for_vte` |
| 图片序列只吞自己，前后文本完整 | `image_sequence_swallows_only_itself`、`multiple_images_keep_text_and_order` |
| 随机 chunk 边界（ESC/OSC/0x9D/UTF-8） | `esc_and_osc_split_at_every_boundary`、`random_chunk_boundaries_mixed_corpus_are_chunking_invariant` |
| 真实 dsh raw 防回归 | `real_dsh_cjk_text_slice_is_byte_identical`、`real_dsh_kitty_chunked_is_conserved` |
| Kitty APC 非法/截断 | `malformed_kitty_apc_does_not_swallow_following_text`、`unterminated_kitty_apc_is_bounded_and_aborts_on_can` |
| 管线镜像防漂移 | `pipeline_mirror_matches_pane_source` |

真实 raw fixture 来源与再生成命令见
[`crates/core/tests/fixtures/README.md`](../crates/core/tests/fixtures/README.md)。

## 3. 发现（按严重度）

复现全部失败：`cargo test -p tervo-core --test filter_conservation -- --ignored`，
完整输出见
[`docs/evidence/filter-audit-2026-11-11/pre-fix-failing-tests.txt`](evidence/filter-audit-2026-11-11/pre-fix-failing-tests.txt)
（7 failed）。

### F1 [High] Sixel filter 不识别 UTF-8 续字节，普通文本会触发 C1 状态机

`0x90/0x98/0x9B/0x9D/0x9E/0x9F` 既是 8-bit C1 控制符，也是常见 UTF-8
字符的续字节。`sixel.rs` 的 `Ground` 分支只看单字节：

- `0x90` 进 `DcsHeader`，后续文本全部进 `pending`（`held=28` 的
  Cyrillic 用例），在遇到 ASCII final byte 前**不会交给 vte**：画面缺字/
  延迟；若此时第一个 final byte 是 `q`，会被误判成 Sixel 起点，
  `pending` 被清空（**文本丢失**）并继续吞后续字节。
- `0x9D/0x9B/0x9F/0x9E/0x98` 分别进 OSC/CSI/String，状态机停在非 Ground。
- 后果之一：紧跟其后的真 Sixel 被当正文放给 vte。BEL 终止的 DCS 会让
  vte 吞掉后续文本——正是 2026-11-11 事故同层的“静默丢字节”。

真实 dsh raw 证据：fixture 文本切片里 3000 字节命中 10 个此类字符，包括
`┐`(E2 94 90)、`某`(E6 9F 90)、`面`(E9 9D A2)、`是`(E6 98 AF)、
`域`(E5 9F 9F)。IIP filter 在 `7e50800` 已针对 0x9D 做了 UTF-8 窗口判定，
Sixel 未同步。

测试：`plain_text_is_byte_identical_for_every_chunking`、
`utf8_c1_continuation_then_sixel_is_not_swallowed_by_vte`、
`random_chunk_boundaries_mixed_corpus_are_chunking_invariant`、
`regression_c1_continuation_bytes_are_not_controls`。

### F2 [High] Sixel filter 在暂存态返回 `None`，原始 ESC 被重复交给 vte

`feed()` 的返回契约是 `None = 本段没有过滤，调用方原样交给 vte`。但
`Sixel`/`SixelEscape` 状态下如果本 chunk 只含 ESC，`swallowed` 为 false、
`out`/`pending` 为空，函数返回 `None`。调用方于是把已经被状态机消费的
ESC 再交给 vte。

触发条件很普通：`ESC \`（Sixel ST）被 PTY 读缓冲切成两块，第二块只有
ESC 时必现。vte 收到孤立 ESC，会和下一个字节拼成新转义序列（如 `ESC [`
开 CSI），吞掉后续字符或产生残影。这正是“图片错位/残影”类症状的一个
可复现字节级来源。

测试：`image_sequence_swallows_only_itself`（drip 切分失败）、
`regression_sixel_lone_esc_must_not_be_passed`。

### F3 [Medium] 截断 Sixel 的畸形终止 ESC 被吃掉

`SixelEscape` 看到不是 `\`/ESC 的字节时中止图片，但只把当前字节按
Ground 重放，**丢掉 ESC**。截断 Sixel 后紧跟 `ESC[0m` 时，vte 收到
`[0m`（正文），SGR 失效且屏幕出现 `[0m` 字样。

测试：`truncated_sixel_is_aborted_by_next_escape_sequence`。

### F4 [Info] 未终止 IIP 的固有限制（已测试，不列为缺陷）

没有 BEL/ST/CAN/ESC 的 `OSC 1337;File=...` 在协议上无法与后续正文区分，
filter 会持续吞到终止符；有 `MAX_BODY_BYTES` 上限，不会死循环/撑爆内存。
本套测试固化了“CAN/SUB 或下一个 ESC 可恢复”的可证明部分，并明确记录
“无任何终止符时正文无法恢复”是协议限制，不是静默丢弃。

### F5 [Info] Kitty C1 APC 未识别（不在本次范围）

Kitty parser 只认 `ESC _ G`，C1 `0x9F G` 不识别；字节会透传给 vte，
不会静默丢弃，故未改。记录为兼容性缺口。

## 4. 修复（同分支 fix(core) commit）

三处都只改 `crates/core/src/sixel.rs`，不动 IIP/Kitty/vte/pane：

1. **F1**：`SixelFilter` 增加 3 字节 UTF-8 尾部窗口（与
   `iip.rs::looks_like_utf8_continuation` 同一判定）。`Ground` 分支的
   `0x90/0x98/0x9B/0x9D/0x9E/0x9F` 入口全部加 `!is_continuation` 守卫；
   快速路径 `return None` 前保留尾部 3 字节。只有“C1 字节同时是合法 UTF-8
   续字节”的输入行为改变；纯 C1 流（无 UTF-8 前缀）语义不变。
2. **F2**：`feed()` 在 `state != Ground` 时也返回 `Some(out)`（可为空）。
   调用方不会再拿到“已消费字节的原始副本”；完整序列行为不变。
3. **F3**：`SixelEscape` 遇到畸形终止时把 ESC 重放为 `Escape` 起点，再处理
   当前字节；截断 Sixel 后的 `ESC[..m` 恢复为完整转义序列。

验证：

```text
cargo test -p tervo-core --test filter_conservation
=> 18 passed; 0 failed; 0 ignored
cargo test -p tervo-core
=> 258 passed, 0 failed（12 个 test binary，含真实 PTY 的 sixel/kitty e2e）
=> Doc-tests tervo_core: 0 passed, 0 failed
```

两个真实 dsh raw fixture 测试（文本逐字节、Kitty 分片守恒）在修复前后都
通过，用于防回归；上述 7 个失败测试在修复前全红（证据文件），修复后全绿。
`plain_text_corpus` 还加入了 2026-11-11 第一处根因（IIP `ESC x` 后丢 `]`）
的最小序列与真实 dsh `[stderr]` 行，防止同类状态泄漏回归。

## 5. 测试命令

```bash
cd /home/user/AIworkspace/rusttabbyterminal/.worktrees/agent-filter-audit
CARGO_HOME=/home/user/AIworkspace/rusttabbyterminal/.cargo CARGO_NET_OFFLINE=true \
  CARGO_TARGET_DIR=/home/user/AIworkspace/rusttabbyterminal/target \
  cargo test -p tervo-core --test filter_conservation
# 审计基线（本 commit 的 7 个已知缺陷，修复前全失败）：
CARGO_HOME=/home/user/AIworkspace/rusttabbyterminal/.cargo CARGO_NET_OFFLINE=true \
  CARGO_TARGET_DIR=/home/user/AIworkspace/rusttabbyterminal/target \
  cargo test -p tervo-core --test filter_conservation -- --ignored
# 全量 core（含真实 PTY 的 sixel/kitty e2e）：
CARGO_HOME=/home/user/AIworkspace/rusttabbyterminal/.cargo CARGO_NET_OFFLINE=true \
  CARGO_TARGET_DIR=/home/user/AIworkspace/rusttabbyterminal/target \
  cargo test -p tervo-core
```

## 6. 限制

- 管线镜像与 `pane.rs` 是两份代码，靠防漂移测试约束；`pane.rs` 若改变
  分派语义，需要人工同步（测试会失败提醒，但不会自动修正）。
- UTF-8/C1 判定是启发式：极端畸形流里“UTF-8 lead byte + 真 C1 控制符”
  会按正文处理；这与 IIP `7e50800` 已接受的取舍一致，纯 C1 流不受影响。
- 未做微基准：修复只在含 ESC/C1 的慢路径加 3 字节窗口，热路径无分配变化；
  若后续要求，可补 `criterion` 对比。
- 真实 raw 覆盖 Kitty 分片传输与 CJK 文本；Sixel 的真实 PTY 行为仍由
  `crates/core/tests/sixel.rs` 的真实 PTY 用例覆盖（本次已随全量 core 跑）。
- 未验证 GUI 端到端截图：本层目标是把 `bytes -> filter` 的守恒钉死；Frame/
  framebuffer 层由其他 agent 的差分测试覆盖。
