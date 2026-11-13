# Multi-window session v2 — evidence (2026-11-13)

Branch: `agent/mw-session-integration` (base `main` @ `85a4994`)

## Scope

Product integration of session v2 multi-window save/restore:

- `SharedServices` owns the 2s autosave timer and the config watcher.
- Every 2s `runtime::capture_restorable_snapshot_v2()` -> `session::save_v2()`
  (tmp + fsync + rename + previous-generation `.bak`).
- Window create/close triggers an immediate save; only the primary process is
  the session writer (`--window` child stays writer=false).
- Startup reads `SnapshotV2`, restores all windows (geometry/maximized/
  active_tab/tabs/splits/focus); v1 files migrate to one v2 window.
- Empty/bad/partial windows are skipped or fall back to a fresh tab; startup is
  never aborted for a bad snapshot.
- Closing primary keeps autosave/config watcher alive while another window
  exists; only the last window exits and clears the running marker.

## Unit tests

```bash
CARGO_HOME=$PWD/../../.cargo CARGO_TARGET_DIR=$PWD/../../target \
CARGO_NET_OFFLINE=true cargo test -p tervo-app --bin tervo
# test result: ok. 549 passed; 0 failed; 1 ignored
```

New/covered cases include `v1_loads_as_single_window_v2`,
`v2_roundtrip_preserves_windows_geometry_maximized_and_tabs`,
`v1_file_roundtrips_through_v2_disk_format`,
`restorable_windows_skip_empty_and_clamp_active_tab`,
`restorable_windows_are_capped_for_corrupt_files`.

## Multi-window session e2e (Xvfb + Openbox)

```bash
CARGO_TARGET_DIR=$PWD/../../target ./scripts/e2e_multi_window_session.sh
# PASS autosave_v2_two_windows
# PASS auto_restore_v2_two_windows
# PASS primary_close_keeps_config_watcher
# PASS primary_close_keeps_session_writer
# PASS restore_after_primary_close
# PASS explicit_restore_flag_two_windows
# PASS v1_snapshot_migrates_single_window
# PASS partial_snapshot_skips_empty_window
# PASS restore_primary_maximized
# PASS restore_switch_off_keeps_fresh_layout
# PASS last_window_clean_exit
```

Automation summary (`report.json`):

- Start 2 windows, 2/3 tabs, geometry 640x480 @60,80 and 700x560 @760,120;
  maximize window 2. After 2s autosave, `session.json` is v2 with two windows,
  correct `tabs`, `active_tab` (1/2), geometry and `maximized` (false/true).
- `kill -9` then restart: both windows/tabs/active_tab/geometry/maximized come
  back; logs `多窗口恢复完成：2/2`.
- Close primary through the live socket; second keeps running, autosave writes
  the surviving 1-window/4-tab layout, config hot reload still logs
  `已套用外部热更新`.
- `--restore` with `restore_session_on_startup=false` still restores 2 windows.
- v1 snapshot restores 1 window / 2 tabs / active=1.
- v2 snapshot with one empty window and `active_tab=99` restores 1 usable
  window, clamps active_tab to 0, process stays alive.
- Primary-maximized v2 snapshot restores maximized through the X11 state
  machine.
- `restore_session_on_startup=false` without `--restore` keeps a fresh layout.
- After primary is closed the running marker stays; closing the last window via
  WM_DELETE_WINDOW exits with code 0, writes the 1-window/4-tab layout and
  removes the running marker.

Existing multi-window e2e still passes:

```bash
CARGO_TARGET_DIR=$PWD/../../target E2E_OUT=/tmp/mw-inproc-e2e \
  ./scripts/e2e_multi_window_inproc.sh
# PASS two_mainwindows_one_process ... PASS kill -9 single-window recovery (3 tabs)
```

`E2E_OUT` must be a short path under this worktree: AF_UNIX `sun_path` is
limited to 108 bytes and the worktree prefix is long.

## 2m dual-window soak (autosave enabled)

```bash
CARGO_TARGET_DIR=$PWD/../../target SOAK_OUT=$PWD/target/mw-soak \
  ./scripts/soak_multi_window_inproc.sh
# PASS 2m dual-window soak: rss_growth=3940KiB threads=21->21 fds=28->28
```

Soak config enables `restore_session_on_startup=true`; `session.json` at the
end is v2 with 2 windows. Samples: `soak/samples.csv`, `soak/samples.json`.
15m/30m soak not run (not required for this branch; AGENTS.md memory gate).
