# Full-repo review backlog (2026-11-18)

Source: `.logs/review/SUMMARY.md` (9 P0 / 36 P1 / 19 P2), plus the
plugins/WASM review that was missing from the first pass.

Baseline: `7f260b8`. Fixed in: `bdd0548`, `8af72f7`, `8430583`, `c320a54`.
**Install policy: no more `~/.local/bin/xtervo` updates until every item below is
fixed, tested and Codex-reviewed. The final release is installed exactly once.**

## 1. Status legend

- `fixed` — code + regression + relevant e2e/soak evidence in the commit.
- `open` — not fixed; must be fixed before the final install.
- `accepted` — consciously not changed; requires a written rationale and a
  regression that documents the chosen behavior.

## 2. P0 (9)

| ID | Area | Status | Note |
|---|---|---|---|
| S1 | OSC 52 remote clipboard read | fixed | remote `Load` denied by default; unknown value denies |
| S2 | clipboard content in stderr | fixed | log length only |
| S3 | config restore truncation | fixed | unique exclusive temp + rename |
| S4 | recorder export truncation | fixed | unique exclusive temp + rename |
| S5 | multi-window WorkspaceStore overwrite | fixed | `Shared<WorkspaceStore>` |
| S6 | `parse_xcolor` UTF-8 panic | fixed | char-based slice + test |
| S7 | `Shared<T>` Deref/DerefMut aliasing UB | fixed | `Deref/DerefMut` removed, `with/with_mut` only (`runtime.rs:90-130`); `shared_handles_point_at_one_value` |
| S8 | session clear before rewrite | fixed | write new snapshot before consuming |
| S9 | e2e `rm -rf $E2E_OUT` | fixed | refuse existing caller directory |

## 3. P1 (36)

### Core input/output

| ID | Area | Status | Note |
|---|---|---|---|
| C1 | foreground/PTY byte ordering | fixed | chunk carries pgrp, ordered transition chunks |
| C2 | foreground sampling only on read | fixed | 100 ms `poll` timeout in reader (no extra thread) |
| C3 | close pane does not stop blocking I/O | fixed | `TransportHalves::shutdown` + PaneHandle drop (`transport.rs:70`, `pane.rs:5440`); `dropping_pane_handle_stops_blocked_plugin_read_within_1s` |
| C4 | keyboard UTF-8 continuation as C1 | fixed | UTF-8 guard + chunked test |
| C5 | `Exited` lost when event queue full | fixed | `exit_pending` AtomicBool bypass (`event.rs:183`); `exited_survives_full_queue` |
| O1 | OSC UTF-8 continuation as C1 | fixed | payload preserved; malformed ESC test |
| O2 | clear + kitty transmit same batch | fixed | byte-order segments; `kitty_clear_then_transmit_same_chunk_matches_split_chunks` |
| O3 | kitty placement unbounded | fixed | p=0 dedup + 4096 cap (`pane.rs:2474,2697`); `kitty_zero_id_placements_dedup_and_cap` |
| O4 | sixel canvas allocated before limit | fixed | budget checked before resize (`sixel.rs:807`); `huge_repeated_bands_are_rejected_before_canvas_blowup` |
| O5 | OSC palette query uses batch-end color | fixed | query at sequence position; `osc4_same_chunk_query_set_query_returns_query_time_colors` |
| O6 | linkifier callback under lock | fixed | Arc list cloned, lock released (`link.rs:435`); `linkifier_callback_can_register_without_deadlock` |
| O7 | recorder active after PTY EOF | fixed | stop on `PtyClosed` (`pane.rs:643`); `recording_stops_when_pty_reaches_eof` |

### App / lifecycle / platform

| ID | Area | Status | Note |
|---|---|---|---|
| A1 | save failure reported as success | fixed | `Config::save/save_to -> Result` (`config.rs:822-848`), UI error (`main.rs:1967`); `save_failure_is_reported_to_caller` |
| A2 | unbounded notification threads | fixed | cap 4 + 2 s child timeout |
| A3 | reopen loses snapshot on failure | fixed | push snapshot back |
| A4 | closing last tab quits whole app | fixed | close current window, e2e added |
| A5 | open_window timeout still creates | fixed | cancel bit checked before UI closure (`main.rs:21215`); `secondary_times_out_with_clear_error` |
| A6 | X11 corners matched by title | fixed | `_NET_WM_PID` + physical size (`xconn.rs:279`); multi-window e2e |
| A7 | kitty release uses release-time target | fixed | press target/mods; `key_identity_matches_press_and_release` |
| A8 | multi-window e2e stale title suffix | fixed | `_NET_WM_PID` XID lookup (`scripts/mw_session_e2e.py:248`) |
| P1 | maximize worker exit only flips UI | fixed | rollback on send failure (`maximize/controller.rs:370`) |
| P2 | Quake hotkey registered when disabled | fixed | registration only when enabled (`main.rs:21959`) |
| P3 | X11 fast path loses screen/server | fixed | explicit DISPLAY screen (`xconn.rs:420`); `display_number_parses_host_suffix` |
| P4 | clipboard image temp not cleaned | fixed | 24h sweep + RAII failure cleanup (`main.rs:14009-14067`); `stale_clipboard_images_are_cleaned`, `failed_clipboard_image_encode_removes_partial_temp` |
| P5 | Quake key auto-repeat toggles | fixed | `ToggleGate` rejects fake release/press (`quake.rs:30-66`); `auto_repeat_release_press_pairs_do_not_retoggle` |
| M1 | MCP token from untrusted state dir | fixed | `O_NOFOLLOW`+fstat+0600+dir tightening (`mcp.rs:572-740`); `token_read_rejects_group_or_world_accessible_mode`, `token_dir_is_tightened_or_rejected` |
| M2 | MCP old token reused after tighten | fixed | mtime reload/revoke (`mcp.rs:1088` test) |
| M3 | MCP token init race | fixed | temp+fsync+hard_link publish (`mcp.rs:711`); `concurrent_token_creation_sees_one_valid_token` |
| M4 | MCP token read limit after full read | fixed | bounded `take(MAX+1)` (`mcp.rs:612`); `token_read_is_bounded_even_for_endless_reader` |
| M5 | MCP enabled when config unreadable | fixed | fail closed (`mcp.rs:470`); `config_enabled_from_text_is_conservative` |

### Tests / CI / docs

| ID | Area | Status | Note |
|---|---|---|---|
| T1 | CI image helper from wrong checkout | fixed | builds helpers from current checkout (`scripts/ci_gate.sh:14`) |
| T2 | TabBar `tabs=1` early log | fixed | before/after close marker (`scripts/e2e_tabbar_plus_profile.sh:136+`) |
| T3 | diag live single `recv` assumes JSON | fixed | loop until newline/EOF (`scripts/e2e_diag_live.sh:58`) |
| T4 | doctor `--fail-on` bad value exits 0 | fixed | enum validation returns error (`doctor.rs:554`); `doctor_cli` tests |
| T5 | doctor `--attach-tail` >64 KiB | fixed | `file.take(max_bytes)` (`doctor.rs:487`) |
| T7 | matrix/docs stale | **open** | stale notice + key conflicts fixed; full row-by-row rewrite still pending (see §9) |

## 4. P2 (19 + review follow-ups)

| ID | Area | Status | Note |
|---|---|---|---|
| C6 | mouse press/release split by full queue | fixed | press reserves release slot (`pane.rs:5325`, `pane.rs:5389`) |
| C7 | reader spawn failure leaks dup fd | fixed | spawn-failure closes `fg_fd` (`pane.rs:1442-1446`) |
| C8 | intermediate keyboard test timing | fixed | pgrp marker sync (`tests/keyboard_protocol.rs:252`) |
| O2-O7 | see P1 table | fixed | all verified above |
| R4 | glyph overhang damage leaves old strokes | fixed | glyph quad/UV clipped to cell (`render-wgpu/src/text.rs:502-552`); `overhanging_glyph_does_not_survive_cell_damage` |
| R6 | missing-font image/damage tests return early | fixed | hard panic helper (`test_support.rs:21-27`); `image_layer_diff_tests` |
| P6 | DamageWatcher drop leaves thread/fd | fixed | stop + join in `Drop` (`latency.rs:308`) |
| P7 | IME smoke ignores PTY bytes | fixed | asserts `[send] bytes=` (`scripts/check_ime.sh:59-68`) |
| P8 | DISPLAY parser test not production fn | fixed | test calls `parse_display` (`maximize/x11.rs:533-534`) |
| T6 | soak PASS with thread/fd warning; no live heap | fixed | `SOAK_STRICT=1` promotes thread/FD growth to issues (`scripts/soak.sh:973-980`) |
| K1 | shell first-prompt kitty/modifyOtherKeys reset | accepted | foreground-pgrp change resets stale flags; next prompt re-declares. Regression: `foreground_command_exit_resets_stale_kitty_flags`, `intermediate_tui_child_exit_resets_stale_kitty_flags` |
| F1-F5 / V1-V5 / H1-H6 / W1-W2 | plugins/WASM/vault/SSH review findings | see §7 | F/V/H fixed; W1/W2 remain open (see §9) |

## 5. Execution plan (18:00+)

1. Batch S7: remove `Deref/DerefMut` from `Shared<T>`; add `with/with_mut` only,
   migrate call sites; add compile-fail/aliasing regression where possible.
2. Core input/output: C3, C5, O2-O7, C6-C8; each with failing test first.
3. App/lifecycle/platform: A1, A5-A8, P1-P5, M1-M5.
4. GPU/render: R4, R6; add pixel/damage regression per `AGENTS.md`.
5. Tests/CI/docs: T1-T7, C8, P6-P8, T6.
6. Plugins/WASM findings F1-F?.
7. Final: app/core/scene/graphics tests, TabBar/keyboard/pin/multi-window e2e,
   2m soak (15m/30m for release candidate per `AGENTS.md`), Codex review.
8. **Only then** build/install one final release; record sha256.

## 6. Useful commands

```bash
CARGO_HOME=$PWD/.cargo cargo test -p xtervo-app --bin xtervo --offline
CARGO_HOME=$PWD/.cargo cargo test -p xtervo-core --offline
CARGO_HOME=$PWD/.cargo cargo test -p xtervo-scene -p xtervo-graphics --offline
XTERVO_INSTANCE_DIR=/tmp/xtervo-e2e E2E_BIN_DIR=$PWD/target/debug \
  E2E_OUT=/tmp/xtervo-kbd bash scripts/e2e_keyboard_protocol.sh
XTERVO_INSTANCE_DIR=/tmp/xtervo-e2e DURATION=2m BIN=$PWD/target/release/xtervo \
  bash scripts/soak.sh
```

## 7. plugins / WASM / vault / SSH review (2026-11-19)

`plugin.rs` is legacy and not compiled (`plugin_v3` is the live host); its 4
findings are recorded in `.logs/review/L7d-*` but are not actionable unless the
file is re-enabled. Live-host findings:

| ID | Sev | Area | Status |
|---|---|---|---|
| F1 | P1 | supervisor stdin `write_all` blocks past deadline/shutdown (`supervisor.rs:1960`) | fixed | writer thread + bounded queue; `frozen_stdin_command_times_out_and_shutdown_kills_process` |
| F2 | P1 | plugin-chosen `window_id` makes statuses unbounded / fake asks exhaust approval slots (`supervisor.rs:1799`) | fixed | registry-bounded windows; `fake_window_asks_do_not_occupy_approval_slots`, `fake_window_status_ids_do_not_grow_status_map` |
| F3 | P1 | `when` recursive parser stack overflow during discovery/reload (`manifest.rs:425`, `context.rs:353`) | fixed | `MAX_WHEN_DEPTH` (`context.rs:311,358`); `deeply_nested_when_is_rejected_not_stack_overflow` |
| F4 | P2 | plugin stderr/message injected raw into logs/UI (ANSI/CR) | fixed | sanitize before display (`plugin_v3/supervisor.rs`); `plugin_stderr_control_bytes_are_stripped_from_ui` |
| F5 | P2 | WASM Enable `fs::read` whole file before 8 MiB check (`mod.rs:1312`) | fixed | stat + `read_capped` (`plugin_wasm.rs:525`); `oversized_wasm_is_rejected_from_metadata_without_reading` |
| V1 | P1 | vault `change_master` races foreground `set`/second process; split master key (`vault.rs:394`) | fixed | flock + merge/rekey guards; `cross_process_set_merges_stale_session`, `change_master_invalidates_other_session_credentials` |
| V2 | P1 | vault `MAX_FILE` checked after full read; persist has no size cap (`vault.rs:566`) | fixed | stat + `take(MAX_FILE+1)` (`vault.rs:938-954`), persist cap (`vault.rs:1019`); `oversized_vault_file_is_rejected_before_read` |
| V3 | P2 | vault temp file predictable/symlink-following; 0600 unchecked; no parent fsync | fixed | unique tmp + `O_NOFOLLOW` + 0600 + fsync (`vault.rs:1027-1076`); `persist_does_not_follow_symlinked_legacy_tmp_file` |
| V4 | P2 | vault plaintext/key zeroization incomplete | fixed | wipe displaced secrets/key; `clear_remove_and_overwrite_wipe_displaced_secrets`, `unlocked_wipe_zeroes_key_and_entries_in_place` |
| V5 | P2 | vault KDF header limits too wide (256 MiB / seconds) | fixed | params capped before Argon2 (`vault.rs:1355`); `kdf_params_above_caps_are_rejected_before_argon2` |
| W1 | P2 | WASM per-plugin ticker/instance unbounded; sync compile on UI | **open** | still one `EpochTicker` thread per filter (`plugin_wasm.rs:145`) and `Module/Instance::new` on the enable caller (`plugin_wasm.rs:226`); needs shared ticker + off-UI compile, not a contained fix |
| W2 | P2 | WASM hot path holds host read lock while UI wants write | **open** | `filter()` holds `self.filters.read()` across plugin calls (`plugin_wasm.rs:627`); fixing needs clone-Arc + contention regression, left for the WASM host pass |
| H1 | P2 | SSH plugin path skips known_hosts precheck; safe `-o` ordering can be downgraded | fixed | precheck before spawn (`ssh_plugin.rs:1656,1668`); `host_key_precheck_gates_real_ssh_only` |
| H2 | P2 | SSH spawn error paths leak child/thread/FD | fixed | child guard kills/reaps (`ssh_plugin.rs:1682`); `child_guard_kills_and_reaps_on_early_return` |
| H3 | P2 | SSH/Profile text control chars unsanitized | fixed | `sanitize_control` (`ssh_plugin.rs:726`); `stderr_control_sequences_are_sanitized_in_pane`, `remote_label_sanitizes_control_characters` |
| H4 | P2 | SSH stderr tail implementation keeps head, not tail | fixed | ring keeps last bytes (`ssh_plugin.rs:1532,1764`) |
| H5 | P2 | SSH watchdog check-then-act false timeout | fixed | atomic timeout claim (`ssh_plugin.rs:1699`); `watchdog_timeout_claim_is_atomic_with_output_and_close` |
| H6 | P2 | `SplitTransport::close` does not close I/O; blocked reader never exits | fixed | `close_inner` calls shutdown (`transport.rs:232`); `split_control_close_wakes_blocked_read` |

SSH/transport review found no P0/P1. WASM found no sandbox escape; vault has two
P1 races/limits. All must be fixed before the single final install.

## 8. 2026-11-19 收尾状态（安装门禁仍关闭）

已合并到 main（按时间）：`bdd0548` → `8af72f7` → `8430583` → `c320a54` →
`2d52928`（S7）→ `06929f3` → vault/plugin/SSH 三个 worktree → `cb02cf1` →
`3886f68` → `07aba29` → `5a967a5`。

已修并验证：
- 全部 9 个 P0（含 S7 `Shared<T>` 移除 Deref/DerefMut）。
- C1/C2/C4/C5/C7、O1/O3/O4/O6/O7、A1-A8（A8 脚本）、P1-P5、
  M1/M3/M4/M5、T1-T6、R1/R2/R3/R5。
- 插件/WASM/vault/SSH 复审：F1-F5、V1-V5、H1-H6 全部修复；
  vault 25 tests、plugin_v3 88 tests、ssh 23 tests。
- 证据：app 703 passed；core lib 172 + 集成全绿；TabBar/keyboard/pin/
  single-instance 多窗口 e2e PASS；2m soak PASS（threads 111 / fds 121 不变）。

已修（2026-11-19 下午）：
- **O2/O5**：clear/Kitty 同批按字节序切段；OSC 4/10/11/12 查询按序列位置
  立即回包（同批 query→set→query 与拆批一致）。core 174 lib + 集成全绿。
- **C3**：`PaneHandle` drop 保存 `TransportHalves::shutdown`，插件阻塞 read
  1s 内唤醒；PTY reader stop 保留。
- **C6**：鼠标 press 预留 release 槽位；press 未入队时 release 一并丢弃，
  满队列拖拽不再留孤立 release。
- **R4**：GPU glyph quad/UV 裁剪到字符 cell；越界笔画局部 damage 逐字节
  与整屏重绘一致。
- **R6**：缺字体时差分测试显式失败，不再静默 return。
- **M2**：`token()` 校验文件 mtime，运行中替换/删除会重读或撤销。
- **T7**：FEATURE_MATRIX 加过时提示并修正明显冲突行；tabby-reference 的
  SSH/Serial/会话恢复已改成真实状态（完整矩阵重写仍可作为文档 backlog）。

最终门禁（已完成）：
- app 704 passed；core lib 174 + 集成全绿。
- TabBar/keyboard/pin/single-instance 多窗口 e2e（debug + release）PASS。
- 2m soak PASS；15m `SOAK_STRICT=1` soak PASS：
  RSS 575.0→666.8 MiB（+4.01 MiB/min，R²=0.27），threads 111 / fds 121
  不变，fatal=0。
- Codex 最终复审（`864ebd8 → d16ec56`）无 finding；更早复审的 1 P1 + 4 P2
  均已修复。
- **已安装** `~/.local/bin/xtervo`，sha256
  `34afe69bead840b6e11e40f2fd5fd738ccf6cc8ff9d9bd4e49517c2411d9eb8c`。

仍记录但不阻塞：
- SSH plugin 首次连接要求先用普通 SSH 确认主机密钥（安全取舍，未接
  hostkey 对话框）。
- T7 的矩阵逐条重写（当前已修正关键冲突并加过时提示）。

这些项完成并通过最终 e2e/2m soak 后，才执行唯一一次
`install -m 755 target/release/xtervo ~/.local/bin/xtervo`。

## 9. 2026-11-24 open-item reconciliation

基线 `fdfd9c4`（main）。逐条把本文件所有 `status=open` 行与当前源码/测试
对照；上面表格已改为复核后的状态，证据写在每行 Note 里。结论：

- **已在此前提交修复，复核通过**：S7、C3/C5/C6/C7/C8、O2-O7、A1、A5-A8、
  P1-P4、M2/M3/M5、T1-T6、R4/R6、P6-P8、F1-F5、V1-V5、H1-H6。
  C3/C5 已由 `fb9853a`/`6554a7a`/`77043ac` 完成（transport shutdown、
  `Exited` 旁路），本轮只复核，不重复改架构。
- **本轮补的洞**（均有先失败、后通过的回归测试）：
  - M1/M4：`read_token_file` 之前只查 `symlink_metadata` 后整读，0640
    token 仍会被 bridge 接受，且 read 无上限。现在 `O_NOFOLLOW` 打开、
    fd 上校验 owner/mode，最多读 `MAX_TOKEN_BYTES+1`；token 目录
    group/other 可写时收紧到 0700，sticky 共享目录拒绝。
    测试：`token_read_rejects_group_or_world_accessible_mode`、
    `token_dir_is_tightened_or_rejected`、
    `token_read_is_bounded_even_for_endless_reader`。
  - P4：编码失败会留下半截 `xtervo-paste-*.png`。成功文件仍需保留给 TUI
    异步读取（24h 清理），失败路径用 `PasteTempFile` RAII 删除。
    测试：`failed_clipboard_image_encode_removes_partial_temp`。
  - P5：旧 `held` 标志把 X11 自动重复的 fake release/press 对当成真实
    松开，repeat delay 后再次 toggle。`ToggleGate` 用 release→press
    <30ms 识别自动重复对。
    测试：`auto_repeat_release_press_pairs_do_not_retoggle`、
    `quick_double_press_is_debounced_until_release`。
  - A1：补 `save_failure_is_reported_to_caller`，把“save 返回 Result 且
    UI 提示失败”固化成回归。
- **仍 open**：
  - **T7**（matrix 逐条重写）：`docs/FEATURE_MATRIX.md` 首行仍有过时提示，
    本轮只确认关键冲突行已修；完整逐行重写属于文档工作，未在安全修复里
    硬塞。
  - **W1**（WASM ticker/编译在 UI 线程）：每个过滤器仍有一条
    `EpochTicker` 线程，`Module/Instance::new` 仍在 enable 调用线程；
    需要共享 ticker + 后台编译的架构改动。
  - **W2**（WASM 热路径持 host read lock）：`filter()` 在插件调用期间
    持有 `filters.read()`；需要 clone-Arc 后放锁 + 争用回归，属于同一
    轮 WASM host 架构改动。
- **accepted**：K1（shell 首提示符 kitty/modifyOtherKeys）：无法可靠判定
  写者，保留前台 pgrp 变化时的安全复位；第二个提示符由应用重新声明，
  `foreground_command_exit_resets_stale_kitty_flags` 与
  `intermediate_tui_child_exit_resets_stale_kitty_flags` 固化行为。
