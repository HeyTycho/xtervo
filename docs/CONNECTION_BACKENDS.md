# Connection backends (native vs fallback)

Tervo's default SSH/serial paths are in-process libraries; system tools remain
explicit fallbacks for configurations the native stack does not cover yet.

## SSH

| `Profile.transport` | Backend | Notes |
|---|---|---|
| empty / `native` | `russh` in-process | Password (Vault), unencrypted key, or ssh-agent. `known_hosts` entry required. |
| `system` | system `ssh` in a PTY | Full OpenSSH config, ProxyJump, forwards, extra args. |
| `plugin` | `ssh_plugin.rs` sidecar | POC wrapper around system `ssh`. |

Native SSH now supports ProxyJump (including multi-hop), local/dynamic/remote
port forwarding, and agent forwarding. It uses a pure-Rust parser for
`~/.ssh/config` and `known_hosts` (including hashed hosts, wildcards, `!`
negation and `@revoked`); it no longer invokes `ssh -G` or `ssh-keygen -F`.

It still falls back to `system` for `extra_args`, `ProxyCommand`, `Match`
blocks, `IdentityFile` in ssh_config, a password not saved in Vault, or a
known_hosts entry that is missing/revoked. Encrypted private keys use the
Vault-stored passphrase from the Profile form. Native requests a PTY, starts
the shell, forwards window-size changes, and keeps jump sessions alive for the
PTY's lifetime.

## Serial

| `Profile.transport` | Backend | Notes |
|---|---|---|
| empty / `native` | `serialport` in-process | 5-8 data bits, none/even/odd parity, 1/2 stop bits, none/software/hardware flow control. |
| `socat` | external `socat` PTY bridge | 8N1 only; retained as compatibility/fallback. |

The Profile form lists detected ports (`serialport::available_ports`) with a
Refresh action; a 1.5s timer refreshes the list while the form is open, so
plug/unplug is visible without restarting. DTR and RTS can be set per profile
and are applied when the port opens.

If native serial open fails (permissions/unsupported device), Tervo logs the
error and falls back to the socat command only for 8N1/no-flow profiles.
Control-line *status readback* (CTS/DSR/DCD) is not exposed yet.

## Packaging

The `.deb` packaging should eventually declare `openssh-client`/`socat` only
for users who rely on the fallbacks. The native build does not require either
at runtime for simple SSH/serial profiles.

## Verification (2026-11-19)

- `cargo test -p tervo-app --bin tervo`: 710 passed, 0 failed, 2 ignored.
  - `ssh_native::integration_tests::native_ssh_connects_password_and_echoes`:
    in-process russh server + password auth + temp known_hosts + PTY/shell + echo.
  - `serial_native::pty_tests::native_serial_roundtrips_through_a_pty`:
    openpty, real `TimedOut` retry, two-way bytes.
- Release e2e: TabBar, keyboard protocol, tab pin/drag, single-instance multi-window.
- 15m `SOAK_STRICT=1`: PASS, RSS 576.6→671.3 MiB (+4.44 MiB/min, R²=0.32),
  threads 111→111, fds 121→121, fatal=0.
- Installed `~/.local/bin/tervo` sha256
  `d1ec335658c242b5aa7d55b1c39c305e2912ca3ff6f2856c9297d6dbd3737ba5`.

The native SSH path is now fully in-process for config parsing, known_hosts
lookup, authentication, transport, PTY, shell, forwarding and agent channels.
`ssh-keygen` is only used in unit tests to generate temporary test keys.
