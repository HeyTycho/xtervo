# Logging

xtervo duplicates its stderr into a per-run log file. Panics, window/input warnings, SSH and SFTP events, and renderer detection all land there, so the log is usually the first place to look when the UI misbehaves.

## Where logs go

| Item | Default |
| --- | --- |
| Log directory | `~/.local/state/xtervo/logs/` |
| With `XDG_STATE_HOME` | `$XDG_STATE_HOME/xtervo/logs/` |
| Override | `XTERVO_LOG_DIR=/path/to/logs` |
| File name | `xtervo-<epoch seconds>.log` |
| Retention | the newest 8 files |
| File logging off | `XTERVO_LOG=0` |

Each launch names its log file after the current Unix time in seconds. At startup, older `xtervo-*.log` files beyond the newest eight are deleted; there is no size-based rotation. A very chatty session can therefore produce a large file.

`XTERVO_LOG_DIR` is useful for tests and for separating multiple instances. `XTERVO_LOG=0` disables file logging, but panics are still printed to stderr.

## What is captured

- Everything the process writes to stderr is copied to both the original stderr and the log file. This includes normal diagnostics, warnings, SSH/SFTP messages, and panic output.
- The log starts with a header containing the xtervo version, start epoch, PID, command-line arguments, selected environment values (`DISPLAY`, Wayland variables, session type, desktop, `SLINT_BACKEND`, `TERM`, `SHELL`, `XTERVO_CONFIG`, `XTERVO_PROFILES`), and a platform hint.
- Lines are written as produced. There is no per-line timestamp; use the start header, the file mtime, and event-specific fields to correlate.
- Renderer discovery logs the requested backend and the backend that was actually confirmed. If a GPU backend cannot be confirmed, xtervo can fall back to the software renderer; that fallback may restart the process, so check the newest log file.

There is no user-facing log-level setting. All stderr output is captured; more detail is enabled with the opt-in switches below.

## Panics

The panic hook captures a backtrace with `Backtrace::force_capture()` and writes it to the same log and to stderr:

```text
[panic] thread 'main' panicked at ...:
backtrace:
   0: ...
```

A panic is visible even when `XTERVO_LOG=0`, because the hook still prints to stderr. Backtraces contain symbol names and local source paths.

## One-shot diagnosis: `xtervo --diagnose`

```bash
xtervo --diagnose
```

This does not start the UI. It prints:

- version, PID, display/Wayland/session values, and the requested backend;
- whether `/dev/dri` exists;
- config, profiles, Vault, and log paths (Vault status only, never credentials);
- the inferred UI backend and `xdg_app_id`;
- X11 screen size, monitors, work area, and pointer position when available;
- the active window (PID, size, position, title), this process's windows, and other visible xtervo windows that could be covering clicks;
- the last 40 lines of the current log.

It is the quickest way to diagnose "right-click does nothing", focus problems, or an old full-screen xtervo window sitting over the new one.

## Reading the log

### Renderer detection

Look for the requested and actual renderer:

```text
[render] requested_backend=...
[render] actual_renderer=...
[render] GL_VENDOR="..."
[render] GL_RENDERER="..."
```

`actual_renderer=skia-opengl` with real `GL_RENDERER` means the GPU/OpenGL path was confirmed. If `GL_RENDERER` contains `llvmpipe` or `softpipe`, that is CPU rasterization and must not be reported as GPU performance. `actual_renderer=skia-software-surface` means Skia has no GL context and may show a black window; the automatic fallback to `winit-software` runs shortly after startup and appears in the newest log file.

There is no `/dev/dri` in headless/CI environments, so GPU behavior cannot be validated there. Test scripts should set `SLINT_BACKEND=winit-skia` explicitly and should not present llvmpipe numbers as NVIDIA results.

### Right-click and focus problems

1. Right-click in the xtervo window and search the log for:

   ```text
   [trace] pane-down-right pane=... at ... win_pos=... win_size=...
   [ui] right click pane=... at ...
   [trace] pane-up-right ...
   ```

   - `[trace] pane-down-right` present: the X11 event reached xtervo. If no menu appeared, the problem is UI routing or overlay state; look for `settings open=` or `menu dismissed`.
   - No `[trace]` line: the event did not reach xtervo. Run `xtervo --diagnose` and inspect `active_window` and the "other xtervo instance windows" section. A stale full-screen window or a click landing in another application is a common cause.

2. Right-click always opens the xtervo menu first. `Shift+right-click` forwards the event to a TUI such as tmux or vim that has mouse reporting enabled. Old logs that show only `[trace] pane-down-right` and no `[ui] right click pane=` come from the previous forwarding behavior.

3. Startup warns when another visible xtervo window may be covering clicks. Close the old instance instead of restarting the new one.

## Useful diagnostic switches

Set these in the environment before launching xtervo:

| Variable | Effect |
| --- | --- |
| `SLINT_BACKEND` | Force a backend, for example `winit-skia` or `winit-software`; unset uses GPU when `/dev/dri` exists, otherwise software |
| `XTERVO_RENDER_PROFILE=1` | Per-frame pane draw, image handoff, and Skia composition timings, including `full/rows/copy/full_redraws` |
| `XTERVO_RESIZE_PROFILE=1` | Resize apply count/time, background hide/restore, resize summary |
| `XTERVO_RESIZE_KEEP_BG=1` | Comparison test: keep a large background image visible during resize |
| `XTERVO_FORCE_GPU=1` | Test-only: use the GPU/Skia path even without `/dev/dri` |
| `XTERVO_FALLBACK_FONTS` | Colon-separated extra fallback fonts (optional `#face_index`) |
| `XTERVO_FONTLOG=1` | Log each fallback font's load time/failure, useful for missing glyphs or first-use CJK/emoji stalls |

These switches add detail to the same log. Remove them after collecting evidence; `XTERVO_FORCE_GPU=1` on unsupported hardware is not a supported configuration.

## What to include in a bug report

Please attach:

- `xtervo --version` output, distribution, desktop environment, and whether the session is X11, Wayland, or XWayland;
- the complete `xtervo --diagnose` output;
- the `xtervo-*.log` file from the failing run (or the relevant `[trace]`, `[ui]`, `[panic]`, `[render]`, and connection sections);
- steps to reproduce, expected behavior, and actual behavior;
- for rendering/residue problems, a doctor case or snapshot: run `xtervo diag env --json`, `xtervo diag snapshot --json`, and `xtervo diag doctor --issue` (or the MCP equivalents) as described in [Diagnostics](diagnostics.md);
- whether tmux/vim/another TUI with mouse reporting was active, and whether more than one xtervo instance was running;
- whether the session used the GPU or software renderer, if known.

Issues that reproduce reliably are fixed first.

## Redaction and privacy

The log is plain text and is not a sanitized support bundle. It normally contains:

- the command line used to start xtervo;
- selected environment values from the startup header;
- config, profile, Vault, and log paths;
- SSH host/user names, key paths, ProxyJump and forwarding arguments (the password itself is not on the command line);
- SFTP local/remote paths and error text;
- window titles, which may contain document or directory names;
- terminal dimensions, renderer information, and event metadata.

By design, Vault passwords and key passphrases do not appear in the log. Vault messages record lengths and booleans, askpass messages record whether it was enabled and which environment keys were set, and error messages do not echo the master password. The optional `XTERVO_ASKPASS_DEBUG` file is separate from the application log; it records the server's prompt text (usually user/host) but never the password.

Review and trim the file before posting it publicly. Do not attach `vault.enc`, `profiles.toml`, `config.toml`, the askpass debug file, or raw PTY recordings unless you have reviewed them; these files can contain hosts, users, paths, tokens, or terminal output even though they contain no Vault secrets.

---

Chinese reference: [docs/LOGGING.md](https://github.com/HeyTycho/xtervo/blob/main/docs/LOGGING.md).
