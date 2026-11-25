# FAQ

## Which Linux distributions are supported?

xtervo has only been tested on **Ubuntu 22.04 and 24.04**. Other distributions and
desktop environments are unverified. Release names distinguish x86_64 (`amd64` in the
`.deb`) and aarch64 (`arm64`); check the release assets for your architecture. The
`.deb` targets Debian/Ubuntu; the tarball is the alternative on other distributions,
but it has not been tested there. Reports from other systems are welcome in
[Issues](https://github.com/HeyTycho/xtervo/issues); fixes are scheduled by
reproducibility.

## How do I install, upgrade, or roll back xtervo?

- **Install:** download `xtervo-<version>-linux-x86_64.tar.gz` or the `.deb` from
  [Releases](https://github.com/HeyTycho/xtervo/releases), verify `SHA256SUMS`, then run
  the tarball's `install.sh` (default prefix `~/.local`) or
  `sudo apt install ./xtervo_<version>-1_amd64.deb`. Full steps are in
  [Installation](installation.md).
- **Upgrade:** run the new tarball's `install.sh` with the same prefix (the previous
  binary is kept for rollback), or install the new `.deb`. Configuration is untouched.
- **Roll back:** run
  `~/.local/lib/xtervo/install.sh --rollback --prefix ~/.local`. Running it again
  switches forward. `.deb` installs have no previous-version slot; install the earlier
  `.deb` to downgrade.

## Is there automatic updating?

No. Auto-update, crash reporting/upload, AppImage/Flatpak/Snap, release signing, and an
APT repository are not implemented. Check
[Releases](https://github.com/HeyTycho/xtervo/releases) or
[CHANGELOG.md](https://github.com/HeyTycho/xtervo/blob/main/CHANGELOG.md) and upgrade
manually as described above. See also
[Known limitations](known-limitations.md#packaging-and-updates).

## Does xtervo work on Wayland?

Yes. Wayland sessions are supported through XWayland (the documented runtime path), and
native Wayland has additional limitations: clients cannot set global window positions,
so `set_position` is a no-op and Quake mode is unavailable; native file drag-and-drop is
unsupported; clipboard access needs `ext-/wlr-data-control`; and session snapshots store
only the window size. See [Known limitations](known-limitations.md) for details.

## How do I use fcitx5 or another input method?

X11 uses winit's XIM (fcitx5 / ibus); native Wayland uses text-input-v3. Chinese input
has been tested with GNOME 46 + fcitx5 (pinyin). Other compositors and IMEs still need
matrix testing. When reporting an IME problem, include the compositor, IME and version,
whether the session is X11 or Wayland, and the relevant logs.

## Why doesn't copy/paste work on Wayland?

Clipboard and PRIMARY work on X11 and on "Wayland window + XWayland". Pure Wayland needs
the compositor to provide `ext-/wlr-data-control`, which GNOME/Mutter 46 does not. When
the operation fails, xtervo prints a readable reason once. Native Wayland clipboard
support therefore depends on the compositor.

## Is xtervo using the GPU or software rendering?

With `/dev/dri`, xtervo uses the GPU (Skia). Without DRM, or if no GPU API is confirmed
within 1s, it falls back to `winit-software`. This is automatic and needs no
configuration. The startup log reports the actual renderer
(`[render] actual_renderer=...`). To force a backend for testing:

```bash
SLINT_BACKEND=winit-software xtervo   # software
SLINT_BACKEND=winit-skia xtervo       # GPU/Skia path
```

## Are plugins sandboxed?

No. Process plugins run external programs with the current user's privileges and can
read or write any file that user can access, so only install trusted plugins. WASM
output filters are narrower (no imports, with memory and fuel limits), but they can
still rewrite or delete terminal output. Declarative host permissions are for display
and gating; they are not OS-level isolation. See the
[security policy](https://github.com/HeyTycho/xtervo/blob/main/SECURITY.en.md),
[Plugins](plugins.md), and [Plugin API v3](plugin-api-v3.md).

## What is MCP, and is it enabled by default?

MCP is a local control and diagnostics bridge, not a plugin. It is enabled by default
but **read-only**: input, tab, and destructive operations require the corresponding
groups to be enabled in **Settings → Privacy → MCP live control**. The live socket is
`$XDG_RUNTIME_DIR/xtervo/mcp.sock` (0600), with a state token at
`~/.local/state/xtervo/mcp.token` (0600); other local processes running as the same UID
can access it. See [`docs/MCP.md`](https://github.com/HeyTycho/xtervo/blob/main/docs/MCP.md).

## Where are the logs?

Logs are written to `~/.local/state/xtervo/logs/` as `xtervo-<epoch seconds>.log`, one
file per launch, with the most recent 8 kept. `XTERVO_LOG_DIR` overrides the directory.
Run `xtervo --diagnose` to print the version, environment, config/log paths, and recent
log lines; `xtervo diag snapshot` emits JSON diagnostics. Include `xtervo --version`,
your distribution and desktop (X11/Wayland), reproduction steps, and the relevant log in
bug reports.

## How do I uninstall xtervo?

For a tarball install, run the installed lifecycle script:

```bash
~/.local/lib/xtervo/install.sh --uninstall --prefix ~/.local
```

Or, from an extracted tarball, `./install.sh --uninstall --prefix ~/.local`. For a
`.deb` install, use `sudo apt remove xtervo`. Uninstall removes only program files; it
preserves `~/.config/xtervo` (configuration, profiles, themes) and the logs. Remove
those directories manually if you also want to delete user data.

## Why can't I drag tabs?

Tab dragging is not implemented. Reorder tabs with `Ctrl+Shift+PageUp` /
`Ctrl+Shift+PageDown`; dragging a tab out of the window or between windows is also not
supported. Splits do support divider dragging and window-edge resizing. See
[Keyboard shortcuts](shortcuts.md) and
[Known limitations](known-limitations.md#tabs-and-splits).
