# Keyboard shortcuts

The default bindings follow GNOME Terminal conventions. Every binding can be changed
in **Settings → Keys** (gear icon in the tab bar). Shortcuts use `Ctrl+Shift` where a
plain `Ctrl` chord is needed by the shell.

## Default shortcuts

| Action | Shortcut |
|---|---|
| Command palette | `Ctrl+Shift+P` |
| New / close tab | `Ctrl+Shift+T` / `Ctrl+Shift+W` (`Ctrl+W` is left to the shell for word deletion) |
| Previous / next tab | `Ctrl+PageUp` / `Ctrl+PageDown` |
| Move tab left / right | `Ctrl+Shift+PageUp` / `Ctrl+Shift+PageDown` |
| Tabs 1–10 | `Alt+1` … `Alt+9` / `Alt+0` |
| Split right / down | `Ctrl+Shift+E` / `Ctrl+Shift+D` |
| Copy / paste | `Ctrl+Shift+C` / `Ctrl+Shift+V` |
| Find | `Ctrl+Shift+F` |
| Sidebar | `Ctrl+Shift+B` |
| Zoom / restore pane | `Ctrl+Shift+M` |
| Zoom in/out / reset | `Ctrl+=` `Ctrl+-` / `Ctrl+0` |
| Fullscreen | `F11` |

Additional default bindings:

| Action | Shortcut |
|---|---|
| New / close window | `Ctrl+Shift+N` / `Ctrl+Shift+Q` |
| Search tabs | `Ctrl+Shift+A` |
| Last used tab | `Ctrl+Shift+L` |
| Rename tab / undo rename | `F2` / `Ctrl+Shift+Z` |
| Reopen closed tab | `Ctrl+Alt+Shift+T` |
| Close pane | `Ctrl+Shift+X` |
| Focus pane left / right / up / down | `Ctrl+Shift+←` / `Ctrl+Shift+→` / `Ctrl+Shift+↑` / `Ctrl+Shift+↓` |
| Close other tabs | `Ctrl+Alt+W` |
| Close tabs to the left / right | `Ctrl+Alt+←` / `Ctrl+Alt+→` |
| Scroll to top / bottom | `Ctrl+Shift+Home` / `Ctrl+Shift+End` |
| Next / previous tab (alternate) | `Ctrl+Tab` / `Ctrl+Shift+Tab` |

Some desktops reserve combinations such as `Ctrl+Alt+←` / `Ctrl+Alt+→` for workspace
switching; rebind those rows in Settings → Keys if they conflict.

## Tabs

- `Ctrl+Shift+T` opens a new tab with your default profile; `Ctrl+Shift+W` closes the
  current tab.
- `Ctrl+PageUp` / `Ctrl+PageDown` move between tabs. `Ctrl+Tab` / `Ctrl+Shift+Tab` do
  the same.
- `Alt+1` … `Alt+9` select tabs 1–9; `Alt+0` selects tab 10.
- `Ctrl+Shift+PageUp` / `Ctrl+Shift+PageDown` move the current tab left or right within
  its group (pinned or unpinned).
- `Ctrl+Shift+L` returns to the last used tab, `Ctrl+Shift+A` opens tab search, and
  `F2` renames the current tab.
- Tabs can be pinned to the left pinned area, but they cannot be reordered by dragging
  and cannot be moved between windows. Use the move shortcuts instead; see
  [Known limitations](known-limitations.md).

## Splits

- `Ctrl+Shift+E` splits the active pane to the right; `Ctrl+Shift+D` splits it down.
- `Ctrl+Shift+←` / `Ctrl+Shift+→` / `Ctrl+Shift+↑` / `Ctrl+Shift+↓` move focus to the
  nearest pane in that direction.
- `Ctrl+Shift+M` zooms the focused pane or restores it. Double-clicking the pane handle
  does the same.
- `Ctrl+Shift+X` closes the focused pane.
- Drag the divider between panes to resize them. Window edges can also be dragged to
  resize the window.

## Mouse

- Drag to select text. Double-click selects a word; triple-click selects a line.
- Selection copies automatically by default (`copy_on_select = true`).
  `Ctrl+Shift+C` copies explicitly.
- Middle-click pastes the PRIMARY selection, falling back to the clipboard when PRIMARY
  is empty (`paste_on_middle_click = true`).
- `Ctrl+click` opens URLs and file paths under the pointer.
- Right-click opens the context menu.
- The mouse wheel scrolls through scrollback (2000 lines by default; `--scrollback`
  changes it).

## Rebinding in Settings → Keys

1. Open **Settings** with the gear icon in the tab bar, then select **Keys**.
2. Find the action and click **Edit** on its row.
3. Press the new chord. A chord needs `Ctrl`, `Alt`, or `Meta`, or an `F1`–`F24` key;
   `Shift` can be part of it. Click **Cancel** or press `Esc` to discard.
4. Press `Backspace` to restore that row's default. **Reset All** restores every
   binding.
5. Turn on **2-chord** to record a sequence such as `Ctrl+K Ctrl+T`. In that mode,
   press `Enter` after the first chord to save a single-chord binding.

Conflicts with other bindings, including a binding that is a prefix of another, are
rejected and marked `CONFLICT`; overridden defaults are marked `CUSTOM`. Chords reserved
by the desktop are saved with a warning. Bindings are stored in `config.toml`.

## Chords left to the shell

`Ctrl+W`, `Ctrl+T`, and `Ctrl+F` are not captured by xtervo:

| Chord | Shell action |
|---|---|
| `Ctrl+W` | Delete the previous word |
| `Ctrl+T` | Transpose characters |
| `Ctrl+F` | Move forward one character (readline) |

This is why tab, find, and close use `Ctrl+Shift` variants. `Ctrl+C` is also sent to the
shell as `SIGINT`; copy and paste use `Ctrl+Shift+C` and `Ctrl+Shift+V`.
