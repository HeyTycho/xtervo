# Terminal UI basics

This page covers the terminal's scroll position and scrollbar, the visual bell, file drag-and-drop, and transient status notices.

## Scroll position and scrollbar

The terminal exposes its scroll state as a small snapshot:

- `offset` is the distance from the bottom: `0` means the bottom (newest content), and `offset = history` means the top (oldest content).
- `history` is the number of scrollback lines, excluding the viewport.
- `rows` is the number of rows in the viewport.

The UI reads an atomic snapshot, so reading the scroll position never blocks the UI or the PTY. The snapshot is refreshed after output, wheel scrolling, search jumps, resizes (history can shrink), and screen clears. Absolute scrolling is clamped to `0..=history`.

The scrollback capacity behind this position is controlled by `scrollback` and `scrollback_unlimited`; see [Terminal behavior](terminal-behavior.md#scrollback).

### Scrollbar

The scrollbar is a right-edge overlay drawn over the terminal grid:

- The track is the pixel height of the terminal grid and has a transparent background. The thumb is 5 px wide, rounded, and changes to the accent color on hover or press.
- The thumb has a minimum height of 24 px so it remains usable in a long history.
- The overlay is 9 px wide and sits just inside the outer 5 px of the window, which remains the resize hit band. The scrollbar does not take that area.
- Under the `auto` policy the scrollbar is visible only when `history > 0`. `always` and `never` behave as described in [Terminal behavior](terminal-behavior.md#scrollbar-policy).
- Clicking or dragging maps the normalized track position to a scroll offset. The thumb center follows the pointer: clicking the top jumps to the oldest content, and clicking the bottom returns to the newest.
- The scrollbar does not consume terminal columns, change the pane size, or trigger a PTY resize. Output, scrolling, resizing, and bell flashes refresh the model.

## Visual bell

The visual bell flashes the pane border when the terminal receives BEL.

- `visual_bell` is enabled by default (`visual_bell = true`); older config files that omit the field also fall back to enabled. The switch is in **Settings → Terminal**.
- When `CoreEvent::Bell` arrives for a pane in the **active tab**, the pane border is highlighted for about 180 ms. A 30 ms repeat timer clears the state and stops itself when no flash is active.
- The overlay is a transparent-background 2 px accent border. Its opacity animates over 160 ms and its border width over 140 ms with an ease-out curve.
- The overlay does not cover terminal content or intercept input. A single bell frame does not change the scroll position, and scrollbar, selection, and mouse-reporting behavior are unaffected.
- Turning `visual_bell` off clears an in-progress flash immediately and stops the timer.
- A BEL in a background tab does not flash. Instead it sets the tab's unread indicator and, when `desktop_notifications = true` (the default), sends a desktop notification through `notify-send`.
- The visual bell is independent of the audible bell. You can enable either one, both, or neither. See [Terminal behavior](terminal-behavior.md#audible-bell).

Limitations:

- The visual bell does not follow the system reduced-motion setting yet.
- `visual_bell` is only an on/off switch; flash duration and color are not configurable.

## Drag and drop

You can drag files and directories from a file manager onto a pane.

### Insertion rules

- Each dropped path is escaped for the shell with single quotes: `'` becomes `'\''`, and every other character is kept literally. Spaces, `$`, `;`, backticks, `*`, `~`, and double quotes are not expanded. An empty path becomes `''`.
- Multiple paths are separated by spaces, and the inserted text ends with one trailing space so you can continue typing arguments.
- If the terminal has enabled bracketed paste, the insertion is wrapped in `ESC[200~` / `ESC[201~`; otherwise it is inserted as plain text.
- The text is only inserted. No Enter is sent and no command is executed automatically. xtervo does not filter "dangerous" commands; bracketed paste does not change that.
- winit delivers one `DroppedFile` event per file, so each path is inserted once. The final command line still looks like `'a b' 'c' `.

### Hover and targeting

- While a file is dragged over the window, the pane under the pointer gets an accent hover border.
- On X11, xtervo uses `WinitWindowAccessor::on_winit_window_event` to consume `HoveredFile`, `HoveredFileCancelled`, and `DroppedFile`; XDND negotiation and the local path are provided by winit. The pointer position is converted from X11 window coordinates to Slint logical coordinates to find the pane.
- The drop re-tests the pointer position and inserts into the pane under the pointer, then focuses that pane.
- On non-X11 platforms the native bridge returns no pane and the drop falls back to the currently focused pane.
- If the drag crosses several panes, hover highlighting stays on the first pane because winit does not expose XdndPosition coordinates. The final drop still re-tests the pointer, so the insertion lands in the correct pane.

### Platform notes

- Native file drag-and-drop currently works on X11 only.
- Slint 1.18.1 has a `DropArea`, but `i-slint-backend-winit` does not translate winit's `HoveredFile` / `DroppedFile` events into Slint `DragMove` / `Drop` / `DataTransfer`. The native bridge above fills that gap.
- On native Wayland, winit does not guarantee file-drop delivery, so drag-and-drop is not supported. If a drop is delivered, it goes to the focused pane.
- Dropping onto a read-only pane is ignored and produces a notice.

## Status and notices

- A transient notice appears at the bottom of the window and clears automatically after four seconds. Read Only changes, Save Contents results, and link-related messages use this path.
- The notice is informational; it does not take focus or block input.
- The `READ ONLY` badge in the top-right corner of a pane is persistent while Read Only is enabled. The context-menu label and the transient notice provide the same state.
- Save Contents reports write errors in red inside the dialog, and reports success with `Saved N bytes to <path>` after the dialog closes.
- An inactive tab that receives BEL gets an unread indicator; with `desktop_notifications = true` it also triggers a desktop notification. The active tab uses the visual bell instead.

## Known limitations

- The visual bell does not follow reduced motion and has no duration or color configuration.
- The scrollbar is a 9 px overlay, and the outer 5 px of the right edge remains reserved for window resizing. In a very narrow pane the minimum thumb can fill the whole track.
- Native file drag-and-drop is X11-only. On Wayland, winit does not provide the events needed for the bridge, and the Slint `DropArea` path does not receive external `DataTransfer` events in the pinned winit backend.
- Hover highlighting does not track a drag across panes in real time; the final drop position is still correct.
- Scrollbar interaction never introduces a new layout or PTY resize path.
