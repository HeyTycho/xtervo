# Installation

xtervo is distributed as prebuilt Linux artifacts and can also be built from source.
Only Ubuntu 22.04 and 24.04 have been tested so far; other distributions and desktop
environments are unverified. For a source build, use Rust 1.92+ with Slint 1.18.

## Release artifacts

Prebuilt artifacts are published on [GitHub Releases](https://github.com/HeyTycho/xtervo/releases):

| Artifact | Purpose |
|---|---|
| `xtervo-<version>-linux-x86_64.tar.gz` | Extract and run `install.sh`; installs to `~/.local` by default, `--prefix` overrides |
| `xtervo_<version>-1_amd64.deb` | Debian / Ubuntu package; `apt install ./...deb` resolves dependencies |
| `SHA256SUMS` | SHA-256 checksums for the downloaded artifacts |

Artifacts are named after the build architecture: x86_64 maps to `amd64` in `.deb`,
aarch64 maps to `arm64`.

## Verify the download

Run this in the directory that contains the downloaded files. Only files present in
that directory are checked:

```bash
sha256sum -c --ignore-missing SHA256SUMS
```

After installation, with `$PREFIX/bin` on your `PATH`, `xtervo --version` prints the
version, commit and build time.

## Install from a release

### tar.gz (default prefix `~/.local`)

```bash
tar xzf xtervo-<version>-linux-x86_64.tar.gz
./xtervo-<version>-linux-x86_64/install.sh

# custom prefix
./xtervo-<version>-linux-x86_64/install.sh --prefix /opt/xtervo

# or through the environment
PREFIX=/opt/xtervo ./xtervo-<version>-linux-x86_64/install.sh
```

The default prefix is `$HOME/.local` and does not require root. For a system-wide
install, use a writable prefix such as `/usr` with the appropriate privileges.

Make sure `$PREFIX/bin` is on your `PATH`:

```bash
export PATH="$HOME/.local/bin:$PATH"
```

The installed desktop entry uses an absolute `Exec` path, so launching xtervo from the
application menu does not depend on the login `PATH`.

Files installed under `$PREFIX`:

| Path | Purpose |
|---|---|
| `$PREFIX/bin/xtervo` | Current binary |
| `$PREFIX/lib/xtervo/previous/xtervo` | Previous binary used by `--rollback` |
| `$PREFIX/lib/xtervo/versions/<version>-<commit>/xtervo` | Archived versions |
| `$PREFIX/lib/xtervo/manifest` | File manifest used by `--uninstall` |
| `$PREFIX/lib/xtervo/install.sh` | Installed lifecycle script |
| `$PREFIX/share/applications/xtervo.desktop` | Desktop entry |
| `$PREFIX/share/icons/hicolor/` | Application icons |
| `$PREFIX/share/doc/xtervo/` | License, notices and build information |

### .deb

```bash
sudo apt install ./xtervo_<version>-1_amd64.deb
```

The package installs `/usr/bin/xtervo`, the desktop entry, icons and documentation.
`Depends` is generated from the binary's ELF requirements plus the X11/XKB runtime
libraries; `libgl1` and `fonts-dejavu-core` are recommended, not required. Without a
GPU, xtervo falls back to software rendering.

## Upgrade, roll back, uninstall

### Upgrade a tar.gz install

Run the new tarball's installer against the same prefix:

```bash
./xtervo-<new-version>-linux-x86_64/install.sh --prefix ~/.local
```

The replaced binary is kept as the previous version, and the current version is
archived. The binary is replaced atomically, so a running xtervo keeps its old inode
and is not corrupted by the upgrade. Configuration, profiles, themes and logs are not
touched.

### Roll back

```bash
~/.local/lib/xtervo/install.sh --rollback --prefix ~/.local
```

This swaps `previous/xtervo` back to `bin/xtervo` and stores the replaced version as
the new previous one, so running `--rollback` again switches back. If there is no
previous version, the script exits non-zero and prints the missing path. `.deb`
installs do not create a previous-version slot; install the earlier `.deb` to downgrade.

### Uninstall

```bash
# installed lifecycle script
~/.local/lib/xtervo/install.sh --uninstall --prefix ~/.local

# or from an extracted tarball
./xtervo-<version>-linux-x86_64/install.sh --uninstall --prefix ~/.local

# .deb package
sudo apt remove xtervo
```

Uninstall removes only the files listed in the manifest and the
`$PREFIX/lib/xtervo` state directory. It does not recurse into unrelated files in the
prefix, and it preserves `~/.config/xtervo` (configuration, profiles, themes and
migration backups) and the logs. Repeated uninstalls are idempotent.

## Build from source

Ubuntu / Debian build dependencies:

```bash
sudo apt install build-essential pkg-config cmake ninja-build ca-certificates curl libudev-dev \
  libxkbcommon-dev libx11-dev libxcursor-dev libxrandr-dev libxi-dev \
  libgl1-mesa-dev libegl1-mesa-dev libfontconfig1-dev libfreetype6-dev
```

```bash
git clone https://github.com/HeyTycho/xtervo.git
cd xtervo
cargo build --release -p xtervo-app
./target/release/xtervo --cols 120 --rows 32
```

Runtime: Linux (X11; Wayland through XWayland), Rust 1.92+, Slint 1.18.

By default Cargo uses `~/.cargo`. If that directory is read-only (sandbox/CI), point it
inside the repository instead:

```bash
export CARGO_HOME=$PWD/.cargo
```

## Packaging and test scripts

```bash
./scripts/package.sh                  # → dist/ (binary + tar.gz + .deb + SHA256)
./scripts/install.sh                  # install to $PREFIX (default ~/.local)
./scripts/install.sh --rollback       # roll back to the previous version
./scripts/test_packaging.sh           # package + clean-prefix install + upgrade/rollback/uninstall
```

- `scripts/package.sh` builds a release binary, strips a copy, and writes the binary,
  `tar.gz`, `.deb` and `SHA256SUMS` to `dist/`. It refuses a dirty worktree by default
  and is reproducible for the same commit and toolchain; see the
  [release process](https://github.com/HeyTycho/xtervo/blob/main/docs/RELEASING.md) and
  [packaging notes](https://github.com/HeyTycho/xtervo/blob/main/packaging/README.md).
- `scripts/install.sh` installs from the repository (building the release binary when
  needed) or from an extracted tarball. It supports `--prefix`, `--no-strip`,
  `--rollback` and `--uninstall`.
- `scripts/test_packaging.sh` packages the application, installs into a clean temporary
  prefix, and verifies upgrade, rollback, uninstall, desktop entries, checksums and
  configuration preservation. Set `KEEP_TEST=1` to keep the temporary directory.

## Runtime, GPU fallback, first run

Start xtervo with no arguments to open a window with your default shell:

```bash
xtervo
```

The initial window is 120 columns by 32 rows by default; `--cols` and `--rows` override
it, and `--restore` restores the previous layout.

Rendering uses the GPU (Skia) when `/dev/dri` is available. Without DRM, or if no GPU
API is confirmed within 1s, xtervo falls back to `winit-software` automatically.
To force a backend while diagnosing rendering:

```bash
SLINT_BACKEND=winit-software xtervo   # software
SLINT_BACKEND=winit-skia xtervo       # GPU/Skia path
```

Configuration and state paths:

| Data | Default path |
|---|---|
| Configuration | `~/.config/xtervo/config.toml` |
| Profiles | `~/.config/xtervo/profiles.toml` |
| Workspaces | `~/.config/xtervo/workspaces.toml` |
| Logs and state | `~/.local/state/xtervo/` (logs in `~/.local/state/xtervo/logs/`) |

Configuration files are created as needed when settings change. Each launch writes a
new log file under `logs/`, and the most recent 8 are kept. `XTERVO_CONFIG`,
`XTERVO_PROFILES` and `XTERVO_LOG_DIR` override the corresponding paths.

For bug reports, `xtervo --diagnose` prints the version, environment, config and log
paths, and recent log lines. `xtervo diag env|snapshot|trace` provides offline JSON
diagnostics.

## Next steps

- [Keyboard shortcuts](shortcuts.md)
- [Known limitations](known-limitations.md)
- [FAQ](faq.md)
