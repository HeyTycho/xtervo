# Vault

Vault is xtervo's local encrypted credential store. It holds SSH passwords and, optionally, private-key passphrases. It is a single encrypted file protected by a master password; there is no OS keyring or Secret Service backend.

Vault is a security boundary for data at rest, not a sandbox. Read [Security model and limits](#security-model-and-limits) before relying on it.

## Storage location

| Item | Default |
| --- | --- |
| Vault file | `~/.config/xtervo/vault.enc` |
| With `XDG_CONFIG_HOME` | `$XDG_CONFIG_HOME/xtervo/vault.enc` |
| Override | `XTERVO_VAULT=/path/to/vault.enc` |
| Lock file | `<vault>.lock` (an adjacent `flock` file) |
| File mode | `0600` |

An empty `XTERVO_VAULT` falls back to the XDG path. The file is created on first use; the parent directory is created if needed. Per-process overrides make it possible to run separate instances with separate vaults.

The encrypted file can be copied or synced like any other file: it remains protected by the master password. However, an older copy does not rotate the master password, and restoring an old file also restores its old credentials.

## Master password and KDF

The master password must be at least 8 characters. It is never written to the vault file or application logs.

- Key derivation: **Argon2id**, 19 MiB memory (`m = 19456` KiB), `t = 2`, `p = 1`, with a random 16-byte salt and a 32-byte derived key.
- Encryption: **ChaCha20-Poly1305** AEAD with a random 12-byte nonce.
- The entire file header (magic, version, KDF parameters, salt, nonce) is authenticated as associated data. Editing the header makes decryption fail rather than silently changing parameters.

The on-disk format is version 1:

```text
XTERVO-VAULT 1
kdf=argon2id
m=19456
t=2
p=1
salt=<base64, 16 bytes>
nonce=<base64, 12 bytes>
ciphertext=<base64 ciphertext + 16-byte tag>
```

The plaintext is a JSON object mapping profile IDs to secrets. Key passphrases use a separate slot from login passwords, so saving one never overwrites the other.

The parser validates the magic, version, KDF name, parameter ranges, salt/nonce lengths, base64, and a 4 MiB file-size limit. Unknown or missing fields are rejected. KDF parameters are capped at `m` 8192–65536 KiB, `t ≤ 4`, and `p ≤ 4`; an out-of-range file is rejected before Argon2 runs.

Writes are atomic: a unique temporary file is created with `O_EXCL | O_NOFOLLOW` and mode `0600`, written, fsynced, renamed over the vault, and the parent directory is fsynced. An existing file with wider permissions is tightened to `0600` on load or write.

Concurrent access is serialized by `flock` on `<vault>.lock`. `set`, `remove`, `clear`, and `change_master` re-read the file under the lock, merge or validate, and then write. If another process changed the master password after your session unlocked, a write returns "modified by another process" and asks you to unlock again; it does not overwrite the newer file with stale data.

Creating a vault refuses to overwrite an existing file. A file that exists but cannot be unlocked is not replaced automatically.

## Unlock and lock

Open **Settings → Vault**. The page shows the current status (uninitialized, locked, or unlocked with the credential count), the vault path, a masked master-password field, and these actions:

- **Set / Change** — create the vault when uninitialized, or re-encrypt all credentials under a new master password when unlocked. Changing the password uses a new salt and key. When locked, unlock with the current password first; the action does not bypass it.
- **Unlock** — derive the key and decrypt the entries.
- **Lock** — discard the in-memory key and credentials.
- **Clear** — delete all credentials but keep the file and master password. It requires two clicks.

Argon2id does not run on the UI thread. Initialize, unlock, and change-master operations run in a background task; the Vault page stays responsive and shows a progress notice. Only one Vault task runs at a time: buttons are rejected while busy, and a task that ends with an error restores the page to a usable state. The UI thread only adopts the completed session.

A wrong master password and a corrupted file are indistinguishable at the AEAD layer. Both report "wrong master password or corrupt file"; a corrupt file is not overwritten. A failed unlock leaves the previous state unchanged.

Locking is manual or happens when the process exits. There is no idle auto-lock, no lock-on-screen-lock integration, and no master-password recovery. If you forget the master password, the vault cannot be decrypted.

## SSH password askpass

For an SSH profile with `auth = "password"` and **Save password to Vault** enabled:

1. Saving the profile stores the password with `vault.set(profile.id, password)`. `profiles.toml` stores only `save_password = true`.
2. Opening the profile when Vault is locked redirects to **Settings → Vault** with an unlock prompt. The password is read only after Vault is unlocked.
3. The host-key precheck runs first (see [SSH and SFTP](ssh-sftp.md#host-keys-and-known_hosts)). On a first connection, askpass is deliberately disabled so you confirm the host key and type the password once; after the host is known, subsequent connections can fill it automatically.
4. Before starting the system `ssh` client, xtervo forces password-only authentication:

   ```text
   -o PubkeyAuthentication=no
   -o PreferredAuthentications=password,keyboard-interactive
   -o KbdInteractiveAuthentication=yes
   ```

   `keyboard-interactive` is a fallback for PAM servers that only offer it; it comes after `password`. Without these options, ssh can succeed with an agent or key before ever showing a password prompt, so the Vault password and askpass would never be used.
5. xtervo creates a private temporary directory `$TMPDIR/xtervo-askpass-<pid>-<n>/` (mode `0700`) and an askpass script (mode `0700`). The script contains no password. The ssh child receives:

   ```text
   SSH_ASKPASS=<temporary script>
   SSH_ASKPASS_REQUIRE=force
   XTERVO_ASKPASS_SECRET=<password>
   ```

6. The script recognizes common localized password prompts (password, passwd, passwort, mot de passe, contraseña, senha, wachtwoord, пароль, パスワード, 비밀번호, 密码, 密碼, 口令) and prints the secret only for those. Verification-code, OTP, one-time-code, and other keyboard-interactive prompts get `no`; an unrecognized custom prompt also gets `no`. This is deliberate: OpenSSH can treat any askpass output as an answer to a host-key question, and returning the password under `SSH_ASKPASS_REQUIRE=force` can cause a fast retry loop or leak the password to a non-password prompt. With `no`, authentication fails quickly and readably instead.
7. Closing the pane removes the temporary directory and best-effort clears the environment copies.

`SSH_ASKPASS_REQUIRE` requires OpenSSH 8.4 or newer. Older clients ignore `force`; if `DISPLAY` is set they may still use askpass, otherwise ssh falls back to prompting in the terminal. Check with `ssh -V`.

If **Save password to Vault** is not enabled, no askpass is created. ssh prompts in the terminal and the password never enters Vault. The password-only options above still apply, so authentication does not silently fall back to a public key.

The application log records only metadata: that a password was retrieved for a profile (with its length), that askpass was enabled with `require=force`, which environment keys were injected (`secret_set=true`), and the known_hosts lookup result. It does not record the password.

For troubleshooting, setting `XTERVO_ASKPASS_DEBUG=/path/to/file` in the xtervo process makes the askpass script append one line per call with the prompt text and match branch. The file never contains the password, but the prompt usually contains the user and host names. Do not leave this enabled in normal use.

## SSH key passphrases

For `auth = "key"` with an encrypted private key, the profile can store the passphrase in Vault with **Save passphrase to Vault** (`save_key_passphrase = true`). It is stored in a separate slot from the login password and is used only by the native SSH backend.

When the key needs a passphrase, xtervo prompts before connecting, or after an authentication failure. Entering a new passphrase replaces the saved one; leaving the field blank keeps an existing Vault entry. On the system ssh path, ssh prompts in the terminal instead.

## Security model and limits

1. **askpass passes the password through the child environment.** The same user, and root, can read `/proc/<ssh-pid>/environ` and may recover the secret from process memory or a core dump. This avoids writing a plaintext file, but it is not the final design. A future version should pass the secret over an inherited file descriptor or a per-request Unix socket.
2. **Unlocked secrets live in process memory.** This version does not use `mlock` or complete zeroization. The background KDF thread overwrites its own master-password copy; dropping the unlocked session and `set`/`remove`/`clear` overwrite keys and replaced/deleted strings; decrypted AEAD buffers are cleared on drop. Allocator copies and crash dumps may still retain data. Do not keep Vault unlocked on an untrusted machine.
3. **No Secret Service/Keyring backend and no recovery.** The encrypted file is the only backend. A forgotten master password cannot be recovered.
4. **No idle auto-lock or unlock popup.** Lock manually with **Lock** or exit the process. Locked-vault actions redirect to the Vault settings page.
5. **Canceling Save password to Vault while locked leaves the old entry.** When unlocked, unchecking the option removes the entry immediately. While locked, the old ciphertext remains until you unlock and save or cancel again, or use **Clear**. The log notes this without printing the password.
6. **Wrong password and corruption are indistinguishable.** Both fail with the same message so the AEAD layer does not become an oracle. A corrupt file is not overwritten.
7. **Clear removes credentials, not the vault.** The file and master password remain, and the credential count becomes zero.
8. **Backups are protected by the same master password.** Copying `vault.enc` does not weaken it, but older copies do not rotate the master password. Changing the master password re-encrypts the current file with a new salt and key.
9. **Only `vault.enc` is forced to 0600.** The XDG config directory and `profiles.toml` permissions are not uniformly tightened and follow the process umask. `profiles.toml` contains hosts, users, and paths, so treat it as sensitive even though it contains no secrets.
10. **Host-key precheck is best-effort.** Final authentication and host-key verification remain the SSH backend's job. ProxyJump/ProxyCommand hosts cannot be scanned, keyscan can fail, and dynamic ssh options can make the local lookup differ from ssh's behavior. There is no silent-trust path; a failed precheck must be resolved in the terminal.
11. **Password profiles disable public-key authentication.** If the server accepts only keys, or you want to fall back to an agent, change the profile to `auth = "agent"` or `auth = "key"`; do not rely on an implicit fallback.
12. **askpass is intentionally narrow.** It answers only recognized password prompts and rejects OTP/verification prompts. If a server asks a custom question, authentication fails instead of feeding it the account password. Disable password saving and type the answer interactively in that case.
13. **The debug file is sensitive metadata.** `XTERVO_ASKPASS_DEBUG` records prompt text (typically user/host) but never the password. Enable it only while diagnosing.

The Vault page does not list individual credentials; it shows only the count. Use **Clear** to remove all entries, or change the profile's save options to remove one.

To check the file mode:

```bash
stat -c '%a' ~/.config/xtervo/vault.enc
```

The output should be `600`. The file is text with base64 fields; a plaintext search for a known password must return nothing.

---

Chinese reference: [docs/VAULT.md](https://github.com/HeyTycho/xtervo/blob/main/docs/VAULT.md).
