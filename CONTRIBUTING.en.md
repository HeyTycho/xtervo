# Contributing to xtervo

[简体中文](CONTRIBUTING.md) | **English**

Thank you for contributing code, tests, documentation or feedback. xtervo is still a
0.x Linux terminal emulator, so **reproducible, verifiable, clearly scoped** changes are
preferred over large all-at-once features.

## Read these first

- [README.en.md](README.en.md): install, build, shortcuts and known limitations
- [AGENTS.md](AGENTS.md): development discipline (tests first, layered debugging, memory gates)
- [docs/VERIFICATION.md](docs/VERIFICATION.md): full verification checklist and evidence requirements
- [docs/RELEASING.md](docs/RELEASING.md): versions, tags and the release process
- [docs/SOAK.md](docs/SOAK.md): 2m/15m/30m stability and memory gates
- [docs/PLUGIN_SYSTEM.md](docs/PLUGIN_SYSTEM.md): plugin capabilities and security boundary

> Do **not** open a public issue for security vulnerabilities; report them privately
> as described in [SECURITY.en.md](SECURITY.en.md).
> Community behavior is covered by [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).

## Feedback and feature requests

- **Bugs**: use the [bug report template](.github/ISSUE_TEMPLATE/bug.yml) and include
  `xtervo --version` / `xtervo --diagnose`, your system and desktop environment, and
  minimal reproduction steps.
- **Feature requests**: use the [feature request template](.github/ISSUE_TEMPLATE/feature.yml);
  focus on the **problem, use case and expected behavior**.
- **Usage questions**: check [README.en.md](README.en.md) and `docs/` first, then open an issue.

Search existing issues before filing. Logs, screenshots and config can contain private
data — redact them first. **Never paste** master passwords, Vault files, SSH private
keys, tokens, API keys, full terminal scrollback or unredacted logs.

## Development environment

So far xtervo has been tested on real hardware on **Ubuntu 22.04 / 24.04** only. Reports
from other distributions/desktops are welcome, but fixing them depends on reproducing
the issue.

### Build dependencies

```bash
sudo apt-get update
sudo apt-get install -y \
  build-essential pkg-config cmake ninja-build python3 \
  ca-certificates curl libudev-dev \
  libxkbcommon-dev libx11-dev libxcursor-dev libxrandr-dev libxi-dev \
  libgl1-mesa-dev libegl1-mesa-dev libfontconfig1-dev libfreetype6-dev
```

Running e2e / soak additionally needs:

```bash
sudo apt-get install -y xvfb openbox x11-utils ffmpeg openssh-client \
  fonts-dejavu-core fonts-noto-cjk
```

Rust uses stable; the minimum version is in the root `Cargo.toml` `rust-version`
(currently 1.92):

```bash
rustup toolchain install stable --component rustfmt
rustup default stable
```

Wayland currently runs through XWayland. **IME acceptance requires real Wayland +
fcitx5/RIME evidence**; XIM under X11/Xvfb/Xephyr does not produce text-input preedit
events and cannot prove an IME fix.

### Build

```bash
# In a sandbox/container where ~/.cargo is read-only, put the cargo cache in the repo
# (already ignored by .gitignore).
export CARGO_HOME=$PWD/.cargo

cargo check --workspace
cargo build --release -p xtervo-app --bin xtervo
./target/release/xtervo --cols 120 --rows 32
```

Without a GPU / headless, force the software backend with
`SLINT_BACKEND=winit-software`. Rendering conclusions must state the actual backend;
do not present llvmpipe numbers as GPU results.

## Worktree and branch rules

Do not develop or commit directly on `main`. Use a dedicated worktree + branch for each
change:

```bash
git worktree add .worktrees/<topic> -b <type>/<topic> main
cd .worktrees/<topic>
```

In a fresh clone you can exclude the worktree directory locally so it does not show up
as untracked in the main checkout:

```bash
echo '.worktrees/' >> .git/info/exclude
```

Conventions:

- One worktree / branch does one thing; keep refactors and behavior changes separate.
- The worktree must be clean before a PR: `git status --short` shows nothing.
- Do not commit `target/`, `dist/`, `.logs/`, `.tmp/`, screenshot caches, or other
  build/evidence artifacts.
- Do not edit third-party code under `vendor/` directly; if unavoidable, explain why,
  the diff against upstream, and the license impact.
- Do not rewrite shared branch history; tag rules are in [docs/RELEASING.md](docs/RELEASING.md).
- Automated agents must also follow the time windows and gates in [AGENTS.md](AGENTS.md).

## Testing

```bash
cargo fmt --all -- --check
cargo test -p xtervo-core
cargo test -p xtervo-app --bin xtervo
```

CI runs `cargo fmt`, the documentation link/count checks and the two test suites on
Ubuntu 22.04 / 24.04. **CI does not run e2e / soak**, so changes touching UI, input,
rendering, connections or the plugin host must include their own evidence.

### Every change needs a failing test first

- For a bug fix, first write a failing test that reproduces the issue, paste the failure
  output before the fix, then make it pass.
- Terminal parsing and image/filter-layer changes also need byte-level or pixel-level
  differential tests.
- Keyboard/IME changes must at least cover: stale preedit does not swallow keys,
  Num/Caps Lock do not trigger escape sequences, and shortcuts match platform defaults
  item by item.
- If automation is genuinely impossible, explain why in the PR and provide minimal
  reproduction steps, logs/screenshots and manual verification records; maintainers
  decide whether to accept it.

### Debug by layer; never mask the root cause with full redraws

Find the first divergence along `bytes -> filter -> Term -> Frame -> framebuffer -> Slint/window`.
Do not mask the root cause with default full-screen redraws, a new buffer every frame, or
`malloc_trim`; those may only exist behind an explicit debug switch. If two consecutive
fixes do not change the symptom, stop guessing and switch to instrumentation or bisection.

### e2e and soak expectations

| Change scope | Minimum verification |
|---|---|
| Docs / CI / packaging | Docs checks + `cargo fmt`; run related tests when the tree builds |
| Ordinary bug fix | Failing test + related unit tests |
| UI / rendering / input | Failing test + related e2e + screenshot or diff evidence |
| Terminal core / render loop / plugin host / large feature | Unit tests + related e2e + **2m soak** |
| Release candidate | Full tests + **30m soak** (at least 15m when touching core/rendering/loops) |

```bash
# 2m script gate; use 15m when touching the terminal/rendering/loop/plugin host
DURATION=2m ./scripts/soak.sh
DURATION=15m ./scripts/soak.sh

# Example: pixel-level split repaint e2e
python3 scripts/e2e_split_repaint.py
```

Soak reports must record RSS/PSS, threads/FDs, live heap and scenario coverage; artifacts
live in `.logs/soak/<stamp>/` (`report.md`, `summary.json`). **Do not change soak
thresholds**, and do not replace lifetime fixes with `malloc_trim`.

## Documentation

When changing behavior, configuration, shortcuts or the CLI, update the corresponding
documentation. Before committing run:

```bash
python3 scripts/check_doc_links.py
python3 scripts/check_docs_counts.py
```

The CI docs job runs both. `check_docs_counts.py` verifies that the statistics in
`docs/CLOSURE.md` and `docs/FEATURE_MATRIX.md` agree; when they drift, update the real
source (matrix/closure) and recompute — do not patch a single number. Documentation is
currently mostly Chinese, uses relative links and copy-pasteable commands, and cites
external sources with links. English translations are welcome.

## Commits and PRs

- Use `type(scope): summary`, e.g. `fix(input): do not swallow stale preedit`,
  `docs(soak): document the 15m gate`; the body explains motivation, risk and verification.
- Use the repository PR template, link the issue, and keep PRs small and focused; discuss
  large changes in a draft/issue first.
- CI must pass before merge; reviewers may ask for failing tests, e2e evidence or a soak report.
- Inspect the staged diff before committing (`git diff --cached`) and make sure no secrets,
  tokens, private paths or unrelated artifacts are included.
- Report security issues privately via [SECURITY.en.md](SECURITY.en.md), not in PRs/issues.

## License

xtervo is licensed under **GPL-3.0-only** (see [LICENSE](LICENSE)). By submitting a PR you
confirm that you have the right to contribute the content and agree to license it under
the same terms. Before adding third-party code, fonts, icons or test assets, verify
license compatibility and update `THIRD_PARTY_NOTICES.md`.
