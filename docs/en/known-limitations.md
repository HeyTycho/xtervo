# Known limitations

These are the current known limitations. xtervo has only been tested on Ubuntu 22.04
and 24.04 so far; other distributions and desktops are unverified. For workarounds, see
[Keyboard shortcuts](shortcuts.md) and [FAQ](faq.md).

## Rendering and GPU

Terminal glyph rasterization is still CPU-based (fontdue), but uses double-buffered
persistent frames: `Image` is only a refcount clone, with no per-frame full
`SharedPixelBuffer` copy; Skia still uploads the frame as a texture before compositing.
With `/dev/dri`, xtervo uses Skia/GL by default. Without DRM, or if no GPU API is
confirmed within 1s, it falls back to `winit-software`. Real NVIDIA GPU
frame-rate and latency data is still waiting for user re-testing.

## Tabs and splits

Tabs cannot be reordered by dragging. Splits already support divider dragging and
window-edge resizing. To move a tab, use `Ctrl+Shift+PageUp` / `Ctrl+Shift+PageDown`.

## Drag and drop

Native file drag-and-drop bridging currently covers X11/winit only: Slint 1.18.1's
winit backend does not translate `HoveredFile`/`DroppedFile` into `DataTransfer`, and
winit 0.30.13's Wayland backend has no DnD implementation. Native Wayland explicitly
does not support it (no fake hover). The visual bell does not follow the system's
reduced-motion setting yet.

## Window position, size, and Quake mode

On X11, window position and size are managed by the app layer. Native Wayland does not
allow clients to set global positions, so `set_position` is a no-op and Quake mode is
unavailable (startup and Settings say so explicitly). Wayland session snapshots only
store the size (`position_known=false`), so they never pollute the next X11 restore
with `(0,0)`.

## Clipboard and PRIMARY

Clipboard and PRIMARY work on X11 and on "Wayland window + XWayland". Pure Wayland
needs the compositor to provide `ext-/wlr-data-control`, which GNOME/Mutter 46 does
not. A readable reason is printed once when the operation fails.

## Window icon

X11 uses the runtime `_NET_WM_ICON` property. On Wayland, the installed
`xtervo.desktop` is associated with `app_id=xtervo`, and no icon is written at runtime.

## Fonts and emoji

Fonts are fixed to JetBrains Mono (falling back to DejaVu); Chinese automatically falls
back to WenQuanYi Micro Hei / Droid / Noto CJK. Color emoji are enabled by default
(Noto Color Emoji CBDT/CBLC/sbix; disable with `XTERVO_COLOR_EMOJI=0`). Custom fonts and
ligatures are not supported yet; COLR/SVG color emoji still fall back to
monochrome/placeholder.

## Input methods (IME)

X11 uses winit's XIM (fcitx5 / ibus); native Wayland uses text-input-v3. Chinese input
has been tested with GNOME 46 + fcitx5 (pinyin); other compositors and IMEs still need
matrix testing. More Wayland limitations and verification details are in
[`docs/WAYLAND_ADAPTATION.md`](https://github.com/HeyTycho/xtervo/blob/main/docs/WAYLAND_ADAPTATION.md).

## Themes

- Following the system theme depends on GNOME `gsettings`; non-GNOME environments fall
  back to `gtk-theme` / `COLORFGBG` for a one-shot detection.
- Custom themes are still scanned at startup. Editing a theme file requires a restart;
  hot reload is not implemented.

## Settings

Settings currently has no font-family or line-height controls. Search jumps to the
settings card rather than to an individual control, and keyboard focus traversal
(Tab/arrows) is still mostly mouse/search driven. The legacy modal presents the same set
of controls in a different presentation; it is not the pre-2026-11 implementation.

## Session restore

Session snapshots restore layout, geometry, title, cwd, profile association, and pinned
tabs, but not scrollback, screen contents, or running processes. With
`restore_session_on_startup=true`, autosave runs every 2s; unclean exits are
detected via a running marker and restored. Session v2 stores
geometry/maximized/active_tab/tabs for all windows; a corrupt primary falls back to the
previous `.bak`. SSH/Serial/Telnet do not reconnect automatically.

## SSH, Serial, and Telnet profiles

These profiles are not reconnected by session restore. The user must open them
explicitly, which avoids making connections at startup. They are restored as a plain
shell with the tab title, layout, and cwd preserved.

## Tab pinning

Tab pin means "pin to the left pinned area", not "always on top". Order can still be
changed within the pinned group, but tabs cannot cross into the unpinned group.
Dragging a tab out of the window or between windows is not implemented.

## Profile appearance overrides

Profile appearance overrides are per-pane "pins": after a profile sets them, global
font-size, theme, and cursor changes do not refresh that pane. Clearing the profile
field and opening a new tab returns to inheritance.

## Packaging and updates

Auto-update, crash reporting/upload, AppImage/Flatpak/Snap, release signing, and an APT
repository are not implemented and have no buttons. Upgrades go through the new
tarball's `install.sh` or a new `.deb`; rollback uses `install.sh --rollback`. See
[Installation](installation.md) and
[`packaging/README.md`](https://github.com/HeyTycho/xtervo/blob/main/packaging/README.md).
