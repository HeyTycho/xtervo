# 打包与安装

## 产物策略

| 路径 | 用途 | 符号 | 体积（本机实测） |
|---|---|---|---|
| `target/release/tervo` | 开发、`perf` / flamegraph、`scripts/phase0.sh` 等性能脚本 | 保留（release profile 带 `debug = 1`） | 约 168.4 MiB（176.6 MB） |
| `dist/tervo`（`scripts/package.sh`） | 对外发布 / 分发 | 已 strip | 约 30.5 MiB（实测 32,001,232 B，-81.9%） |
| `$PREFIX/bin/tervo`（`scripts/install.sh`） | 本机安装 | 默认已 strip，可用 `--no-strip` 保留 | 同 dist |

体积随代码与工具链变化，表中为本机实测值。

**不要就地 strip `target/release/tervo`**：`docs/phase0-report.md` 与
`scripts/phase0.sh` / `run_gpu_validation.sh` 的 perf、flamegraph 流程依赖这些符号。
打包脚本只处理复制出来的副本。

## 发布打包：`scripts/package.sh`

流程：`cargo build --release` → 复制到输出目录 → `strip --strip-all` → `--help` 冒烟 → 体积门禁。

```bash
export CARGO_HOME=$PWD/.cargo        # 沙箱内 ~/.cargo 只读时

./scripts/package.sh                          # 默认输出 dist/tervo
./scripts/package.sh /tmp/dist                # 位置参数指定输出目录
TERVO_DIST=/tmp/dist ./scripts/package.sh  # 环境变量指定输出目录
SKIP_BUILD=1 ./scripts/package.sh             # 复用已有 release 二进制
MAX_MB=30 ./scripts/package.sh                # 自定义体积门禁（默认 40）
```

输出目录相对仓库根解析。产物先写 `$OUT.stripped`，成功后才原子替换 `$OUT`；
失败不会留下半成品。若输出目录被设成 `target/release`，脚本直接拒绝执行，
避免误 strip 开发二进制。

环境变量：

| 变量 | 默认 | 说明 |
|---|---|---|
| `TERVO_DIST` | `dist` | 输出目录；位置参数优先 |
| `MAX_MB` | `40` | 产物体积上限（MiB），超过则退出非 0 |
| `SKIP_BUILD` | `0` | `1` 表示不构建，直接打包现有 release 二进制 |
| `SKIP_SMOKE` | `0` | `1` 表示跳过 strip 后的 `--help` 冒烟（交叉编译时用） |
| `STRIP` | 自动探测 `strip` / `llvm-strip` | strip 命令 |
| `STRIP_FLAGS` | `--strip-all` | strip 参数 |

实测输出（本机）：

```text
== 体积 ==
  开发二进制  target/release/tervo（168.4 MB，保留符号）
  发布产物    dist/tervo（30.5 MB，stripped）
  缩减        81.9%
  file        ELF 64-bit LSB pie executable, x86-64, ... stripped
  门禁        ≤ 40 MB ✓
```

## 安装：`scripts/install.sh`

默认行为不变：装到 `$PREFIX`（默认 `~/.local`），附带 `.desktop`、图标，
仅在 `target/release/tervo` 不存在时构建。区别是安装副本现在会先在
`mktemp -d` 的临时目录里 strip，再 `install` 到目标路径；开发二进制不受影响。

```bash
./scripts/install.sh                          # ~/.local，strip 后安装
PREFIX=/tmp/prefix ./scripts/install.sh       # 自定义 prefix（不写系统目录）
./scripts/install.sh --prefix /tmp/prefix     # 同上，命令行参数
./scripts/install.sh --no-strip               # 保留符号（调试用）
TERVO_STRIP=0 ./scripts/install.sh        # 同上，环境变量
PREFIX=/usr/local sudo -E ./scripts/install.sh
```

- `strip` 探测顺序：`$STRIP` → `strip` → `llvm-strip`。
- 找不到 `strip` 或 strip 失败时，打印警告并回退安装未 strip 的副本，
  不阻断安装；发布打包走 `package.sh` 时则是硬失败。
- `dist/` 与 `*.stripped` 已在 `.gitignore` 中。

## 验证

```bash
export CARGO_HOME=$PWD/.cargo

# 打包（输出到仓库内默认目录）
bash scripts/package.sh
ls -l dist/
file dist/tervo
./dist/tervo --help

# 输出目录覆盖（/tmp 在沙箱内每次调用重建，测完即弃）
TERVO_DIST=/tmp/dist bash scripts/package.sh
ls -l /tmp/dist
/tmp/dist/tervo --help

# 符号检查：发布产物 0，开发二进制 >0
readelf -S dist/tervo | grep -c '\.debug_info'
readelf -S target/release/tervo | grep -c '\.debug_info'
```

## 兼容性与风险

- `install.sh` 的安装路径、桌面项、图标、按需构建行为都保持原样；唯一默认变化是
  安装的二进制被 strip。需要调试符号时用 `--no-strip` / `TERVO_STRIP=0`，
  或直接运行 `target/release/tervo`。
- `install.sh` 现在会解析参数：`--prefix` / `--no-strip` / `-h` 之外的自定义参数
  会以退出码 2 报错（旧版会静默忽略），脚本化调用时不要传多余参数。
- strip 后没有行号与函数符号，`RUST_BACKTRACE=1` 的栈不可读；不要用发布产物做性能分析。
- `strip` 来自 binutils（或 LLVM `llvm-strip`）；交叉编译时用 `STRIP=<triple>-strip`
  指定目标平台的 strip。
- 打包只做 strip，不做自包含打包：产物仍动态链接系统库（X11、字体等），
  不能直接拷到无对应运行库的机器。
- 之前已经安装过的旧二进制不会自动变小，重新执行 `install.sh` 才会替换。
