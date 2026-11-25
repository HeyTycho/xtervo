# Terminal behavior

The **Settings → Terminal** page controls the bell, scrolling, the scrollbar, and the bytes sent by Backspace and Delete. Every change is written to `~/.config/xtervo/config.toml` and applied immediately, with one exception: scrollback capacity only affects panes created after the change.

Settings search indexes these controls and jumps to the Terminal category.

## Settings and defaults

| Key | Default | Meaning |
|---|---|---|
| `audible_bell` | `false` | Ring the system bell when a pane receives BEL |
| `visual_bell` | `true` | Flash the pane border when a pane in the active tab receives BEL |
| `scroll_on_output` | `false` | Return to the bottom whenever new output arrives |
| `scroll_on_keystroke` | `true` | Return to the bottom after a keystroke |
| `scrollback` | `2000` | History lines for newly created panes |
| `scrollback_unlimited` | `false` | Ignore `scrollback` and use the 100,000-line safety cap |
| `scrollbar_policy` | `"auto"` | `always` / `auto` / `never` |
| `backspace_binding` | `"ascii-delete"` | `ascii-delete` (`0x7f`) / `control-h` (`0x08`) |
| `delete_binding` | `"delete-sequence"` | `delete-sequence` (`ESC [3~`) / `ascii-del` (`0x7f`) / `control-h` (`0x08`) |

## Audible bell

- `audible_bell` is **off by default**, so upgrading does not suddenly make the terminal ring. It can be enabled in Settings.
- It is independent of the visual bell: you can ring only, flash only, enable both, or disable both.
- A BEL from a pane in a background tab still rings; the pane does not need to be in the active tab. This matches GNOME Terminal.
- On X11, xtervo sends a core `Bell` request with `percent = -100`, which means the server's default volume. The X11 connection is opened lazily and reused on the UI thread; a failed connection is cleared and retried on the next BEL.
- Without X11 (no `DISPLAY`) and when stderr is a terminal, xtervo writes one BEL byte so the terminal that launched xtervo rings. If both paths fail, the event is silent and logged.
- Multiple BELs received in the same event-processing round are coalesced into one ring, which prevents a BEL storm from flooding the X server. BEL is handled immediately through a CoreEvent wake-up; a 400 ms timer is only a fallback. The log line is:

  ```text
  [bell] ts_ms=... audible count=N backend=ok|unavailable
  ```

Limitations:

- Whether X11 produces sound depends on the desktop sound theme and `xset b`; under headless Xvfb only the request can be verified.
- There are no volume, tone, or per-tab mute settings.
- On Wayland without XWayland, only the stderr fallback is available.

The visual bell is controlled by `visual_bell` (default `true`) and flashes the pane that received BEL when its tab is active. See [Terminal UI basics](terminal-ui-basics.md#visual-bell) for details.

## Scroll policy

| Key | Default | Meaning |
|---|---|---|
| `scroll_on_output` | `false` | Whether output forces the view back to the bottom |
| `scroll_on_keystroke` | `true` | Whether a keystroke returns the view to the bottom (GNOME default) |

### `scroll_on_output = false` (default)

When you scroll up to read history, new output grows the history but does **not** steal the viewport. This is the existing alacritty grid behavior: while `display_offset != 0`, `Grid::scroll_up` increments `display_offset` in step, so the viewport stays on the same content.

### `scroll_on_output = true`

After a batch of PTY bytes is parsed, the core calls `Term::scroll_display(Scroll::Bottom)`, so any output pulls the viewport back to the latest line. The switch can be changed at runtime through `PaneHandle::set_scroll_on_output(bool)` (`Ctrl::SetScrollOnOutput`), and it takes effect on existing panes, not only on new ones.

Because the rule is literally "any output follows the bottom," output that produces no new line (such as BEL or an OSC sequence) also returns the view to the bottom.

### `scroll_on_keystroke = true` (default)

After a key is encoded and written to the PTY:

1. The UI reads a lock-free atomic snapshot from `PaneHandle::scroll_position()` and checks whether `offset > 0`.
2. Only if the view is actually scrolled up does it call `PaneHandle::scroll_to_bottom()` (`Ctrl::ScrollTo(0)`). The request is asynchronous and does not block the UI or the PTY.
3. This applies only to physical keypresses and IME commits. Paste and drag-and-drop insertion use a different path and do not trigger it.

When `scroll_on_keystroke = false`, keypresses do not change the scroll position at all.

## Scrollback

`scrollback` sets the history buffer size for new panes. The default is 2,000 lines.

| Key | Default | Meaning |
|---|---|---|
| `scrollback_unlimited` | `false` | When true, ignore `scrollback` and use the safety cap |

- The safety cap is `UNLIMITED_SCROLLBACK_LINES = 100_000` lines, which is also the maximum value of the `+/-` control in Settings.
- The memory cost is explicit: alacritty scrollback is **preallocated**, so 100,000 lines × 120 columns × 16 B ≈ **192 MB**, and **each new pane reserves its own copy**. "Unlimited" therefore means a clear, bounded, explainable cap, not literally infinite history. The Settings notice shows this number.
- The setting applies only to panes created afterward; there is no safe alacritty API for growing an existing history buffer. Existing panes keep their old capacity.
- Using `+/-` in Settings automatically turns off `scrollback_unlimited` and continues from the current numeric `scrollback` value.
- `Config::effective_scrollback()` determines the capacity for new panes. The CLI flag `--scrollback` still changes only the numeric field; when `scrollback_unlimited = true`, the cap takes precedence.

## Scrollbar policy

| Key | Default | Meaning |
|---|---|---|
| `scrollbar_policy` | `"auto"` | `always` / `auto` / `never` |

- `auto`: show the scrollbar only when `history > 0`, meaning scrollback actually extends beyond the viewport.
- `always`: always show it. With no history, the thumb fills the whole track, indicating that a scrollbar exists but all content fits in the viewport. With history, the thumb shrinks proportionally.
- `never`: always hide it. Wheel scrolling, keyboard scrolling, selection, and mouse reporting are unaffected.
- The scrollbar is a 9 px overlay on the right edge. It does not consume terminal columns and does not change the pane size or trigger a PTY resize. The outer 5 px of the right edge remains the window-resize hit band.
- A settings change refreshes every pane view immediately (`refresh_all_pane_views`); a new pane is not required.

See [Terminal UI basics](terminal-ui-basics.md#scroll-position-and-scrollbar) for scrollbar interaction.

## Backspace and Delete

| Key | Default | Values |
|---|---|---|
| `backspace_binding` | `"ascii-delete"` | `ascii-delete` (`0x7f`) / `control-h` (`0x08`) |
| `delete_binding` | `"delete-sequence"` | `delete-sequence` (`ESC [3~`) / `ascii-del` (`0x7f`) / `control-h` (`0x08`) |

- The defaults match GNOME Terminal: Backspace sends `0x7f`, Delete sends `ESC [3~`.
- Only the **bytes sent to the PTY** change; Slint/winit key recognition is unchanged. An Alt modifier still adds the ESC prefix, so `Alt+Backspace` sends `ESC` followed by `0x08`.
- Special keys take precedence over the kitty keyboard CSI-u protocol: Backspace and Delete always use the configured bytes.
- Config parsing is case- and whitespace-insensitive. Unknown values fall back to the default deterministically, so a typo cannot change key behavior, and the value is rewritten as a canonical string.
- Changes apply immediately: every keypress reads `Config::key_bindings()`, so no restart or pane rebuild is required.

## How changes apply

- Changing a value in **Settings → Terminal** writes `config.toml` and applies the value without restarting.
- Audible and visual bell, both scroll policies, the scrollbar policy, and the Backspace/Delete bindings update existing panes immediately.
- `scrollback_unlimited` and scrollback capacity affect only new panes. Changing them does not resize the history buffer of a pane that is already open.
- A valid external edit to `config.toml` is also detected and applied while the app is running. An invalid or partial file is ignored and the current settings remain active.
- Theme files are different: adding or editing a file under `~/.config/xtervo/themes/` requires a restart. See [Themes](themes.md#custom-theme-directory).
