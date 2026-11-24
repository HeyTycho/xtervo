# 参与 xtervo

感谢你愿意贡献代码、测试、文档或反馈。xtervo 仍是 0.x 阶段的 Linux 终端模拟器，
优先接受**可复现、可验证、范围清晰**的改动，而不是一次合并很大的功能。

## 先读这些

- [README.md](README.md)：安装、构建、快捷键与已知限制
- [AGENTS.md](AGENTS.md)：项目开发纪律（测试优先、分层排查、内存门禁）
- [docs/VERIFICATION.md](docs/VERIFICATION.md)：完整验证清单与证据要求
- [docs/RELEASING.md](docs/RELEASING.md)：版本、tag 与发布流程
- [docs/SOAK.md](docs/SOAK.md)：2m/15m/30m 稳定性与内存门禁
- [docs/PLUGIN_SYSTEM.md](docs/PLUGIN_SYSTEM.md)：插件能力与安全边界

> 安全漏洞**不要开公开 issue**，请按 [SECURITY.md](SECURITY.md) 私下报告。
> 行为规范见 [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md)。

## 反馈与许愿

- **Bug**：使用 [Bug 报告模板](.github/ISSUE_TEMPLATE/bug.yml)，附 `xtervo --version` /
  `xtervo --diagnose`、系统与桌面环境、最小复现步骤。
- **功能许愿**：使用 [功能许愿模板](.github/ISSUE_TEMPLATE/feature.yml)，重点写清
  **问题、使用场景、期望行为**。
- **使用问题**：先查 [README.md](README.md) 与 `docs/`，仍无法解决再开 issue。

提交前请先搜索现有 issue。日志、截图、配置都可能含私有信息，请先脱敏；**不要粘贴**
主密码、Vault 文件、SSH 私钥、token、API Key、完整终端回滚或未脱敏日志。

## 开发环境

目前只在 **Ubuntu 22.04 / 24.04** 做过实机验证。其他发行版/桌面环境的报告同样欢迎，
但修复排期取决于能否复现。

### 构建依赖

```bash
sudo apt-get update
sudo apt-get install -y \
  build-essential pkg-config cmake ninja-build python3 \
  libxkbcommon-dev libx11-dev libxcursor-dev libxrandr-dev libxi-dev \
  libgl1-mesa-dev libegl1-mesa-dev libfontconfig1-dev libfreetype6-dev
```

跑 e2e / soak 还需要：

```bash
sudo apt-get install -y xvfb openbox x11-utils ffmpeg
```

Rust 使用 stable，最低版本见根 `Cargo.toml` 的 `rust-version`（当前 1.85）：

```bash
rustup toolchain install stable --component rustfmt
rustup default stable
```

Wayland 目前通过 XWayland 运行。**IME 验收必须有真实 Wayland + fcitx5/RIME 的证据**；
X11/Xvfb/Xephyr 的 XIM 不产生 text-input preedit，不能当作输入法修好的证明。

### 构建

```bash
# 在 ~/.cargo 只读的沙箱/容器里，可以把 cargo 缓存放到仓库内（已被 .gitignore 忽略）
export CARGO_HOME=$PWD/.cargo

cargo check --workspace
cargo build --release -p xtervo-app --bin xtervo
./target/release/xtervo --cols 120 --rows 32
```

无 GPU / headless 时可用 `SLINT_BACKEND=winit-software` 强制软件后端；渲染相关结论必须
注明实际 backend，不能把 llvmpipe 数据写成 GPU 结论。

## Worktree 与分支规则

不要直接在 `main` 上开发或提交。每个改动使用独立 worktree + 分支：

```bash
git worktree add .worktrees/<topic> -b <type>/<topic> main
cd .worktrees/<topic>
```

新 clone 可把 worktree 目录加入本地排除，避免主工作区出现未跟踪目录：

```bash
echo '.worktrees/' >> .git/info/exclude
```

约定：

- 一个 worktree / 分支只做一件事；重构与行为变化尽量分开。
- PR 前 worktree 必须干净：`git status --short` 无未提交改动。
- 不提交 `target/`、`dist/`、`.logs/`、`.tmp/`、截图缓存等构建/证据产物。
- 不直接改 `vendor/` 里的第三方代码；确需修改时单独说明原因、上游 diff 与许可证影响。
- 不改写共享分支历史；tag 规则见 [docs/RELEASING.md](docs/RELEASING.md)。
- 自动化 agent 还必须遵守 [AGENTS.md](AGENTS.md) 的时间窗口与门禁规则。

## 测试

```bash
cargo fmt --all -- --check
cargo test -p xtervo-core
cargo test -p xtervo-app --bin xtervo
```

CI 在 Ubuntu 22.04 / 24.04 上跑 `cargo fmt`、文档链接/计数检查和上述两组测试；
**CI 不跑 e2e / soak**，所以涉及 UI、输入、渲染、连接或插件宿主的改动需要自己附证据。

### 改动必须先有失败测试

- Bug 修复先写能复现问题的失败测试，贴出修复前的失败输出，再让测试通过。
- 终端解析、图片/过滤层改动要同时有逐字节或逐像素差分测试。
- 键盘/IME 改动至少覆盖：stale preedit 不吞键、Num/Caps Lock 不触发转义、
  快捷键与平台默认逐项对齐。
- 如果确实无法自动化，PR 中说明原因，并提供最小复现步骤、日志/截图和手动验证记录，
  由维护者判断是否接受。

### 分层排查，不用整屏重绘掩盖根因

按 `bytes -> filter -> Term -> Frame -> framebuffer -> Slint/窗口` 找第一处分歧。
禁止用默认整屏重绘、每帧新 buffer、`malloc_trim` 之类手段掩盖根因；这些只能作为显式
debug 开关。连续两个修复不改变症状时，停止猜测，转插桩或二分。

### e2e 与 soak 预期

| 改动范围 | 最低验证 |
|---|---|
| 文档 / CI / 打包 | 文档检查 + `cargo fmt`；能构建时跑相关测试 |
| 普通 bugfix | 失败测试 + 相关单测 |
| UI / 渲染 / 输入 | 失败测试 + 相关 e2e + 截图或差分证据 |
| 终端核心 / 渲染循环 / 插件宿主 / 大功能 | 单测 + 对应 e2e + **2m soak** |
| 发布候选 | 完整测试 + **30m soak**（触及核心/渲染/循环时至少 15m） |

```bash
# 2m 脚本门禁；触及终端/渲染/循环/插件宿主时用 15m
DURATION=2m ./scripts/soak.sh
DURATION=15m ./scripts/soak.sh

# 示例：像素级分屏重绘 e2e
python3 scripts/e2e_split_repaint.py
```

soak 报告要记录 RSS/PSS、线程/FD、live heap 与场景覆盖，产物在
`.logs/soak/<stamp>/`（`report.md`、`summary.json`）。**不要修改 soak 阈值**，也不要用
`malloc_trim` 替代生命周期修复。

## 文档

改代码行为、配置、快捷键或 CLI 时请同步更新对应文档。提交前跑：

```bash
python3 scripts/check_doc_links.py
python3 scripts/check_docs_counts.py
```

CI 的 docs job 会跑这两条。`check_docs_counts.py` 校验 `docs/CLOSURE.md` 与
`docs/FEATURE_MATRIX.md` 的统计一致；发现漂移时先更新矩阵/闭环的真实来源，再重新计算，
不要只改一个数字。文档使用中文、相对链接和可复制命令，引用外部资料时给出链接。

## 提交与 PR

- 提交信息建议 `type(scope): summary`，例如 `fix(input): 不吞 stale preedit`、
  `docs(soak): 补充 15m 门禁`；正文说明动机、风险和验证方式。
- 使用仓库的 PR 模板，关联 issue，尽量小而聚焦；大改动先开 draft/issue 讨论。
- PR 合并前 CI 必须通过；reviewer 可能要求补失败测试、e2e 证据或 soak 报告。
- 提交前检查暂存区：`git diff --cached`，确认没有密钥、token、私有路径或无关产物。
- 安全漏洞按 [SECURITY.md](SECURITY.md) 私下报告，不要放进 PR/issue 正文。

## 许可证

xtervo 使用 **GPL-3.0-only**（见 [LICENSE](LICENSE)）。提交 PR 即表示你有权提交这些内容，
并同意以同一许可证授权你的贡献。引入第三方代码、字体、图标或测试素材前，请确认许可证
兼容并更新 `THIRD_PARTY_NOTICES.md`。
