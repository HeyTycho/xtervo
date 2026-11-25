# Context menu and find

Right-clicking a pane opens the terminal context menu. The find bar searches the active pane from the keyboard. Both operate on the pane without restarting its shell or PTY.

## Context menu

The default menu contains these terminal actions. Plugin commands and profile entries may add more items.

| Item | Action | Default shortcut |
|---|---|---|
| Copy | Copy the current selection to the clipboard | `Ctrl+Shift+C` |
| Paste | Paste the clipboard at the cursor | `Ctrl+Shift+V` |
| Find | Open the find bar for the focused pane | `Ctrl+Shift+F` |
| Select All | Select the visible screen | none |
| Read Only | Toggle input blocking for the pane; the label ends in `: ON` or `: OFF` | none |
| Reset | Reset terminal state and clear the screen; the current upstream also clears saved lines | none |
| Reset and Clear | Reset and additionally clear saved lines explicitly | none |
| Save Contents… | Open the save dialog for a text snapshot | none |

The menu also includes link actions (`Open Link`, `Copy Link`) when the pointer is over a URL or file path, pane split and close actions, Broadcast Input, Clear, Profiles, and zoom controls.

### Copy and paste

- **Copy** copies the current selection. If nothing is selected, there is nothing to copy.
- **Paste** inserts the clipboard at the cursor. It follows the normal paste rules: bracketed paste is used when the terminal has enabled it, and multi-line paste asks for confirmation by default (`confirm_multiline_paste = true`).
- The search field has its own copy and paste path. Its `Ctrl+C` / `Ctrl+V` shortcuts act only while the find field is focused and do not trigger terminal copy or paste.

## Find

### Open and close

- Open the find bar with `Ctrl+Shift+F` (the default binding for `terminal.find`), the context menu item **Find**, or the command palette.
- The query field is a real text input and receives focus when the bar opens, so you can type immediately.
- The last query and the Match case / Match regex options are preserved. Reopening the bar restores them and runs the search again.
- `Esc` closes the bar, clears the search highlight, and returns keyboard and IME focus to the terminal. The IME candidate-window anchor is marked dirty so the next poll re-sends it.
- Clicking the terminal also closes the find bar before the click is handled as a terminal click.
- A background `TouchArea` consumes clicks on empty parts of the bar, so they do not fall through to the terminal or start a selection.

### Query and options

- Both **Match case** and **Match regex** are off by default. Click either toggle to change it. With the query field focused, `Alt+C` toggles Match case and `Alt+R` toggles Match regex.
- In plain-text mode, regex metacharacters in the query are escaped first, so `.` and `*` are literal characters.
- In regex mode, the query is passed to alacritty's `RegexSearch`.
- Case handling is controlled only by the Match case toggle. The engine wraps the query in an inline group: `(?i:<query>)` when Match case is off and `(?-i:<query>)` when it is on.
- If the options change while a query is present, the search runs again from the current position and clears the old match origin.

### Navigation and match count

- `Enter` moves to the next match; `Shift+Enter` moves to the previous match.
- The next search starts one cell past the end of the current match (forward) or past its start (backward), and wraps at the top or bottom of the grid.
- The status area on the right shows `current/total`, where `current` is the 1-based index of the current hit and `total` is the number of matches in scrollback plus the viewport. With no match it shows `0/0`.
- The count and the hit are produced by the same core search request, so they use the same grid, line-wrapping, wide-character, and case rules.
- Only the current match is highlighted. Other matches counted in `total` are not drawn as a separate highlight layer and cannot be clicked directly.

### Errors

- An invalid regular expression does not clear the query. It clears the old highlight, shows `invalid regex` in the status area, and draws a red border around the search bar. The full error is written to the log.
- A core search reply is capped at 200 ms. If the reply times out, the UI treats it as no hit and shows `0/0`.

### Field editing

The query field supports normal single-line editing through Slint's `TextInput`:

- Click to place the caret; drag to select.
- `Ctrl+A` selects all text in the field.
- `Ctrl+C` / `Ctrl+V` copy and paste within the field.
- Typing, Backspace, paste, and mouse insertion all re-run the incremental search through the same `edited` callback.

These shortcuts apply only while the field is focused; they do not reach the terminal.

## Read Only

Read Only is a per-pane input gate.

- Enable or disable it from the context menu or with the command `terminal.toggle-read-only`. The menu label shows the current state as `Read Only: ON` or `Read Only: OFF`.
- While enabled, a `READ ONLY` badge appears in the top-right corner of the pane, the menu item reflects the state, and a notice appears at the bottom for four seconds.
- Keyboard input is blocked before encoding and again in the core. Pasting and drag-and-drop insertion are blocked at the app layer, and the core is the final gate. Both layers are non-blocking.
- Input that is already queued but not yet written is discarded when the pane switches to read-only.
- `wants_mouse()` and `wants_motion()` return false while read-only, and mouse events are dropped. Local selection and wheel scrolling still work, but no mouse reports are sent to a TUI. Focus events are also blocked.
- Turning Read Only off restores keyboard and paste input immediately.
- The state is per pane and is not persisted across tabs or sessions. New panes start with Read Only off.
- The command acts on the focused pane; the context menu acts on the pane that was right-clicked.

## Reset and Reset and Clear

Both commands reset the terminal state through alacritty's `Handler::reset_state()`: modes, cursor, character sets, scrolling region, title stack, both grids, and the screen. Selection and search highlights are cleared, the full frame is redrawn, and mode bits, cursor, and scroll snapshot are refreshed. `Reset and Clear` additionally sends `clear_screen(ClearMode::Saved)`.

- `Reset` and `Reset and Clear` currently have the same observable effect on scrollback: upstream `reset_state()` calls `clear_history()`, so both clear saved lines. The two commands remain separate so they can diverge if upstream adds a reset that preserves history.
- The session and PTY are not closed. Input works normally after either command.
- Commands: `terminal.reset` and `terminal.reset-and-clear`. Neither has a default shortcut, so they do not take keys from the terminal.
- Notices: `Terminal reset` and `Terminal reset and cleared`.

## Save Contents

**Save Contents…** (command `terminal.save-contents`) opens a centered dialog with an explicit output path.

- The default path is `$HOME/xtervo-save-<epoch>.txt`; without `HOME` it falls back to `/tmp/xtervo-save-<epoch>.txt`.
- The path is editable. `~` is expanded, and a relative path is resolved against the process working directory. The dialog shows a full-path preview below the input.
- The saved text comes from the core `PaneHandle::contents()` snapshot. It covers **scrollback plus the viewport**, not just the visible frame, and ends with a newline.
- Saving is a synchronous user action and the core read is capped at 500 ms. Writing happens only when you press Save or Enter; normal output is not affected.
- On failure, the dialog stays open and shows the I/O error in red text. On success, the dialog closes and a bottom notice reports `Saved N bytes to <path>`.
- There is no overwrite confirmation: an explicit path is treated as the user's intent.
- This is not a system file picker. Slint 1.18 has no native file dialog on the winit backend, so the dialog uses an explicit path field instead.

## Behavior notes

- The find count is computed by a full scan of scrollback plus the viewport for each search. This is cheap at the default 2,000-line scrollback, but a very large scrollback or a high input rate can add UI-thread work.
- Only the current match is highlighted; there is no all-matches highlight.
- Chinese IME composition inside the find field relies on Slint's `TextInput`. Focus restoration to the terminal after closing the bar is verified; composition inside the field has not been specifically tested with fcitx or ibus.
- Read Only does not persist, and it does not disable local selection or scrolling.
- Save Contents uses an explicit path and does not add a second overwrite confirmation.
