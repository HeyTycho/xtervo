# Full-repo review backlog (2026-11-18)

Source: `.logs/review/SUMMARY.md` (9 P0 / 36 P1 / 19 P2), plus the
plugins/WASM review that was missing from the first pass.

Baseline: `7f260b8`. Fixed in: `bdd0548`, `8af72f7`, `8430583`, `c320a54`.
**Install policy: no more `~/.local/bin/tervo` updates until every item below is
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
| S7 | `Shared<T>` Deref/DerefMut aliasing UB | **open** | remove unsafe `as_ptr` API; migrate call sites |
| S8 | session clear before rewrite | fixed | write new snapshot before consuming |
| S9 | e2e `rm -rf $E2E_OUT` | fixed | refuse existing caller directory |

## 3. P1 (36)

### Core input/output

| ID | Area | Status | Note |
|---|---|---|---|
| C1 | foreground/PTY byte ordering | fixed | chunk carries pgrp, ordered transition chunks |
| C2 | foreground sampling only on read | fixed | 100 ms `poll` timeout in reader (no extra thread) |
| C3 | close pane does not stop blocking I/O | **open** | writer/reader/SplitControl cancellation |
| C4 | keyboard UTF-8 continuation as C1 | fixed | UTF-8 guard + chunked test |
| C5 | `Exited` lost when event queue full | **open** | terminal exit must be observed/reserved |
| O1 | OSC UTF-8 continuation as C1 | fixed | payload preserved; malformed ESC test |
| O2 | clear + kitty transmit same batch | **open** | parser index/store divergence |
| O3 | kitty placement unbounded | **open** | dedup/cap by image+position |
| O4 | sixel canvas allocated before limit | **open** | check raster size first |
| O5 | OSC palette query uses batch-end color | **open** | query at call position |
| O6 | linkifier callback under lock | **open** | drop lock before callback |
| O7 | recorder active after PTY EOF | **open** | stop recorder on `PtyClosed` |

### App / lifecycle / platform

| ID | Area | Status | Note |
|---|---|---|---|
| A1 | save failure reported as success | **open** | return `Result`, surface UI error |
| A2 | unbounded notification threads | fixed | cap 4 + 2 s child timeout |
| A3 | reopen loses snapshot on failure | fixed | push snapshot back |
| A4 | closing last tab quits whole app | fixed | close current window, e2e added |
| A5 | open_window timeout still creates | **open** | cancel queued closure / ack protocol |
| A6 | X11 corners matched by title | **open** | use XID/`_NET_WM_PID` |
| A7 | kitty release uses release-time target | **open** | record press target/mods |
| A8 | multi-window e2e stale title suffix | **open** | `mw_session_e2e.py` still uses `Tervo — 2` |
| P1 | maximize worker exit only flips UI | **open** | reconcile X11 state |
| P2 | Quake hotkey registered when disabled | **open** | unregister when off |
| P3 | X11 fast path loses screen/server | **open** | pass screen explicitly |
| P4 | clipboard image temp not cleaned | **open** | RAII temp cleanup |
| P5 | Quake key auto-repeat toggles | **open** | ignore repeat |
| M1 | MCP token from untrusted state dir | **open** | owner/mode validation, secure dir |
| M2 | MCP old token reused after tighten | **open** | reload on permission/change |
| M3 | MCP token init race | **open** | write+fsync before publish |
| M4 | MCP token read limit after full read | **open** | bounded `take` |
| M5 | MCP enabled when config unreadable | **open** | fail closed on helper error |

### Tests / CI / docs

| ID | Area | Status | Note |
|---|---|---|---|
| T1 | CI image helper from wrong checkout | **open** | build helpers in current worktree |
| T2 | TabBar `tabs=1` early log | **open** | assert after action marker |
| T3 | diag live single `recv` assumes JSON | **open** | loop until newline/frame |
| T4 | doctor `--fail-on` bad value exits 0 | **open** | validate enum |
| T5 | doctor `--attach-tail` >64 KiB | **open** | enforce cap while reading |
| T7 | matrix/docs stale | **open** | rewrite from current code |

## 4. P2 (19 + review follow-ups)

| ID | Area | Status | Note |
|---|---|---|---|
| C6 | mouse press/release split by full queue | **open** | atomic pair into queue |
| C7 | reader spawn failure leaks dup fd | **open** | RAII fd guard |
| C8 | intermediate keyboard test timing | **open** | marker/pgrp synchronization |
| O2-O7 | see P1 table | **open** | |
| R4 | glyph overhang damage leaves old strokes | **open** | clip glyph quads / damage |
| R6 | missing-font image/damage tests return early | **open** | skip must fail loudly |
| P6 | DamageWatcher drop leaves thread/fd | **open** | join/close on drop |
| P7 | IME smoke ignores PTY bytes | **open** | assert bytes |
| P8 | DISPLAY parser test not production fn | **open** | call exported parser |
| T6 | soak PASS with thread/fd warning; no live heap | **open** | red flags into verdict |
| K1 | shell first-prompt kitty/modifyOtherKeys reset | **accepted?** | ambiguous writer; safety resets; second prompt re-declares. Add regression + rationale. |
| F1-F? | plugins/WASM review findings | **TBD** | `.logs/review/L7d-plugins-wasm.md` |

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
CARGO_HOME=$PWD/.cargo cargo test -p tervo-app --bin tervo --offline
CARGO_HOME=$PWD/.cargo cargo test -p tervo-core --offline
CARGO_HOME=$PWD/.cargo cargo test -p tervo-scene -p tervo-graphics --offline
TERVO_INSTANCE_DIR=/tmp/tervo-e2e E2E_BIN_DIR=$PWD/target/debug \
  E2E_OUT=/tmp/tervo-kbd bash scripts/e2e_keyboard_protocol.sh
TERVO_INSTANCE_DIR=/tmp/tervo-e2e DURATION=2m BIN=$PWD/target/release/tervo \
  bash scripts/soak.sh
```

## 7. plugins / WASM / vault / SSH review (2026-11-19)

`plugin.rs` is legacy and not compiled (`plugin_v3` is the live host); its 4
findings are recorded in `.logs/review/L7d-*` but are not actionable unless the
file is re-enabled. Live-host findings:

| ID | Sev | Area | Status |
|---|---|---|---|
| F1 | P1 | supervisor stdin `write_all` blocks past deadline/shutdown (`supervisor.rs:1960`) | open |
| F2 | P1 | plugin-chosen `window_id` makes statuses unbounded / fake asks exhaust approval slots (`supervisor.rs:1799`) | open |
| F3 | P1 | `when` recursive parser stack overflow during discovery/reload (`manifest.rs:425`, `context.rs:353`) | open |
| F4 | P2 | plugin stderr/message injected raw into logs/UI (ANSI/CR) | open |
| F5 | P2 | WASM Enable `fs::read` whole file before 8 MiB check (`mod.rs:1312`) | open |
| V1 | P1 | vault `change_master` races foreground `set`/second process; split master key (`vault.rs:394`) | open |
| V2 | P1 | vault `MAX_FILE` checked after full read; persist has no size cap (`vault.rs:566`) | open |
| V3 | P2 | vault temp file predictable/symlink-following; 0600 unchecked; no parent fsync | open |
| V4 | P2 | vault plaintext/key zeroization incomplete | open |
| V5 | P2 | vault KDF header limits too wide (256 MiB / seconds) | open |
| W1 | P2 | WASM per-plugin ticker/instance unbounded; sync compile on UI | open |
| W2 | P2 | WASM hot path holds host read lock while UI wants write | open |
| H1 | P2 | SSH plugin path skips known_hosts precheck; safe `-o` ordering can be downgraded | open |
| H2 | P2 | SSH spawn error paths leak child/thread/FD | open |
| H3 | P2 | SSH/Profile text control chars unsanitized | open |
| H4 | P2 | SSH stderr tail implementation keeps head, not tail | open |
| H5 | P2 | SSH watchdog check-then-act false timeout | open |
| H6 | P2 | `SplitTransport::close` does not close I/O; blocked reader never exits | open |

SSH/transport review found no P0/P1. WASM found no sandbox escape; vault has two
P1 races/limits. All must be fixed before the single final install.
