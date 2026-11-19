# Connection backends (native vs fallback)

Tervo's default SSH/serial paths are in-process libraries; system tools remain
explicit fallbacks for configurations the native stack does not cover yet.

## SSH

| `Profile.transport` | Backend | Notes |
|---|---|---|
| empty / `native` | `russh` in-process | Password (Vault), unencrypted key, or ssh-agent. `known_hosts` entry required. |
| `system` | system `ssh` in a PTY | Full OpenSSH config, ProxyJump, forwards, extra args. |
| `plugin` | `ssh_plugin.rs` sidecar | POC wrapper around system `ssh`. |

Native SSH automatically falls back to `system` when the profile uses
ProxyJump, local/remote/dynamic forwards, agent forwarding, `extra_args`, an
encrypted private key, a password not saved in Vault, or has no known_hosts
entry. Native connects directly to the host, requests a PTY, starts the shell,
and forwards window-size changes.

## Serial

| `Profile.transport` | Backend | Notes |
|---|---|---|
| empty / `native` | `serialport` in-process | 5-8 data bits, none/even/odd parity, 1/2 stop bits, none/software/hardware flow control. |
| `socat` | external `socat` PTY bridge | 8N1 only; retained as compatibility/fallback. |

If native serial open fails (permissions/unsupported device), Tervo logs the
error and falls back to the socat command. Port enumeration/hotplug and
DTR/RTS/control-line status are not implemented yet.

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

Residual external tools in the native SSH path: `ssh -G` (effective
host/port/user/known_hosts paths) and `ssh-keygen -F` (local known_hosts
lookup). Authentication, transport, PTY, shell and data are in-process russh.
