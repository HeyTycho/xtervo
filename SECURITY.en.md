# Security policy

[简体中文](SECURITY.md) | **English**

xtervo is a local terminal emulator that touches shells, SSH credentials, Vault, plugin
processes and the MCP control channel. Please report security issues **privately**; do
not post exploitation details or sensitive data in a public issue.

## Supported versions

xtervo is still 0.x and only accepts security fixes for the **latest release** (currently
0.6.x) and the latest commit on `main`. Please upgrade before attempting to reproduce.

## How to report

Prefer GitHub's private vulnerability reporting:

<https://github.com/HeyTycho/xtervo/security/advisories/new>

If that entry point is unavailable, contact the maintainer privately on GitHub
([@HeyTycho](https://github.com/HeyTycho)) and include:

- affected version / commit (`xtervo --version`);
- reproduction steps or a minimal PoC;
- impact (local privilege escalation, credential leak, denial of service, sandbox
  escape, etc.);
- whether it is already public.

Redact first: do not send real passwords, Vault files, SSH private keys, tokens or full
terminal logs. We will provide a fix plan after confirmation; no bug bounty is offered.

## Threat model

These boundaries are part of the design. Please check whether an issue is a known
limitation before reporting it:

- **Plugins are not a sandbox**: process plugins run external programs with the current
  user's privileges and can read/write any file that user can access; only install
  trusted plugins. Declarative host permissions are for display/gating only and are not
  OS-level isolation.
- **MCP control plane**: read-only by default; write operations must be enabled
  explicitly. The Unix socket uses a 0600 token and a private directory, but local
  processes running as the same UID can still access it.
- **Vault**: the master password is only used for local KDF and credentials are
  encrypted at rest; while unlocked, plaintext exists in process memory. Debuggers or
  memory dumps under the same UID are out of scope.
- **SSH**: known_hosts must already be confirmed by default; `transport = "system"`
  falls back to the system `ssh`, whose configuration and agent behavior are determined
  by OpenSSH.
- **X11**: any local X11 client can read window contents and the clipboard; this is a
  model problem that xtervo cannot fix alone.
- **Screenshot/recording/clipboard**: when the user explicitly triggers these, content is
  written to disk or the clipboard; paths and permissions are documented.

The following are generally **out of scope**: local attacks after obtaining code
execution as the same UID, malicious plugins themselves, cross-client X11 reads, and a
user pasting their own keys into the terminal.

## Hardening recommendations

- Install only from this repository's Releases or from a build you made yourself, and
  verify `SHA256SUMS`;
- Do not run xtervo as root;
- Install plugins only from trusted sources and periodically inspect
  `~/.config/xtervo/plugins/`;
- Keep MCP read-only unless write control is really needed;
- If credentials may have leaked, rotate SSH keys/tokens first, then review logs and
  screenshots.
