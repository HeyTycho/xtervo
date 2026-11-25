# SSH and SFTP

xtervo opens SSH sessions from saved connection profiles. SFTP uses the same profile for host, port, user, key and jump settings. Passwords and key passphrases can be kept in [Vault](vault.md).

SSH profiles have three backends:

- **Native** (default, `transport` unset or `native`): xtervo's in-process SSH client.
- **System ssh** (`transport = "system"`): the OpenSSH client, running inside a PTY.
- **Official SSH plugin POC** (`transport = "plugin"`): the system client wrapped as a transport plugin.

The native backend is used when it supports the profile. Profiles that use `extra_args`, `ProxyCommand`, X11 forwarding, custom ciphers/MACs/KEX, unsupported `ssh_config` options, a password that is not in Vault, or a missing/revoked `known_hosts` entry automatically use the system client instead. Selecting `transport = "system"` forces that path.

## Profiles and `profiles.toml` v2

Profiles are stored in `~/.config/xtervo/profiles.toml` (or `$XDG_CONFIG_HOME/xtervo/profiles.toml`; `XTERVO_PROFILES` overrides the path). The file is version 2. The SSH connection fields below are additive: an older file without them loads with empty strings, empty lists, or `false`, and no migration step is required.

| Field | Form label | Passed to SSH |
| --- | --- | --- |
| `proxy_jump` | Jump | `-J [user@]host[:port]`; comma-separated multi-hop is accepted |
| `remote_forwards` | R-Forwards | one `-R [bind:]port:host:port` per entry |
| `dynamic_forwards` | D-Forwards | one `-D [bind:]port` per entry (SOCKS) |
| `agent_forward` | Agent forwarding (-A) | `-A` |
| `local_forwards` | L-Forwards | one `-L port:host:port` per entry |
| `extra_args` | Extra | appended verbatim, before the host argument |

The core connection fields are `kind = "ssh"`, `host`, `port`, `user`, `auth` (`agent`, `key`, or `password`), `key_path`, and `transport`. Two booleans control Vault use: `save_password` and `save_key_passphrase`.

```toml
# profiles.toml
[[profiles]]
id = 1
name = "prod"
kind = "ssh"
host = "ssh.example.com"
port = 22
user = "deploy"
auth = "agent"
proxy_jump = "ops@bastion:2222"
agent_forward = true
local_forwards = ["8080:localhost:80"]
remote_forwards = ["9000:localhost:3000"]
dynamic_forwards = ["127.0.0.1:1080"]
```

Edit profiles from **Settings → Profiles** unless you need a field the form does not expose. `extra_args` can override `ssh_config` values (for example `["-o", "ServerAliveInterval=30"]`), but it also forces the system backend.

### Authentication

- `auth = "agent"` uses `SSH_AUTH_SOCK`. The native backend needs an agent or a loadable default key; otherwise it falls back to the system client.
- `auth = "key"` loads `key_path`. Encrypted keys can use a passphrase saved in Vault (`save_key_passphrase`); the native backend asks for it before connecting and can keep it in Vault under a separate entry.
- `auth = "password"` is password-only. On the system path xtervo adds:

  ```text
  -o PubkeyAuthentication=no
  -o PreferredAuthentications=password,keyboard-interactive
  -o KbdInteractiveAuthentication=yes
  ```

  This prevents ssh from silently succeeding with an agent or public key before the password prompt, which would bypass Vault and askpass. The native backend uses the Vault password directly; if the password is not saved, the profile uses the system client so ssh can prompt in the terminal.

`profiles.toml` never contains a password or key passphrase. For `auth = "password"`, only `save_password = true` is written; for `auth = "key"`, only `save_key_passphrase = true`.

## Host keys and `known_hosts`

Before opening any SSH profile, xtervo runs a host-key precheck in the background. It has a 10-second hard timeout and can be canceled; the UI does not freeze.

1. `ssh -G <profile args>` resolves the effective `StrictHostKeyChecking`, `HostName`, `Port`, `UserKnownHostsFile`, `HostKeyAlias`, and `ProxyJump`.
2. `ssh-keygen -F <key> -f <file>` looks up existing entries.
3. `ssh-keyscan -T 5 -p <port> <host>` fetches the server's current public keys. Output is capped at 1 MiB per stream, and the process group is killed on timeout or cancellation.

The verdict is one of:

| Verdict | Behavior |
| --- | --- |
| **Known** | Connect immediately. The verified connection configuration is cached in the process so later opens skip the keyscan. |
| **Unknown** | Show the `SHA256:` fingerprint. **Trust & connect** writes the key to the first user `known_hosts` file, then connects. **Once in terminal** disables askpass for this attempt and lets ssh confirm in the terminal. |
| **Changed** | Show the old and new fingerprints with a WARNING. Only **Update key & connect** removes the old entry with `ssh-keygen -R` and writes the new key. **Cancel** leaves the file untouched. |
| **Unverifiable** | ProxyJump, a failed/timed-out keyscan, or no common key type. xtervo does not auto-trust; it falls back to terminal confirmation. |

Lookup keys follow OpenSSH's own rules: the resolved `HostName` from `ssh -G` is used, non-default ports are written as `[host]:port`, and `HostKeyAlias` is matched verbatim (without adding a port). The cache key includes the host, port, jump, and relevant extra arguments, so editing the profile triggers a fresh check.

When `StrictHostKeyChecking` is explicitly `no`, `off`, or `accept-new`, the precheck defers to that setting and shows no in-app prompt; the user's ssh configuration takes over exactly as it does for the system client.

**Settings → Advanced → Known hosts** lists entries for saved SSH profiles and lets you delete one. Deleting an entry makes the next connection confirm the host key again. Only the profile's user-level `known_hosts` files are modified; system files under `/etc/ssh` are never written.

The native and plugin backends require a usable entry before they start. If the entry is unavailable, the native backend is not selected (system ssh may still prompt), while the plugin refuses to start. **Once in terminal** only helps the system client, because the other backends cannot answer ssh's interactive prompt.

## ProxyJump and port forwarding

`proxy_jump` is passed to `ssh -J` unchanged. OpenSSH accepts `user@host`, `user@host:port`, and comma-separated chains. xtervo rejects whitespace and a leading `-`, so the value cannot be mistaken for another option. Jump-host authentication uses the normal ssh mechanisms (agent, key, or terminal prompt); no jump password is stored.

Forwarding entries use OpenSSH syntax:

| Kind | Format | Example |
| --- | --- | --- |
| Local | `port:host:port` | `8080:localhost:80` |
| Remote | `[bind:]port:host:port` | `9000:localhost:3000` |
| Dynamic (SOCKS) | `[bind:]port` | `127.0.0.1:1080` |

Ports must be 1–65535. The native backend supports ProxyJump (including multi-hop), local/remote/dynamic forwarding, and agent forwarding, and forwards window-size changes. The system backend passes all of these to OpenSSH. The plugin POC reuses the profile's ssh arguments, but see its resize limitation below.

The **Port Forwards** panel shows the rules stored in a profile and, for a live native session, their state. New rules written from the panel apply to the profile on the next connection; local and dynamic rules can be started and stopped in the current native session.

## SFTP

SFTP is available for saved SSH profiles from the SFTP panel. It always uses the system `sftp` client, even when the SSH pane uses the native backend. A profile needs `host` and `user`, and the system `sftp` binary must be installed.

The client receives `-i key_path`, `-P port`, `-J proxy_jump`, any configured ciphers/MACs/KEX/`ProxyCommand`, then `extra_args`, then the default connection guards:

```text
-o ConnectTimeout=10
-o ServerAliveInterval=5
-o ServerAliveCountMax=2
```

`extra_args` appear first, and OpenSSH uses the first value it sees, so user-supplied timeouts can override the defaults. Metadata commands have an additional 10-second hard timeout. Transfers have no total duration limit, so large files are not killed arbitrarily; use **Cancel** and rely on keepalive to detect a dead link.

### Queue and transfers

- Uploads and downloads use a FIFO queue. When the current transfer finishes, the next one starts automatically.
- The panel shows the first queued items (with an overflow summary when there are more), a failed-item row when a retry is available, and the total queue count. Completed transfers leave the list.
- **Clear queue** removes only items that have not started; the active transfer is unaffected.
- **Cancel** works for transfers and metadata operations. It marks the request and kills the process group. A canceled transfer can leave a partial file; a canceled recursive delete can leave a partially removed tree.
- **Retry** is available after a failure. It requeues the failed request at the front, or retries immediately when idle. Error text comes from `sftp` stderr.
- Uploads and downloads whose target exists, or whose existence cannot be confirmed from the current listing, require an explicit overwrite confirmation before they enter the queue. Canceling the confirmation starts nothing and changes no file.
- Directory transfers use `sftp -R`. Deleting a directory requires an explicit recursive-delete confirmation; the operation runs with a 120-second budget, does not follow symlinks, and is reported as partial if it times out or is canceled. A single file uses `rm` and falls back to `rmdir`; it is not expanded recursively by guessing.
- **Chmod** accepts one to four octal digits (0–7777, with an optional `0o` prefix). It runs `chmod 0644 <remote>` style commands on the selected entry and reports failures inline.
- The remote listing prefers `ls -l` and falls back to `ls -1`; entries show mode, size, and modification time when the server provides them. Paths are quoted for the batch protocol, so spaces, quotes, and backslashes are preserved.

SFTP does not use Vault or askpass. Use `auth = "agent"` or `auth = "key"`, or an ssh-agent that can authenticate. Host-key trust follows the user's ssh configuration; open the SSH profile once to confirm a new host key before relying on SFTP, because the SFTP panel has no interactive host-key dialog.

## Session status and reconnect

When an SSH session ends, a banner reports **SSH session ended** with the exit code or signal when the backend can provide one. **Reconnect** relaunches ssh with the current profile in the same pane; it does not create a new tab.

If reconnecting fails because the profile was deleted, Vault is locked, or the launch fails, the banner shows the error or directs you to unlock Vault.

Automatic reconnect is opt-in per profile with `auto_reconnect = true` (default `false`). The global policy is `reconnect_max_attempts` (default `3`, `0` disables) and `reconnect_backoff_ms` (default `2000/5000/10000`). An automatic retry opens a new tab and leaves the disconnected pane visible; it gives up after the configured attempts. The Reconnect button remains the immediate, manual path.

## SSH backends: native, system, and plugin POC

### Native (default)

The native backend runs russh in the xtervo process. It parses `~/.ssh/config` and `known_hosts` itself, including hashed hosts, wildcards, `!` negation, and `@revoked` entries. It supports password authentication from Vault, unencrypted private keys, encrypted keys with a Vault-stored passphrase, ssh-agent, ProxyJump (including multi-hop), local/dynamic/remote forwarding, agent forwarding, and PTY window-size changes.

The native backend requires a `known_hosts` entry and refuses configurations it cannot represent. In those cases the profile uses the system client, so the connection still works rather than silently dropping options.

### System ssh

`transport = "system"` runs OpenSSH in a PTY. It covers the full ssh configuration: `extra_args`, `ProxyCommand`, `Match` blocks, `IdentityFile` from `ssh_config`, X11 forwarding, custom algorithms, forwarding, and ProxyJump. Password authentication can use Vault and askpass (see [Vault](vault.md)); if the password is not saved, ssh prompts in the terminal. Window resizes are forwarded to the remote PTY.

### Official SSH plugin POC

The plugin backend is a proof of concept, not the default. It keeps the SSH pane's byte transport behind `Backend::Plugin` while still using the system `ssh` binary. The plugin path does not add a separate SSH library; it runs the system client.

```toml
[[profiles]]
id = 1
name = "remote-plugin"
kind = "ssh"            # the legacy kind = "plugin-ssh" is normalized on load
transport = "plugin"
host = "ssh.example.test"
port = 22
user = "demo"
auth = "agent"          # agent or key; password is rejected
```

Data path:

```text
create_pane (no PTY)
  -> spawn_with_transport(..., Backend::Plugin)
  -> ssh -tt -o BatchMode=yes -o ConnectTimeout=... -o ServerAliveInterval=15 -o ServerAliveCountMax=3
       stdout -> framed [u32 LE length][payload] -> FrameReader -> pane vte/actor
       stdin  <- PaneTransport::write_all (terminal input)
       stderr -> separate thread -> red error/timeout text in the pane, then EOF
```

Frames are at most 256 KiB and the declared length must match the payload exactly; an oversize or partial frame is an error, never silently truncated. The connection state machine is `connecting → connected → closed / failed / timeout`, observable through `SshConnectionStatus`; connection state records host/user and a reason, never payload or passwords.

The default connect watchdog is 20 seconds and can be overridden with `XTERVO_PLUGIN_SSH_CONNECT_TIMEOUT_MS`. A timeout kills ssh and shows yellow error text in the pane. Closing the pane closes the transport and kills ssh; repeated close is idempotent.

From the command palette, use **SSH (plugin): <name>**, or run `ssh.connect:<id|name>` / `ssh.connect` (the first SSH profile). The plugin path requires a known host key before it starts; with `BatchMode=yes` an unconfirmed key cannot be answered interactively, so confirm it through the normal SSH profile first.

| Aspect | System ssh (`transport = "system"`) | Plugin POC (`transport = "plugin"`) |
| --- | --- | --- |
| Backend | OpenSSH child in a PTY | OpenSSH child wrapped as `PaneTransport` |
| Authentication | agent, key, password + Vault askpass | agent or key only; password is rejected |
| PTY resize | forwarded | remote PTY forced with `ssh -tt`; no winsize over the pipe, so resize is recorded but not forwarded |
| Host keys | precheck dialog, then ssh can prompt | precheck must succeed; `BatchMode=yes` fails visibly otherwise |
| Reconnect | banner Reconnect, optional auto-reconnect | no reconnect UI or Connection Manager; closing the pane disconnects |
| Status | supported compatibility/fallback path | POC (usable since 2026-11-17) |

The plugin is not a separate plugin process. The transport wraps the ssh child inside the xtervo process; the full `transport.open`/SCM_RIGHTS protocol and uninstall flow are still future work.

## Limits and status

- The host-key precheck is best-effort, not a second SSH implementation. Final authentication and host-key verification still belong to the SSH backend. `ProxyJump` and `ProxyCommand` cannot be prechecked by `ssh-keyscan`, and dynamic options such as `KnownHostsCommand` or `Match exec` can make the local lookup differ from ssh's own behavior. There is no silent-trust path.
- The **Known hosts** list covers saved SSH profiles only. It is not a general `known_hosts` editor, and entries for hosts you have not configured are not shown.
- The plugin POC does not support password auth, Vault askpass, window resizes, reconnect UI, or a separate plugin process.
- SFTP never receives Vault credentials and has no resume support. Canceled transfers may leave partial files, and recursive deletes may be partial after a timeout.
- Auto-reconnect is off by default; SFTP transfers have no automatic retry loop. Manual Retry and Reconnect remain available where the UI offers them.
- Native SSH still falls back to the system client for some ssh options. If you need a specific OpenSSH feature, set `transport = "system"` explicitly.

---

Chinese reference: [docs/SSH_SFTP.md](https://github.com/HeyTycho/xtervo/blob/main/docs/SSH_SFTP.md).
